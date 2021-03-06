use boolector::BVSolution;
use boolector::option::{BtorOption, ModelGen};
use llvm_ir::*;
use log::{debug, info};
use reduce::Reduce;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::iter::FromIterator;
use std::sync::{Arc, RwLock};

use crate::alloc::Alloc;
use crate::backend::*;
use crate::config::Config;
use crate::error::*;
use crate::extend::*;
use crate::global_allocations::*;
use crate::layout::*;
use crate::project::Project;
use crate::solver_utils::{self, PossibleSolutions};
use crate::varmap::{VarMap, RestoreInfo};
use crate::watchpoints::{Watchpoint, Watchpoints};

/// A `State` describes the full program state at a given moment during symbolic
/// execution.
#[derive(Clone)]
pub struct State<'p, B: Backend> {
    /// Reference to the solver instance being used
    pub solver: B::SolverRef,
    /// The configuration being used
    pub config: Config<'p, B>,
    /// Indicates the instruction which is currently being executed
    pub cur_loc: Location<'p>,

    // Private members
    varmap: VarMap<B::BV>,
    mem: RefCell<B::Memory>,
    alloc: Alloc,
    global_allocations: GlobalAllocations<'p, B>,
    /// This tracks the call stack of the symbolic execution.
    /// The first entry is the top-level caller, while the last entry is the
    /// caller of the current function.
    ///
    /// We won't have a `StackFrame` for the current function here, only each of
    /// its callers. For instance, while we are executing the top-level function,
    /// this stack will be empty.
    stack: Vec<StackFrame<'p, B::BV>>,
    /// These backtrack points are places where execution can be resumed later
    /// (efficiently, thanks to the incremental solving capabilities of Boolector).
    backtrack_points: Vec<BacktrackPoint<'p, B>>,
    /// Log of the basic blocks which have been executed to get to this point
    path: Vec<PathEntry>,
    /// Memory watchpoints (segments of memory to log reads/writes of).
    ///
    /// These will persist across backtracking - i.e., backtracking will not
    /// restore watchpoints to what they were at the backtrack point;
    /// backtracking will not touch the set of mem_watchpoints or their
    /// enabled statuses.
    mem_watchpoints: Watchpoints,
}

/// Describes one segment of a path through the LLVM IR.
///
/// Uses a format suitable for recording - for instance, uses function names
/// rather than references to `Function` objects. For a richer representation of
/// a code location, see [`Location`](struct.Location.html).
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Hash)]
pub struct PathEntry {
    pub modname: String,
    pub funcname: String,
    pub bbname: Name,
    /// Index of the instruction within the basic block at which execution started. E.g., 0 means we started at the beginning of the basic block.
    pub instr: usize,
}

/// Format a basic block `Name` into a concise representation for printing
pub fn pretty_bb_name(name: &Name) -> String {
    match name {
        Name::Name(ref s) => format!("{:?}", s),
        Name::Number(n) => format!("%{}", n),
    }
}

impl fmt::Debug for PathEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{{}: {} {}, instr {}}}", self.modname, self.funcname, pretty_bb_name(&self.bbname), self.instr)
    }
}

/// Fully describes a code location within the LLVM IR.
#[derive(Clone)]
pub struct Location<'p> {
    pub module: &'p Module,
    pub func: &'p Function,
    pub bbname: Name,
    /// Index of the instruction within the basic block. E.g., 0 means the first instruction of the basic block.
    pub instr: usize,
}

/// Implementation of `PartialEq` assumes that module and function names are unique
impl<'p> PartialEq for Location<'p> {
    fn eq(&self, other: &Self) -> bool {
        self.module.name == other.module.name
            && self.func.name == other.func.name
            && self.bbname == other.bbname
            && self.instr == other.instr
    }
}

/// Our implementation of `PartialEq` satisfies the requirements of `Eq`
impl<'p> Eq for Location<'p> {}

impl<'p> fmt::Debug for Location<'p> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<Location: module {:?}, func {:?}, bb {}, instr {}>", self.module.name, self.func.name, pretty_bb_name(&self.bbname), self.instr)
    }
}

impl<'p> From<Location<'p>> for PathEntry {
    fn from(loc: Location<'p>) -> PathEntry {
        PathEntry {
            modname: loc.module.name.clone(),
            funcname: loc.func.name.clone(),
            bbname: loc.bbname,
            instr: loc.instr,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
struct StackFrame<'p, V: BV> {
    /// Indicates the call instruction which was responsible for the call
    callsite: Location<'p>,
    /// Caller's local variables, so they can be restored when we return to the caller.
    /// This is necessary in the case of (direct or indirect) recursion.
    /// See notes on `VarMap.get_restore_info_for_fn()`.
    restore_info: RestoreInfo<V>,
}

#[derive(Clone)]
struct BacktrackPoint<'p, B: Backend> {
    /// Where to resume execution
    loc: Location<'p>,
    /// Call stack at the `BacktrackPoint`.
    /// This is a vector of `StackFrame`s where the first entry is the top-level
    /// caller, and the last entry is the caller of the `BacktrackPoint`'s function.
    stack: Vec<StackFrame<'p, B::BV>>,
    /// Constraint to add before restarting execution at `next_bb`.
    /// (Intended use of this is to constrain the branch in that direction.)
    constraint: B::BV,
    /// `VarMap` representing the state of things at the `BacktrackPoint`.
    /// For now, we require making a full copy of the `VarMap` in order to revert
    /// later.
    varmap: VarMap<B::BV>,
    /// `Memory` representing the state of things at the `BacktrackPoint`.
    /// Copies of a `Memory` should be cheap (just a Boolector refcounted
    /// pointer), so it's not a huge concern that we need a full copy here in
    /// order to revert later.
    mem: B::Memory,
    /// The length of `path` at the `BacktrackPoint`.
    /// If we ever revert to this `BacktrackPoint`, we will truncate the `path` to
    /// its first `path_len` entries.
    path_len: usize,
}

impl<'p, B: Backend> fmt::Display for BacktrackPoint<'p, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<BacktrackPoint to execute bb {:?} with constraint {:?} and {} frames on the callstack>", self.loc.bbname, self.constraint, self.stack.len())
    }
}

