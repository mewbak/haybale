use crate::backend::Backend;
use crate::error::*;
use crate::alloc_utils;
use crate::project::Project;
use crate::return_value::*;
use crate::state::State;
use llvm_ir::*;

pub(crate) fn malloc_hook<'p, B: Backend + 'p>(_proj: &'p Project, state: &mut State<'p, B>, call: &'p instruction::Call) -> Result<ReturnValue<B::BV>> {
    assert_eq!(call.arguments.len(), 1);
    let bytes = &call.arguments[0].0;
    match bytes.get_type() {
        Type::IntegerType { .. } => {},
        ty => return Err(Error::OtherError(format!("malloc_hook: expected argument to have integer type, but got {:?}", ty))),
    };
    match call.get_type() {
        Type::PointerType { .. } => {},
        ty => return Err(Error::OtherError(format!("malloc_hook: expected return type to be a pointer type, but got {:?}", ty))),
    };

    let addr = alloc_utils::malloc(state, bytes)?;
    Ok(ReturnValue::Return(addr))
}

pub(crate) fn calloc_hook<'p, B: Backend + 'p>(_proj: &'p Project, state: &mut State<'p, B>, call: &'p instruction::Call) -> Result<ReturnValue<B::BV>> {
    assert_eq!(call.arguments.len(), 2);
    let num = &call.arguments[0].0;
    let size = &call.arguments[1].0;
    match num.get_type() {
        Type::IntegerType { .. } => {},
        ty => return Err(Error::OtherError(format!("calloc_hook: expected first argument to have integer type, but got {:?}", ty))),
    };
    match size.get_type() {
        Type::IntegerType { .. } => {},
        ty => return Err(Error::OtherError(format!("calloc_hook: expected second argument to have integer type, but got {:?}", ty))),
    };
    match call.get_type() {
        Type::PointerType { .. } => {},
        ty => return Err(Error::OtherError(format!("calloc_hook: expected return type to be a pointer type, but got {:?}", ty))),
    };

    let addr = alloc_utils::calloc(state, num, size)?;
    Ok(ReturnValue::Return(addr))
}

pub(crate) fn free_hook<'p, B: Backend + 'p>(_proj: &'p Project, _state: &mut State<'p, B>, _call: &'p instruction::Call) -> Result<ReturnValue<B::BV>> {
    // The simplest implementation of free() is a no-op.
    // Our allocator won't ever reuse allocated addresses anyway.
    Ok(ReturnValue::ReturnVoid)
}

pub(crate) fn realloc_hook<'p, B: Backend + 'p>(_proj: &'p Project, state: &mut State<'p, B>, call: &'p instruction::Call) -> Result<ReturnValue<B::BV>> {
    assert_eq!(call.arguments.len(), 2);
    let addr = &call.arguments[0].0;
    let new_size = &call.arguments[1].0;
    match addr.get_type() {
        Type::PointerType { .. } => {},
        ty => return Err(Error::OtherError(format!("realloc_hook: expected first argument to be a pointer type, but got {:?}", ty))),
    };
    match new_size.get_type() {
        Type::IntegerType { .. } => {},
        ty => return Err(Error::OtherError(format!("realloc_hook: expected second argument to be an integer type, but got {:?}", ty))),
    };
    match call.get_type() {
        Type::PointerType { .. } => {},
        ty => return Err(Error::OtherError(format!("realloc_hook: expected return type to be a pointer type, but got {:?}", ty))),
    };

    let addr = alloc_utils::realloc(state, addr, new_size)?;
    Ok(ReturnValue::Return(addr))
}