impl<'p, B: Backend> State<'p, B> where B: 'p {
    /// `start_loc`: the `Location` where the `State` should begin executing.
    /// As of this writing, `start_loc` should be the entry point of a
    /// function, or you will have problems.
    pub fn new(
        project: &'p Project,
        start_loc: Location<'p>,
        config: Config<'p, B>,
    ) -> Self {
        let solver = B::SolverRef::new();
        let mut state = Self {
            cur_loc: start_loc.clone(),
            varmap: VarMap::new(solver.clone(), config.loop_bound),
            mem: RefCell::new(Memory::new_uninitialized(solver.clone(), config.null_detection, None)),
            alloc: Alloc::new(),
            global_allocations: GlobalAllocations::new(),
            stack: Vec::new(),
            backtrack_points: Vec::new(),
            path: Vec::new(),
            mem_watchpoints: Watchpoints::from_iter(config.initial_mem_watchpoints.iter().cloned()),

            // listed last (out-of-order) so that they can be used above but moved in now
            solver,
            config,
        };
        // Here we do allocation of the global variables in the Project.
        // We can do _initialization_ lazily (on first reference to the global
        // variable), but we need to do all the _allocation_ up front,
        // because initializers can refer to the addresses of other global
        // variables, potentially even circularly.
        //
        // Note that `project.all_global_vars()` gives us both global variable
        // *definitions* and *declarations*; we can distinguish these because
        // (direct quote from the LLVM docs) "Definitions have initializers,
        // declarations don't." This implies that even globals without an
        // initializer in C have one in LLVM, which seems weird to me, but it's
        // what the docs say, and also matches what I've seen empirically.
        //
        // We'll save each initializer as we allocate the global variable, but
        // only actually process each initializer as the global variable is
        // referenced for the first time.  This saves us from doing all the
        // memory reads/writes right away, which improves performance, especially
        // if the `Project` includes a lot of globals we'll never use (e.g., if
        // we parsed in way more modules than we actually need).
        info!("Allocating global variables and functions");
        debug!("Allocating global variables");
        for (var, module) in project.all_global_vars().filter(|(var,_)| var.initializer.is_some()) {
            // Allocate the global variable.
            //
            // In the allocation pass, we want to process each global variable
            // exactly once, and the order doesn't matter, so we simply process
            // definitions, since each global variable must have exactly one
            // definition. Hence the `filter()` above.
            if let Type::PointerType { pointee_type, .. } = &var.ty {
                let addr = state.allocate(size(&*pointee_type) as u64);
                debug!("Allocated {:?} at {:?}", var.name, addr);
                state.global_allocations.allocate_global_var(var, module, addr);
            } else {
                panic!("Global variable has non-pointer type {:?}", &var.ty);
            }
        }
        // We also have to allocate (at least a tiny bit of) memory for each
        // `Function`, just so that we can have pointers to those `Function`s.
        // We can use `global_allocations.get_func_for_address()` to interpret
        // these function pointers.
        // Similarly, we allocate tiny bits of memory for each function hook,
        // so that we can have pointers to those hooks.
        debug!("Allocating functions");
        for (func, module) in project.all_functions() {
            let addr: u64 = state.alloc.alloc(64 as u64);  // we just allocate 64 bits for each function. No reason to allocate more.
            let addr_bv = state.bv_from_u64(addr, 64);
            debug!("Allocated {:?} at {:?}", func.name, addr_bv);
            state.global_allocations.allocate_function(func, module, addr, addr_bv);
        }
        debug!("Allocating function hooks");
        for (funcname, hook) in state.config.function_hooks.get_all_hooks() {
            let addr: u64 = state.alloc.alloc(64 as u64);  // we just allocate 64 bits for each function. No reason to allocate more.
            let addr_bv = state.bv_from_u64(addr, 64);
            debug!("Allocated hook for {:?} at {:?}", funcname, addr_bv);
            state.global_allocations.allocate_function_hook((*hook).clone(), addr, addr_bv);
        }
        debug!("Done allocating global variables and functions");
        state
    }

    /// Fully duplicate the `State`. Unlike with `clone()`, the `State` this
    /// function returns will have a fully separate (fully duplicated) solver
    /// instance. (With `clone()`, the states will still share references to the
    /// same solver instance.)
    pub fn fork(&self) -> Self {
        let mut cloned = self.clone();
        let new_solver = cloned.solver.duplicate();
        cloned.varmap.change_solver(new_solver.clone());
        cloned.mem.borrow_mut().change_solver(new_solver.clone());
        cloned.global_allocations.change_solver(new_solver.clone());
        cloned.solver = new_solver;
        cloned
    }

    /// Returns `true` if current constraints are satisfiable, `false` if not.
    ///
    /// Returns `Error::SolverError` if the query failed (e.g., was interrupted or timed out).
    pub fn sat(&self) -> Result<bool> {
        solver_utils::sat(&self.solver)
    }

    /// Returns `true` if the current constraints plus the given additional constraints
    /// are together satisfiable, or `false` if not.
    ///
    /// Returns `Error::SolverError` if the query failed (e.g., was interrupted or timed out).
    ///
    /// Does not permanently add the given constraints to the solver.
    pub fn sat_with_extra_constraints<'b>(&'b self, constraints: impl IntoIterator<Item = &'b B::BV>) -> Result<bool> {
        solver_utils::sat_with_extra_constraints(&self.solver, constraints)
    }

    /// Returns `true` if under the current constraints, `a` and `b` must have the
    /// same value. Returns `false` if `a` and `b` may have different values. (If the
    /// current constraints are themselves unsatisfiable, that will result in
    /// `true`.)
    ///
    /// A common use case for this function is to test whether some `BV` must be
    /// equal to a given concrete value. You can do this with something like
    /// `state.bvs_must_be_equal(bv, &state.bv_from_u64(...))`.
    ///
    /// This function and `bvs_can_be_equal()` are both more efficient than
    /// `get_a_solution()` or `get_possible_solutions()`-type functions, as they do
    /// not require full model generation. You should prefer this function or
    /// `bvs_can_be_equal()` if they are sufficient for your needs.
    pub fn bvs_must_be_equal(&self, a: &B::BV, b: &B::BV) -> Result<bool> {
        solver_utils::bvs_must_be_equal(&self.solver, a, b)
    }

    /// Returns `true` if under the current constraints, `a` and `b` can have the
    /// same value. Returns `false` if `a` and `b` cannot have the same value. (If
    /// the current constraints are themselves unsatisfiable, that will also result
    /// in `false`.)
    ///
    /// A common use case for this function is to test whether some `BV` can be
    /// equal to a given concrete value. You can do this with something like
    /// `state.bvs_can_be_equal(bv, &state.bv_from_u64(...))`.
    ///
    /// This function and `bvs_must_be_equal()` are both more efficient than
    /// `get_a_solution()` or `get_possible_solutions()`-type functions, as they do
    /// not require full model generation. You should prefer this function or
    /// `bvs_must_be_equal()` if they are sufficient for your needs.
    pub fn bvs_can_be_equal(&self, a: &B::BV, b: &B::BV) -> Result<bool> {
        solver_utils::bvs_can_be_equal(&self.solver, a, b)
    }

    /// Get one possible concrete value for the `BV`.
    /// Returns `Ok(None)` if no possible solution, or `Error::SolverError` if the solver query failed.
    pub fn get_a_solution_for_bv(&self, bv: &B::BV) -> Result<Option<BVSolution>> {
        self.solver.set_opt(BtorOption::ModelGen(ModelGen::All));
        let solution = if self.sat()? {
            Some(bv.get_a_solution()).transpose()
        } else {
            Ok(None)
        };
        self.solver.set_opt(BtorOption::ModelGen(ModelGen::Disabled));
        solution
    }

    /// Get one possible concrete value for the given IR `Name` (from the given `Function` name).
    /// Returns `Ok(None)` if no possible solution, or `Error::SolverError` if the solver query failed.
    #[allow(clippy::ptr_arg)]  // as of this writing, clippy warns that the &String argument should be &str; but it actually needs to be &String here
    pub fn get_a_solution_for_irname(&mut self, funcname: &String, name: &Name) -> Result<Option<BVSolution>> {
        let bv = self.varmap.lookup_var(funcname, name);
        self.get_a_solution_for_bv(bv)
    }

    /// Get a description of the possible solutions for the `BV`.
    ///
    /// `n`: Maximum number of distinct solutions to check for.
    /// If there are more than `n` possible solutions, this returns a
    /// `PossibleSolutions::AtLeast` containing `n+1` solutions.
    ///
    /// These solutions will be disambiguated - see docs on `boolector::BVSolution`.
    ///
    /// If there are no possible solutions, this returns `Ok` with an empty
    /// `PossibleSolutions`, rather than returning an `Err` with `Error::Unsat`.
    pub fn get_possible_solutions_for_bv(&self, bv: &B::BV, n: usize) -> Result<PossibleSolutions<BVSolution>> {
        solver_utils::get_possible_solutions_for_bv(self.solver.clone(), bv, n)
    }

    /// Get a description of the possible solutions for the given IR `Name` (from the given `Function` name).
    ///
    /// `n`: Maximum number of distinct solutions to check for.
    /// If there are more than `n` possible solutions, this returns a
    /// `PossibleSolutions::AtLeast` containing `n+1` solutions.
    ///
    /// These solutions will be disambiguated - see docs on `boolector::BVSolution`.
    ///
    /// If there are no possible solutions, this returns `Ok` with an empty
    /// `PossibleSolutions`, rather than returning an `Err` with `Error::Unsat`.
    #[allow(clippy::ptr_arg)]  // as of this writing, clippy warns that the &String argument should be &str; but it actually needs to be &String here
    pub fn get_possible_solutions_for_irname(&mut self, funcname: &String, name: &Name, n: usize) -> Result<PossibleSolutions<BVSolution>> {
        let bv = self.varmap.lookup_var(funcname, name);
        self.get_possible_solutions_for_bv(bv, n)
    }

    /// Get the maximum possible solution for the `BV`: that is, the highest value
    /// for which the current set of constraints is still satisfiable.
    /// "Maximum" will be interpreted in an unsigned fashion.
    ///
    /// Returns `Ok(None)` if there is no solution for the `BV`, that is, if the
    /// current set of constraints is unsatisfiable. Only returns `Err` if a solver
    /// query itself fails.
    pub fn max_possible_solution_for_bv(&self, bv: &B::BV) -> Result<Option<u64>> {
        solver_utils::max_possible_solution_for_bv(self.solver.clone(), bv)
    }

    /// Get the maximum possible solution for the given IR `Name` (from the given
    /// `Function` name): that is, the highest value for which the current set of
    /// constraints is still satisfiable.
    /// "Maximum" will be interpreted in an unsigned fashion.
    ///
    /// Returns `Ok(None)` if there is no solution for the `BV`, that is, if the
    /// current set of constraints is unsatisfiable. Only returns `Err` if a solver
    /// query itself fails.
    pub fn max_possible_solution_for_irname(&mut self, funcname: &String, name: &Name) -> Result<Option<u64>> {
        let bv = self.varmap.lookup_var(funcname, name);
        solver_utils::max_possible_solution_for_bv(self.solver.clone(), bv)
    }

    /// Get the minimum possible solution for the `BV`: that is, the lowest value
    /// for which the current set of constraints is still satisfiable.
    /// "Minimum" will be interpreted in an unsigned fashion.
    ///
    /// Returns `Ok(None)` if there is no solution for the `BV`, that is, if the
    /// current set of constraints is unsatisfiable. Only returns `Err` if a solver
    /// query itself fails.
    pub fn min_possible_solution_for_bv(&self, bv: &B::BV) -> Result<Option<u64>> {
        solver_utils::min_possible_solution_for_bv(self.solver.clone(), bv)
    }

    /// Get the minimum possible solution for the given IR `Name` (from the given
    /// `Function` name): that is, the lowest value for which the current set of
    /// constraints is still satisfiable.
    /// "Minimum" will be interpreted in an unsigned fashion.
    ///
    /// Returns `Ok(None)` if there is no solution for the `BV`, that is, if the
    /// current set of constraints is unsatisfiable. Only returns `Err` if a solver
    /// query itself fails.
    pub fn min_possible_solution_for_irname(&self, funcname: &String, name: &Name) -> Result<Option<u64>> {
        let bv = self.varmap.lookup_var(funcname, name);
        solver_utils::min_possible_solution_for_bv(self.solver.clone(), bv)
    }

    /// Create a `BV` constant representing the given `bool` (either constant
    /// `true` or constant `false`).
    /// The resulting `BV` will be either constant `0` or constant `1`, and will
    /// have bitwidth `1`.
    pub fn bv_from_bool(&self, b: bool) -> B::BV {
        B::BV::from_bool(self.solver.clone(), b)
    }

    /// Create a `BV` representing the given constant `i32` value, with the given
    /// bitwidth.
    pub fn bv_from_i32(&self, i: i32, width: u32) -> B::BV {
        B::BV::from_i32(self.solver.clone(), i, width)
    }

    /// Create a `BV` representing the given constant `u32` value, with the given
    /// bitwidth.
    pub fn bv_from_u32(&self, u: u32, width: u32) -> B::BV {
        B::BV::from_u32(self.solver.clone(), u, width)
    }

    /// Create a `BV` representing the given constant `i64` value, with the given
    /// bitwidth.
    pub fn bv_from_i64(&self, i: i64, width: u32) -> B::BV {
        B::BV::from_i64(self.solver.clone(), i, width)
    }

    /// Create a `BV` representing the given constant `u64` value, with the given
    /// bitwidth.
    pub fn bv_from_u64(&self, u: u64, width: u32) -> B::BV {
        B::BV::from_u64(self.solver.clone(), u, width)
    }

    /// Create a `BV` representing the constant `0` of the given bitwidth.
    /// This is equivalent to `self.bv_from_i32(0, width)` but may be more
    /// efficient.
    pub fn zero(&self, width: u32) -> B::BV {
        B::BV::zero(self.solver.clone(), width)
    }

    /// Create a `BV` representing the constant `1` of the given bitwidth.
    /// This is equivalent to `self.bv_from_i32(1, width)` but may be more
    /// efficient.
    pub fn one(&self, width: u32) -> B::BV {
        B::BV::one(self.solver.clone(), width)
    }

    /// Create a `BV` constant of the given width, where all bits are set to one.
    /// This is equivalent to `self.bv_from_i32(-1, width)` but may be more
    /// efficient.
    pub fn ones(&self, width: u32) -> B::BV {
        B::BV::ones(self.solver.clone(), width)
    }

    /// Create a new (unconstrained) `BV` for the given `Name` (in the current function).
    ///
    /// This function performs uniquing, so if you call it twice
    /// with the same `Name`-`Function` pair, you will get two different `BV`s.
    ///
    /// Returns the new `BV`, or `Err` if it can't be created.
    ///
    /// (As of this writing, the only `Err` that might be returned is
    /// `Error::LoopBoundExceeded`, which is returned if creating the new `BV`
    /// would exceed `max_versions_of_name` -- see
    /// [`Config`](struct.Config.html).)
    ///
    /// Also, we assume that no two `Function`s share the same name.
    pub fn new_bv_with_name(&mut self, name: Name, bits: u32) -> Result<B::BV> {
        self.varmap.new_bv_with_name(self.cur_loc.func.name.clone(), name, bits)
    }

    /// Assign the given `BV` to the given `Name` (in the current function).
    ///
    /// This function performs uniquing, so it creates a new version of the
    /// variable represented by the `(String, Name)` pair rather than overwriting
    /// the current version.
    ///
    /// Returns `Err` if the assignment can't be performed.
    ///
    /// (As of this writing, the only `Err` that might be returned is
    /// `Error::LoopBoundExceeded`, which is returned if creating the new version
    /// of the `BV` would exceed `max_versions_of_name` -- see
    /// [`Config`](struct.Config.html).)
    pub fn assign_bv_to_name(&mut self, name: Name, bv: B::BV) -> Result<()> {
        self.varmap.assign_bv_to_name(self.cur_loc.func.name.clone(), name, bv)
    }

    /// Record the result of `thing` to be `resultval`.
    /// Assumes `thing` is in the current function.
    /// Will fail with `Error::LoopBoundExceeded` if that would exceed
    /// `max_versions_of_name` (see [`Config`](struct.Config.html)).
    #[cfg(debug_assertions)]
    pub fn record_bv_result(&mut self, thing: &impl instruction::HasResult, resultval: B::BV) -> Result<()> {
        if size(&thing.get_type()) as u32 != resultval.get_width() {
            Err(Error::OtherError(format!(
                "Computed result for an instruction has the wrong size: instruction {:?} with result size {}, but got result {:?} with size {}",
                thing,
                size(&thing.get_type()),
                resultval,
                resultval.get_width()
            )))
        } else {
            self.assign_bv_to_name(thing.get_result().clone(), resultval)
        }
    }
    #[cfg(not(debug_assertions))]
    pub fn record_bv_result(&mut self, thing: &impl instruction::HasResult, resultval: B::BV) -> Result<()> {
        self.assign_bv_to_name(thing.get_result().clone(), resultval)
    }

    /// Overwrite the latest version of the given `Name` to instead be `bv`.
    /// Assumes `Name` is in the current function.
    pub fn overwrite_latest_version_of_bv(&mut self, name: &Name, bv: B::BV) {
        self.varmap.overwrite_latest_version_of_bv(&self.cur_loc.func.name, name, bv)
    }

    /// Convert an `Operand` to the appropriate `BV`.
    /// Assumes the `Operand` is in the current function.
    /// (All `Operand`s should be either a constant or a variable we previously added to the state.)
    pub fn operand_to_bv(&self, op: &Operand) -> Result<B::BV> {
        match op {
            Operand::ConstantOperand(c) => self.const_to_bv(c),
            Operand::LocalOperand { name, .. } => Ok(self.varmap.lookup_var(&self.cur_loc.func.name, name).clone()),
            Operand::MetadataOperand => panic!("Can't convert {:?} to BV", op),
        }
    }

    /// Convert a `Constant` to the appropriate `BV`.
    pub fn const_to_bv(&self, c: &Constant) -> Result<B::BV> {
        match c {
            Constant::Int { bits, value } => Ok(self.bv_from_u64(*value, *bits)),
            Constant::Null(ty)
            | Constant::AggregateZero(ty)
            | Constant::Undef(ty)
                => Ok(self.zero(size(ty) as u32)),
            Constant::Struct { values: elements, .. }
            | Constant::Array { elements, .. }
            | Constant::Vector(elements)
                => elements.iter()
                    .map(|c| self.const_to_bv(c))  // produces an iterator over Result<B::BV>
                    .reduce(|a,b| Ok(b?.concat(&a?)))  // the lambda has type Fn(Result<B::BV>, Result<B::BV>) -> Result<B::BV>
                    .unwrap(),  // unwrap the Option<> produced by reduce(), leaving the final return type Result<B::BV>
            Constant::GlobalReference { name, .. } => {
                if let Some(ga) = self.global_allocations.get_global_address(name, self.cur_loc.module) {
                    // First, initialize the global if it hasn't been already.
                    // As mentioned in comments in `State::new()`, we lazily
                    // initialize globals upon first reference to them.
                    //
                    // We assume that global-variable initializers can only refer to the
                    // *addresses* of other globals, and not the *values* of other
                    // global constants, so that it's fine that any referred-to globals
                    // may have been allocated but not initialized at this point.
                    // This assumption seems to hold empirically: in my tests,
                    // (1) clang performs constant-folding, even at -O0, on global
                    //     variable initializers so that these initializers do not refer to
                    //     the values of other global constants at the LLVM level. For
                    //     instance, the C code
                    //       `const int a = 1; const int b = a + 3;`
                    //     is translated into the LLVM equivalent of
                    //       `const int a = 1; const int b = 4;`
                    // (2) clang rejects programs where global variable initializers refer
                    //     to the value of externally-defined global constants, in which
                    //     case the constant-folding described above would be impossible.
                    //     Note, however, that clang does allow referring to the *addresses*
                    //     of externally-defined global variables.
                    // Therefore, we can go ahead and set our `.initialized` flag early,
                    // because even if `const_to_bv` on our initializer references other
                    // globals (possibly causing their lazy initialization as well),
                    // those globals can't refer to our contents, so won't know that we
                    // are lying about being initialized.
                    // Setting the flag early prevents an infinite loop where I try to
                    // initialize, but my initializer refers to your address so you try
                    // to initialize, but your initializer refers to my address so I try
                    // to initialize, etc.
                    if !ga.initialized.get() {
                        debug!("Initializing {:?} with initializer {:?}", name, &ga.initializer);
                        ga.initialized.set(true);
                        let write_val = self.const_to_bv(&ga.initializer)?;
                        self.mem.borrow_mut().write(&ga.addr, write_val)?;
                    }
                    Ok(ga.addr.clone())
                } else if let Some(alias) = self.cur_loc.module.global_aliases.iter().find(|a| &a.name == name) {
                    self.const_to_bv(&alias.aliasee)
                } else {
                    Err(Error::OtherError(format!("const_to_bv: GlobalReference to {:?} which was not found (current module is {:?})", name, &self.cur_loc.module.name)))
                }
            },
            Constant::Add(a) => Ok(self.const_to_bv(&a.operand0)?.add(&self.const_to_bv(&a.operand1)?)),
            Constant::Sub(s) => Ok(self.const_to_bv(&s.operand0)?.sub(&self.const_to_bv(&s.operand1)?)),
            Constant::Mul(m) => Ok(self.const_to_bv(&m.operand0)?.mul(&self.const_to_bv(&m.operand1)?)),
            Constant::UDiv(u) => Ok(self.const_to_bv(&u.operand0)?.udiv(&self.const_to_bv(&u.operand1)?)),
            Constant::SDiv(s) => Ok(self.const_to_bv(&s.operand0)?.sdiv(&self.const_to_bv(&s.operand1)?)),
            Constant::URem(u) => Ok(self.const_to_bv(&u.operand0)?.urem(&self.const_to_bv(&u.operand1)?)),
            Constant::SRem(s) => Ok(self.const_to_bv(&s.operand0)?.srem(&self.const_to_bv(&s.operand1)?)),
            Constant::And(a) => Ok(self.const_to_bv(&a.operand0)?.and(&self.const_to_bv(&a.operand1)?)),
            Constant::Or(o) => Ok(self.const_to_bv(&o.operand0)?.or(&self.const_to_bv(&o.operand1)?)),
            Constant::Xor(x) => Ok(self.const_to_bv(&x.operand0)?.xor(&self.const_to_bv(&x.operand1)?)),
            Constant::Shl(s) => Ok(self.const_to_bv(&s.operand0)?.sll(&self.const_to_bv(&s.operand1)?)),
            Constant::LShr(s) => Ok(self.const_to_bv(&s.operand0)?.srl(&self.const_to_bv(&s.operand1)?)),
            Constant::AShr(s) => Ok(self.const_to_bv(&s.operand0)?.sra(&self.const_to_bv(&s.operand1)?)),
            Constant::ExtractElement(ee) => match &ee.index {
                Constant::Int { value: index, .. } => match &ee.vector {
                    Constant::Vector(els) => {
                        let el = els.get(*index as usize)
                            .ok_or_else(|| Error::MalformedInstruction("Constant::ExtractElement index out of range".to_owned()))?;
                        self.const_to_bv(el)
                    },
                    c => Err(Error::MalformedInstruction(format!("Expected ExtractElement.vector to be a Constant::Vector, got {:?}", c))),
                },
                index => Err(Error::MalformedInstruction(format!("Expected ExtractElement.index to be a Constant::Int, but got {:?}", index))),
            },
            Constant::InsertElement(ie) => match &ie.index {
                Constant::Int { value: index, .. } => match &ie.vector {
                    Constant::Vector(els) => {
                        let mut els = els.clone();
                        let el: &mut Constant = els.get_mut(*index as usize)
                            .ok_or_else(|| Error::MalformedInstruction("Constant::InsertElement index out of range".to_owned()))?;
                        *el = ie.element.clone();
                        self.const_to_bv(&Constant::Vector(els))
                    },
                    c => Err(Error::MalformedInstruction(format!("Expected InsertElement.vector to be a Constant::Vector, got {:?}", c))),
                },
                index => Err(Error::MalformedInstruction(format!("Expected InsertElement.index to be a Constant::Int, but got {:?}", index))),
            }
            Constant::ExtractValue(ev) => self.const_to_bv(Self::simplify_const_ev(&ev.aggregate, ev.indices.iter().copied())?),
            Constant::InsertValue(iv) => self.const_to_bv(&Self::simplify_const_iv(iv.aggregate.clone(), iv.element.clone(), iv.indices.iter().copied())?),
            Constant::GetElementPtr(gep) => {
                // heavily inspired by `ExecutionManager::symex_gep()` in symex.rs. TODO could try to share more code
                let bvbase = self.const_to_bv(&gep.address)?;
                let offset = self.get_offset_recursive(gep.indices.iter(), &gep.address.get_type(), bvbase.get_width())?;
                Ok(bvbase.add(&offset))
            },
            Constant::Trunc(t) => self.const_to_bv(&t.operand).map(|bv| bv.slice(size(&t.to_type) as u32 - 1, 0)),
            Constant::ZExt(z) => self.const_to_bv(&z.operand).map(|bv| zero_extend_to_bits(bv, size(&z.to_type) as u32)),
            Constant::SExt(s) => self.const_to_bv(&s.operand).map(|bv| sign_extend_to_bits(bv, size(&s.to_type) as u32)),
            Constant::PtrToInt(pti) => {
                let bv = self.const_to_bv(&pti.operand)?;
                assert_eq!(bv.get_width(), size(&pti.to_type) as u32);
                Ok(bv)  // just a cast, it's the same bits underneath
            },
            Constant::IntToPtr(itp) => {
                let bv = self.const_to_bv(&itp.operand)?;
                assert_eq!(bv.get_width(), size(&itp.to_type) as u32);
                Ok(bv)  // just a cast, it's the same bits underneath
            },
            Constant::BitCast(bc) => {
                let bv = self.const_to_bv(&bc.operand)?;
                assert_eq!(bv.get_width(), size(&bc.to_type) as u32);
                Ok(bv)  // just a cast, it's the same bits underneath
            },
            Constant::AddrSpaceCast(ac) => {
                let bv = self.const_to_bv(&ac.operand)?;
                assert_eq!(bv.get_width(), size(&ac.to_type) as u32);
                Ok(bv)  // just a cast, it's the same bits underneath
            },
            Constant::ICmp(icmp) => {
                let bv0 = self.const_to_bv(&icmp.operand0)?;
                let bv1 = self.const_to_bv(&icmp.operand1)?;
                Ok(match icmp.predicate {
                    IntPredicate::EQ => bv0._eq(&bv1),
                    IntPredicate::NE => bv0._ne(&bv1),
                    IntPredicate::UGT => bv0.ugt(&bv1),
                    IntPredicate::UGE => bv0.ugte(&bv1),
                    IntPredicate::ULT => bv0.ult(&bv1),
                    IntPredicate::ULE => bv0.ulte(&bv1),
                    IntPredicate::SGT => bv0.sgt(&bv1),
                    IntPredicate::SGE => bv0.sgte(&bv1),
                    IntPredicate::SLT => bv0.slt(&bv1),
                    IntPredicate::SLE => bv0.slte(&bv1),
                })
            },
            Constant::Select(s) => {
                let b = self.const_to_bv(&s.condition)?;
                match b.as_bool() {
                    None => Err(Error::MalformedInstruction("Constant::Select: Expected a constant condition".to_owned())),
                    Some(true) => self.const_to_bv(&s.true_value),
                    Some(false) => self.const_to_bv(&s.false_value),
                }
            },
            _ => unimplemented!("const_to_bv for {:?}", c),
        }
    }

    /// Given a `Constant::Struct` and a series of `ExtractValue` indices, get the
    /// final `Constant` referred to
    fn simplify_const_ev(s: &Constant, mut indices: impl Iterator<Item = u32>) -> Result<&Constant> {
        match indices.next() {
            None => Ok(s),
            Some(index) => {
                if let Constant::Struct { values, .. } = s {
                    let val = values.get(index as usize).ok_or_else(|| Error::MalformedInstruction("Constant::ExtractValue index out of range".to_owned()))?;
                    Self::simplify_const_ev(val, indices)
                } else {
                    panic!("simplify_const_ev: not a Constant::Struct: {:?}", s)
                }
            }
        }

    }

    /// Given a `Constant::Struct`, a value to insert, and a series of
    /// `InsertValue` indices, get the final `Constant` referred to
    fn simplify_const_iv(s: Constant, val: Constant, mut indices: impl Iterator<Item = u32>) -> Result<Constant> {
        match indices.next() {
            None => Ok(val),
            Some(index) => {
                if let Constant::Struct { name, mut values, is_packed } = s {
                    let to_replace = values.get(index as usize).ok_or_else(|| Error::MalformedInstruction("Constant::InsertValue index out of range".to_owned()))?.clone();
                    values[index as usize] = Self::simplify_const_iv(to_replace, val, indices)?;
                    Ok(Constant::Struct { name, values, is_packed })
                } else {
                    panic!("simplify_const_iv: not a Constant::Struct: {:?}", s)
                }
            }
        }
    }

    /// Get the offset of the element (in bytes, as a `BV` of `result_bits` bits)
    fn get_offset_recursive<'a>(&self, mut indices: impl Iterator<Item = &'a Constant>, base_type: &Type, result_bits: u32) -> Result<B::BV> {
        match indices.next() {
            None => Ok(self.zero(result_bits)),
            Some(index) => match base_type {
                Type::PointerType { .. } | Type::ArrayType { .. } | Type::VectorType { .. } => {
                    let index = zero_extend_to_bits(self.const_to_bv(index)?, result_bits);
                    let (offset, nested_ty) = get_offset_bv_index(base_type, &index, self.solver.clone())?;
                    self.get_offset_recursive(indices, nested_ty, result_bits)
                        .map(|bv| bv.add(&offset))
                },
                Type::StructType { .. } => match index {
                    Constant::Int { value: index, .. } => {
                        let (offset, nested_ty) = get_offset_constant_index(base_type, *index as usize)?;
                        self.get_offset_recursive(indices, &nested_ty, result_bits)
                            .map(|bv| bv.add(&self.bv_from_u64(offset as u64, result_bits)))
                    },
                    _ => Err(Error::MalformedInstruction(format!("Expected index into struct type to be a constant int, but got index {:?}", index))),
                },
                Type::NamedStructType { ty, .. } => {
                    let arc: Arc<RwLock<Type>> = ty.as_ref()
                        .ok_or_else(|| Error::MalformedInstruction("get_offset on an opaque struct type".to_owned()))?
                        .upgrade()
                        .expect("Failed to upgrade weak reference");
                    let actual_ty: &Type = &arc.read().unwrap();
                    if let Type::StructType { .. } = actual_ty {
                        // this code copied from the StructType case
                        match index {
                            Constant::Int { value: index, .. } => {
                                let (offset, nested_ty) = get_offset_constant_index(base_type, *index as usize)?;
                                self.get_offset_recursive(indices, &nested_ty, result_bits).map(|bv| bv.add(&self.bv_from_u64(offset as u64, result_bits)))
                            },
                            _ => Err(Error::MalformedInstruction(format!("Expected index into struct type to be a constant int, but got index {:?}", index))),
                        }
                    } else {
                        Err(Error::MalformedInstruction(format!("Expected NamedStructType inner type to be a StructType, but got {:?}", actual_ty)))
                    }
                }
                _ => panic!("get_offset_recursive with base type {:?}", base_type),
            }
        }
    }

    /// Given a `BV`, interpret it as a function pointer, and return a
    /// description of the possible `Function`s which it would point to.
    ///
    /// `n`: Maximum number of distinct `Function`s to check for.
    /// If there are more than `n` possible `Function`s, this returns a
    /// `PossibleSolutions::AtLeast` with `n+1` `Function`s.
    ///
    /// Possible errors:
    ///   - `Error::SolverError` if the solver query fails
    ///   - `Error::OtherError` if it finds that it is possible that the `BV`
    ///     points to something that's not a `Function` in the `Project`
    pub(crate) fn interpret_as_function_ptr(&mut self, bv: B::BV, n: usize) -> Result<PossibleSolutions<Callable<'p, B>>> {
        if n == 0 {
            unimplemented!("n == 0 in interpret_as_function_ptr")
        }
        // First try to interpret without a full solve (i.e., with `as_u64()`)
        match bv.as_u64().and_then(|addr| self.global_allocations.get_func_for_address(addr, self.cur_loc.module)) {
            Some(f) => Ok(PossibleSolutions::Exactly(HashSet::from_iter(std::iter::once(f)))),  // there is only one possible solution, and it's this `f`
            None => {
                match self.get_possible_solutions_for_bv(&bv, n)? {
                    PossibleSolutions::Exactly(v) => {
                        v.into_iter()
                            .map(|addr| {
                                let addr = addr.as_u64().unwrap();
                                self.global_allocations.get_func_for_address(addr, self.cur_loc.module)
                                    .ok_or_else(|| Error::OtherError(format!("This BV can't be interpreted as a function pointer: it has a possible solution 0x{:x} which points to something that's not a function.\n  The BV was: {:?}", addr, bv)))
                            })
                            .collect::<Result<HashSet<_>>>()
                            .map(PossibleSolutions::Exactly)
                    },
                    PossibleSolutions::AtLeast(v) => {
                        v.into_iter()
                            .map(|addr| {
                                let addr = addr.as_u64().unwrap();
                                self.global_allocations.get_func_for_address(addr, self.cur_loc.module)
                                    .ok_or_else(|| Error::OtherError(format!("This BV can't be interpreted as a function pointer: it has a possible solution 0x{:?} which points to something that's not a function.\n  The BV was: {:?}", addr, bv)))
                            })
                            .collect::<Result<HashSet<_>>>()
                            .map(PossibleSolutions::AtLeast)
                    }
                }
            }
        }
    }

    /// Get a pointer to the given function name. The name will be resolved in the current module.
    ///
    /// Returns `None` if no function was found with that name.
    pub fn get_pointer_to_function(&self, funcname: impl Into<String>) -> Option<&B::BV> {
        self.global_allocations.get_global_address(&Name::Name(funcname.into()), self.cur_loc.module).map(|ga| &ga.addr)
    }

    /// Get a pointer to the currently active _hook_ for the given function name.
    ///
    /// Returns `None` if no function was found with that name, _or_ if there is no currently
    /// active hook for that function.
    pub fn get_pointer_to_function_hook(&self, funcname: &str) -> Option<&B::BV> {
        self.global_allocations.get_function_hook_address(self.config.function_hooks.get_hook_for(funcname)?)
    }

    /// Read a value `bits` bits long from memory at `addr`.
    /// Note that `bits` can be arbitrarily large.
    pub fn read(&self, addr: &B::BV, bits: u32) -> Result<B::BV> {
        self.mem_watchpoints.process_watchpoint_triggers(self, addr, bits, false)?;
        self.mem.borrow().read(addr, bits)
    }

    /// Write a value into memory at `addr`.
    /// Note that `val` can be an arbitrarily large bitvector.
    pub fn write(&mut self, addr: &B::BV, val: B::BV) -> Result<()> {
        self.mem_watchpoints.process_watchpoint_triggers(self, addr, val.get_width(), true)?;
        self.mem.borrow_mut().write(addr, val)
    }

    /// Add a memory watchpoint. It will be enabled unless/until
    /// `disable_watchpoint()` is called on it.
    ///
    /// If a watchpoint with the same name was previously added, this will
    /// replace that watchpoint and return `true`. Otherwise, this will return
    /// `false`.
    ///
    /// When any watched memory is read or written to, an INFO-level log message
    /// will be generated.
    pub fn add_mem_watchpoint(&mut self, name: impl Into<String>, watchpoint: Watchpoint) -> bool {
        self.mem_watchpoints.add(name, watchpoint)
    }

    /// Remove the memory watchpoint with the given `name`.
    ///
    /// Returns `true` if the operation was successful, or `false` if no
    /// watchpoint with that name was found.
    pub fn rm_mem_watchpoint(&mut self, name: &str) -> bool {
        self.mem_watchpoints.remove(name)
    }

    /// Disable the memory watchpoint with the given `name`. Disabled
    /// watchpoints will not generate any log messages unless/until
    /// `enable_watchpoint()` is called on them.
    ///
    /// Returns `true` if the operation is successful, or `false` if no
    /// watchpoint with that name was found. Disabling an already-disabled
    /// watchpoint will have no effect and will return `true`.
    pub fn disable_watchpoint(&mut self, name: &str) -> bool {
        self.mem_watchpoints.disable(name)
    }

    /// Enable the memory watchpoint(s) with the given name.
    ///
    /// Returns `true` if the operation is successful, or `false` if no
    /// watchpoint with that name was found. Enabling an already-enabled
    /// watchpoint will have no effect and will return `true`.
    pub fn enable_watchpoint(&mut self, name: &str) -> bool {
        self.mem_watchpoints.enable(name)
    }

    /// Allocate a value of size `bits`; return a pointer to the newly allocated object
    pub fn allocate(&mut self, bits: impl Into<u64>) -> B::BV {
        let raw_ptr = self.alloc.alloc(bits);
        self.bv_from_u64(raw_ptr, 64)
    }

    /// Get the size, in bits, of the allocation at the given address, or `None`
    /// if that address is not the result of an `alloc()`.
    pub fn get_allocation_size(&mut self, addr: &B::BV) -> Result<Option<u64>> {
        // First try to obtain the address without a full solve (i.e., with `as_u64()`)
        match addr.as_u64() {
            Some(addr) => Ok(self.alloc.get_allocation_size(addr)),
            None => {
                match self.get_possible_solutions_for_bv(addr, 1)? {
                    PossibleSolutions::AtLeast(_) => Err(Error::OtherError(format!("get_allocation_size: address is not a constant: {:?}", addr))),  // must be at least 2 solutions, since we passed in n==1
                    PossibleSolutions::Exactly(v) => {
                        let addr = v.iter()
                            .next()
                            .ok_or(Error::Unsat)?
                            .as_u64()
                            .ok_or_else(|| Error::OtherError("get_allocation_size: address is more than 64 bits wide".to_owned()))?;
                        Ok(self.alloc.get_allocation_size(addr))
                    },
                }
            }
        }
    }

    /// Record the current location as a `PathEntry` in the current path.
    pub fn record_path_entry(&mut self) {
        let entry = PathEntry::from(self.cur_loc.clone());
        debug!("Recording a path entry {:?}", entry);
        self.path.push(entry);
    }

    /// Get the `PathEntry`s that have been recorded, in order
    pub fn get_path(&self) -> &Vec<PathEntry> {
        &self.path
    }

    /// Record entering a call at the current location
    pub fn push_callsite(&mut self) {
        self.stack.push(StackFrame {
            callsite: self.cur_loc.clone(),
            // TODO: taking this `restore_info` every time a callsite is pushed
            // may be expensive, and is only necessary if the call we're going
            // to make will eventually (directly or indirectly) recurse. In the
            // future we could check the LLVM 'norecurse' attribute to know when
            // this is not necessary.
            restore_info: self.varmap.get_restore_info_for_fn(self.cur_loc.func.name.clone()),
        })
    }

    /// Record leaving the current function. Returns the `Location` at which the
    /// current function was called, or `None` if the current function was the
    /// top-level function.
    ///
    /// Also restores the caller's local variables.
    pub fn pop_callsite(&mut self) -> Option<Location<'p>> {
        if let Some(StackFrame { callsite, restore_info }) = self.stack.pop() {
            self.varmap.restore_fn_vars(restore_info);
            Some(callsite)
        } else {
            None
        }
    }

    /// Save the current state, about to enter the `BasicBlock` with the given `Name` (which must be
    /// in the same `Module` and `Function` as `state.cur_loc`), as a backtracking point.
    /// The constraint will be added only if we end up backtracking to this point, and only then.
    pub fn save_backtracking_point(&mut self, bb_to_enter: Name, constraint: B::BV) {
        debug!("Saving a backtracking point, which would enter bb {:?} with constraint {:?}", bb_to_enter, constraint);
        self.solver.push(1);
        let backtrack_loc = Location {
            module: self.cur_loc.module,
            func: self.cur_loc.func,
            bbname: bb_to_enter,
            instr: 0,
        };
        self.backtrack_points.push(BacktrackPoint {
            loc: backtrack_loc,
            stack: self.stack.clone(),
            constraint,
            varmap: self.varmap.clone(),
            mem: self.mem.borrow().clone(),
            path_len: self.path.len(),
        });
    }

    /// returns `Ok(true)` if the operation was successful, `Ok(false)` if there are
    /// no saved backtracking points, or `Err` for other errors
    pub fn revert_to_backtracking_point(&mut self) -> Result<bool> {
        if let Some(bp) = self.backtrack_points.pop() {
            debug!("Reverting to backtracking point {}", bp);
            self.solver.pop(1);
            self.varmap = bp.varmap;
            self.mem.replace(bp.mem);
            self.stack = bp.stack;
            self.path.truncate(bp.path_len);
            self.cur_loc = bp.loc;
            bp.constraint.assert()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// returns the number of saved backtracking points
    pub fn count_backtracking_points(&self) -> usize {
        self.backtrack_points.len()
    }

    /// returns a `String` containing a formatted view of the current LLVM backtrace
    pub fn pretty_llvm_backtrace(&self) -> String {
        std::iter::once(format!("  #1: {:?}\n", PathEntry::from(self.cur_loc.clone())))
            .chain(self.stack.iter().rev().zip(2..).map(|(frame, num)| {
                format!("  #{}: {:?}\n", num, PathEntry::from(frame.callsite.clone()))
            }))
            .collect()
    }

    /// returns a `String` describing a set of satisfying assignments for all variables
    pub fn current_assignments_as_pretty_string(&self) -> Result<String> {
        self.solver.set_opt(BtorOption::ModelGen(ModelGen::All));
        let string = if self.sat()? {
            let printed = self.solver.print_model();
            let sorted = itertools::sorted(printed.lines());
            sorted.fold(String::new(), |s, line| s + "\n" + line)
        } else {
            "<state is unsatisfiable>".to_owned()
        };
        self.solver.set_opt(BtorOption::ModelGen(ModelGen::Disabled));
        Ok(string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solver_utils::SolutionCount;
    use std::collections::HashMap;

    // we don't include tests here for Memory, Alloc, VarMap, or Watchpoints; those are tested in their own modules.
    // Instead, here we just test the nontrivial functionality that `State` has itself.
    // We do repeat many of the tests from the `solver_utils` module, making sure that they also pass when
    // we use the `State` interfaces.

    /// utility to initialize a `State` out of a `Project` and a function name
    fn blank_state<'p>(project: &'p Project, funcname: &str) -> State<'p, BtorBackend> {
        let (func, module) = project.get_func_by_name(funcname).expect("Failed to find function");
        let start_loc = Location {
            module,
            func,
            bbname: "test_bb".to_owned().into(),
            instr: 0,
        };
        State::new(project, start_loc, Config::default())
    }

    /// Utility that creates a simple `Project` for testing.
    /// The `Project` will contain a single `Module` (with the given name) which contains
    /// a single function (given).
    fn blank_project(modname: impl Into<String>, func: Function) -> Project {
        Project::from_module(Module {
            name: modname.into(),
            source_file_name: String::new(),
            data_layout: String::new(),
            target_triple: None,
            functions: vec![func],
            global_vars: vec![],
            global_aliases: vec![],
            named_struct_types: HashMap::new(),
            inline_assembly: String::new(),
        })
    }

    /// utility that creates a technically valid (but functionally useless) `Function` for testing
    fn blank_function(name: impl Into<String>) -> Function {
        Function::new(name)
    }

    #[test]
    fn sat() -> Result<()> {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");

        // empty state should be sat
        assert_eq!(state.sat(), Ok(true));

        // adding True constraint should still be sat
        state.bv_from_bool(true).assert();
        assert_eq!(state.sat(), Ok(true));

        // adding x > 0 constraint should still be sat
        let x = state.new_bv_with_name(Name::from("x"), 64)?;
        x.sgt(&state.zero(64)).assert();
        assert_eq!(state.sat(), Ok(true));

        Ok(())
    }

    #[test]
    fn unsat() -> Result<()> {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let state = blank_state(&project, "test_func");

        // adding False constraint should be unsat
        state.bv_from_bool(false).assert();
        assert_eq!(state.sat(), Ok(false));

        Ok(())
    }

    #[test]
    fn unsat_with_extra_constraints() -> Result<()> {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");

        // adding x > 3 constraint should still be sat
        let x = state.new_bv_with_name(Name::from("x"), 64)?;
        x.ugt(&state.bv_from_u64(3, 64)).assert();
        assert_eq!(state.sat(), Ok(true));

        // adding x < 3 constraint should make us unsat
        let bad_constraint = x.ult(&state.bv_from_u64(3, 64));
        assert_eq!(state.sat_with_extra_constraints(std::iter::once(&bad_constraint)), Ok(false));

        // the state itself should still be sat, extra constraints weren't permanently added
        assert_eq!(state.sat(), Ok(true));

        Ok(())
    }

    #[test]
    fn get_a_solution() -> Result<()> {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");

        // add x > 3 constraint
        let x = state.new_bv_with_name(Name::from("x"), 64)?;
        x.ugt(&state.bv_from_u64(3, 64)).assert();

        // check that the computed value of x is > 3
        let x_value = state.get_a_solution_for_bv(&x).unwrap().expect("Expected a solution for x").as_u64().unwrap();
        assert!(x_value > 3);

        Ok(())
    }

    #[test]
    fn possible_solutions() -> Result<()> {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");

        // add x > 3 constraint
        let x = state.new_bv_with_name(Name::from("x"), 64)?;
        x.ugt(&state.bv_from_u64(3, 64)).assert();

        // check that there are more than 2 solutions
        let num_solutions = state.get_possible_solutions_for_bv(&x, 2).unwrap().count();
        assert_eq!(num_solutions, SolutionCount::AtLeast(3));

        // add x < 6 constraint
        x.ult(&state.bv_from_u64(6, 64)).assert();

        // check that there are now exactly two solutions
        let solutions = state.get_possible_solutions_for_bv(&x, 2).unwrap().as_u64_solutions();
        assert_eq!(solutions, Some(PossibleSolutions::Exactly(HashSet::from_iter(vec![4,5].into_iter()))));

        // add x < 5 constraint
        x.ult(&state.bv_from_u64(5, 64)).assert();

        // check that there is now exactly one solution
        let solutions = state.get_possible_solutions_for_bv(&x, 2).unwrap().as_u64_solutions();
        assert_eq!(solutions, Some(PossibleSolutions::Exactly(HashSet::from_iter(std::iter::once(4)))));

        // add x < 3 constraint
        x.ult(&state.bv_from_u64(3, 64)).assert();

        // check that there are now no solutions
        let solutions = state.get_possible_solutions_for_bv(&x, 2).unwrap().as_u64_solutions();
        assert_eq!(solutions, Some(PossibleSolutions::Exactly(HashSet::new())));

        Ok(())
    }

    #[test]
    fn lookup_vars_via_operand() {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");

        // create llvm-ir names
        let name1 = Name::from("val");
        let name2 = Name::from(2);

        // create corresponding BV values
        let var1 = state.new_bv_with_name(name1.clone(), 64).unwrap();
        let var2 = state.new_bv_with_name(name2.clone(), 1).unwrap();  // these clone()s wouldn't normally be necessary but we want to reuse the names to create `Operand`s later

        // check that we can look up the correct BV values via LocalOperands
        let op1 = Operand::LocalOperand { name: name1, ty: Type::i32() };
        let op2 = Operand::LocalOperand { name: name2, ty: Type::bool() };
        assert_eq!(state.operand_to_bv(&op1), Ok(var1));
        assert_eq!(state.operand_to_bv(&op2), Ok(var2));
    }

    #[test]
    fn const_bv() {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let state = blank_state(&project, "test_func");

        // create an llvm-ir value which is constant 3
        let constint = Constant::Int { bits: 64, value: 3 };

        // this should create a corresponding BV value which is also constant 3
        let bv = state.operand_to_bv(&Operand::ConstantOperand(constint)).unwrap();

        // check that the BV value was evaluated to 3
        let solution = state.get_a_solution_for_bv(&bv).unwrap().expect("Expected a solution for the bv").as_u64().unwrap();
        assert_eq!(solution, 3);
    }

    #[test]
    fn const_bool() {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let state = blank_state(&project, "test_func");

        // create llvm-ir constants true and false
        let consttrue = Constant::Int { bits: 1, value: 1 };
        let constfalse = Constant::Int { bits: 1, value: 0 };

        // this should create BV values true and false
        let bvtrue = state.operand_to_bv(&Operand::ConstantOperand(consttrue)).unwrap();
        let bvfalse = state.operand_to_bv(&Operand::ConstantOperand(constfalse)).unwrap();

        // check that the BV values are evaluated to true and false respectively
        assert_eq!(
            state.get_a_solution_for_bv(&bvtrue).unwrap().expect("Expected a solution for bvtrue").as_bool().unwrap(),
            true,
        );
        assert_eq!(
            state.get_a_solution_for_bv(&bvfalse).unwrap().expect("Expected a solution for bvfalse").as_bool().unwrap(),
            false,
        );

        // assert the first one, which should be true, so we should still be sat
        bvtrue.assert();
        assert_eq!(state.sat(), Ok(true));

        // assert the second one, which should be false, so we should be unsat
        bvfalse.assert();
        assert_eq!(state.sat(), Ok(false));
    }

    #[test]
    fn backtracking() -> Result<()> {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");
        state.record_path_entry();

        // assert x > 11
        let x = state.new_bv_with_name(Name::from("x"), 64)?;
        x.sgt(&state.bv_from_i64(11, 64)).assert();

        // create a backtrack point with constraint y > 5
        let y = state.new_bv_with_name(Name::from("y"), 64)?;
        let constraint = y.sgt(&state.bv_from_i64(5, 64));
        let bb = BasicBlock::new(Name::from("bb_target"));
        state.save_backtracking_point(bb.name.clone(), constraint);

        // check that the constraint y > 5 wasn't added: adding y < 4 should keep us sat
        assert_eq!(
            state.sat_with_extra_constraints(std::iter::once(&y.slt(&state.bv_from_i64(4, 64)))),
            Ok(true),
        );

        // assert x < 8 to make us unsat
        x.slt(&state.bv_from_i64(8, 64)).assert();
        assert_eq!(state.sat(), Ok(false));

        // note the pre-rollback location
        let pre_rollback = state.cur_loc.clone();

        // roll back to backtrack point; check that we ended up at the right loc
        // and with the right path
        assert!(state.revert_to_backtracking_point().unwrap());
        assert_eq!(state.cur_loc.func, pre_rollback.func);
        assert_eq!(state.cur_loc.bbname, bb.name);
        assert_eq!(state.get_path(), &vec![PathEntry {
            modname: "test_mod".into(),
            funcname: "test_func".into(),
            bbname: "test_bb".into(),  // the `blank_state` comes with this as the current bb name
            instr: 0,
        }]);

        // check that the constraint x < 8 was removed: we're sat again
        assert_eq!(state.sat(), Ok(true));

        // check that the constraint y > 5 was added: y evaluates to something > 5
        assert!(state.get_a_solution_for_bv(&y).unwrap().expect("Expected a solution for y").as_u64().unwrap() > 5);

        // check that the first constraint remained in place: x > 11
        assert!(state.get_a_solution_for_bv(&x).unwrap().expect("Expected a solution for x").as_u64().unwrap() > 11);

        // check that trying to backtrack again fails
        assert!(!state.revert_to_backtracking_point().unwrap());

        Ok(())
    }

    #[test]
    fn fork() {
        let func = blank_function("test_func");
        let project = blank_project("test_mod", func);
        let mut state = blank_state(&project, "test_func");

        // assert x < 42
        let x = state.new_bv_with_name(Name::from("x"), 64).unwrap();
        x.ult(&state.bv_from_u32(42, 64)).assert();

        // `y` is equal to `x + 7`
        let y = x.add(&state.bv_from_u32(7, 64));
        state.assign_bv_to_name(Name::from("y"), y).unwrap();

        // fork the state
        let mut state_2 = state.fork();

        // get the copies of `x` and `y` in each state, via operand lookups
        let op_x = Operand::LocalOperand { name: Name::from("x"), ty: Type::i64() };
        let op_y = Operand::LocalOperand { name: Name::from("y"), ty: Type::i64() };
        let x_1 = state.operand_to_bv(&op_x).unwrap();
        let x_2 = state_2.operand_to_bv(&op_x).unwrap();
        let y_1 = state.operand_to_bv(&op_y).unwrap();
        let y_2 = state_2.operand_to_bv(&op_y).unwrap();

        // in one state, we assert `x > 3`, while in the other, we assert `x < 3`
        x_1.ugt(&state.bv_from_u32(3, 64)).assert();
        x_2.ult(&state_2.bv_from_u32(3, 64)).assert();

        // check that in the first state, `y > 10` (looking up two different ways)
        let y_solution = state
            .get_a_solution_for_bv(&y_1)
            .unwrap()
            .expect("Expected a solution for y")
            .as_u64()
            .unwrap();
        assert!(y_solution > 10);
        let y_solution = state
            .get_a_solution_for_irname(&"test_func".to_owned(), &Name::from("y"))
            .unwrap()
            .expect("Expected a solution for y")
            .as_u64()
            .unwrap();
        assert!(y_solution > 10);

        // check that in the second state, `y < 10` (looking up two different ways)
        let y_2_solution = state_2
            .get_a_solution_for_bv(&y_2)
            .unwrap()
            .expect("Expected a solution for y_2")
            .as_u64()
            .unwrap();
        assert!(y_2_solution < 10);
        let y_2_solution = state_2
            .get_a_solution_for_irname(&"test_func".to_owned(), &Name::from("y"))
            .unwrap()
            .expect("Expected a solution for y_2")
            .as_u64()
            .unwrap();
        assert!(y_2_solution < 10);
    }
}
