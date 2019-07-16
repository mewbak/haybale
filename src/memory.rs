use z3::ast::{Ast, Array, BV};
use std::convert::TryInto;
use log::debug;

#[derive(Clone)]
pub struct Memory<'ctx> {
    ctx: &'ctx z3::Context,
    mem: Array<'ctx>,
    log_bits_in_byte_as_bv: BV<'ctx>,
}

impl<'ctx> Memory<'ctx> {
    pub const INDEX_BITS: u32 = 64;  // memory takes 64-bit indices
    pub const CELL_BITS: u32 = 64;  // memory "cells" are also 64-bit sized; we will mask if smaller operations are needed
    pub const BITS_IN_BYTE: u32 = 8;
    pub const LOG_BITS_IN_BYTE: u32 = 3;  // log base 2 of BITS_IN_BYTE
    pub const CELL_BYTES: u32 = Self::CELL_BITS / Self::BITS_IN_BYTE;  // how many bytes in a cell
    pub const LOG_CELL_BYTES: u32 = 3;  // log base 2 of CELL_BYTES. This many of the bottom index bits determine cell offset.

    pub fn new(ctx: &'ctx z3::Context) -> Self {
        let log_num_cells = Self::INDEX_BITS - Self::LOG_CELL_BYTES;  // 2 to this number gives the number of memory cells
        let domain = ctx.bitvector_sort(log_num_cells);
        let range = ctx.bitvector_sort(Self::CELL_BITS);
        Memory {
            ctx,
            mem: Array::new_const(&ctx, "mem", &domain, &range),
            log_bits_in_byte_as_bv: BV::from_u64(ctx, Self::LOG_BITS_IN_BYTE as u64, Self::INDEX_BITS),
        }
    }

    /// Read an entire cell from the given address.
    /// If address is not cell-aligned, this will give the entire cell _containing_ that address.
    fn read_cell(&self, addr: &BV<'ctx>) -> BV<'ctx> {
        assert_eq!(addr.get_size(), Self::INDEX_BITS);
        let cell_num = addr.extract(Self::INDEX_BITS-1, Self::LOG_CELL_BYTES);  // discard the cell offset
        self.mem.select(&cell_num.into()).try_into().unwrap()
    }

    /// Write an entire cell to the given address.
    /// If address is not cell-aligned, this will write to the cell _containing_ that address, which is probably not what you want.
    // TODO: to enforce concretization, we could just take a u64 address here
    fn write_cell(&mut self, addr: &BV<'ctx>, val: BV<'ctx>) {
        assert_eq!(addr.get_size(), Self::INDEX_BITS);
        assert_eq!(val.get_size(), Self::CELL_BITS);
        let cell_num = addr.extract(Self::INDEX_BITS-1, Self::LOG_CELL_BYTES);  // discard the cell offset
        self.mem = self.mem.store(&cell_num.into(), &val.into());
    }

    /// Read any number of bits of memory, at any alignment, but not crossing cell boundaries.
    /// Returned `BV` will have size `bits`.
    fn read_within_cell(&self, addr: &BV<'ctx>, bits: u32) -> BV<'ctx> {
        debug!("Reading within cell, {} bits at {}", bits, addr);
        let cell_contents = self.read_cell(addr);
        assert!(bits <= Self::CELL_BITS);
        let rval = if bits == Self::CELL_BITS {
            cell_contents  // shortcut to avoid more BV operations
                            // This assumes that `addr` was cell-aligned, but that must be the case if we're reading CELL_BITS bits and not crossing cell boundaries
        } else {
            let offset = addr.extract(Self::LOG_CELL_BYTES-1, 0)  // the actual offset part of the address
                .zero_ext(Self::CELL_BITS - Self::LOG_CELL_BYTES)  // zero-extend to CELL_BITS
                .bvshl(&self.log_bits_in_byte_as_bv);  // offset in bits rather than bytes

            // We can't `extract` at a non-const location, but we can shift by a non-const amount
            cell_contents.bvlshr(&offset)  // shift off whatever low-end bits we don't want
                .extract(bits - 1, 0)  // take just the bits we want, starting from 0
                .simplify()
        };
        debug!("Value read is {}", rval);
        rval
    }

    /// Write any number of bits of memory, at any alignment, but not crossing cell boundaries.
    // TODO: to enforce concretization, we could just take a `u64` address here
    fn write_within_cell(&mut self, addr: &BV<'ctx>, val: BV<'ctx>) {
        debug!("Writing within cell, {} to address {}", val, addr);
        let write_size = val.get_size();
        assert!(write_size <= Self::CELL_BITS);
        let data_to_write = if write_size == Self::CELL_BITS {
            val  // shortcut to avoid more BV operations
                // This assumes that `addr` was cell-aligned, but that must be the case if we're writing CELL_BITS bits and not crossing cell boundaries
        } else {
            let offset = addr.extract(Self::LOG_CELL_BYTES-1, 0)  // the actual offset part of the address
                .zero_ext(Self::CELL_BITS - Self::LOG_CELL_BYTES)  // zero-extend to CELL_BITS
                .bvshl(&self.log_bits_in_byte_as_bv);  // offset in bits rather than bytes

            // maskClear is 0's in the bit positions that will be written, 1's elsewhere.
            // We construct the inverse of this mask, then bitwise negate it.
            let mask_clear = BV::from_u64(self.ctx, 0, write_size)  // a bitvector of zeroes, of width equal to the width that will be written
                .bvnot()  // a bitvector of ones, of width equal to the width that will be written
                .zero_ext(Self::CELL_BITS - write_size)  // zero-extend to CELL_BITS
                .bvshl(&offset)  // now we have ones in the bit positions that will be written, zeroes elsewhere
                .bvnot();  // the final desired mask

            // mask_write is the write data in its appropriate bit positions, 0's elsewhere.
            let mask_write = val.zero_ext(Self::CELL_BITS - write_size).bvshl(&offset);

            self.read_cell(addr)
                .bvand(&mask_clear)  // zero out the section we'll be writing
                .bvor(&mask_write)  // write the data
                .simplify()
        };
        debug!("Final cell data being written is {}", data_to_write);
        self.write_cell(addr, data_to_write);
    }

    /// Read any number (>0) of bits of memory, at any alignment.
    /// Reads less than or equal to the cell size may not cross cell boundaries,
    ///   and reads more than the cell size must start at a cell boundary.
    /// Returned `BV` will have size `bits`.
    pub fn read(&self, addr: &BV<'ctx>, bits: u32) -> BV<'ctx> {
        debug!("Reading {} bits at {}", bits, addr);
        assert_ne!(bits, 0);
        let num_full_cells = (bits-1) / Self::CELL_BITS;  // this is bits / CELL_BITS, but if bits is a multiple of CELL_BITS, it undercounts by 1 (we treat this as N-1 full cells plus a "partial" cell of CELL_BITS bits)
        let bits_in_last_cell = (bits-1) % Self::CELL_BITS + 1;  // this is bits % CELL_BITS, but if bits is a multiple of CELL_BITS, then we get CELL_BITS rather than 0
        let reads = itertools::repeat_n(Self::CELL_BITS, num_full_cells.try_into().unwrap())
            .chain(std::iter::once(bits_in_last_cell))  // this forms the sequence of read sizes
            .enumerate()
            .map(|(i,sz)| {
                let offset_bytes = i as u64 * u64::from(Self::CELL_BYTES);
                self.read_within_cell(&addr.bvadd(&BV::from_u64(self.ctx, offset_bytes, Self::INDEX_BITS)), sz)
            });
        fold_from_first(reads, |a,b| b.concat(&a)).simplify()
    }

    /// Write any number (>0) of bits of memory, at any alignment.
    /// Writes less than or equal to the cell size may not cross cell boundaries,
    ///   and writes more than the cell size must start at a cell boundary.
    pub fn write(&mut self, addr: &BV<'ctx>, val: BV<'ctx>) {
        debug!("Writing {} to address {}", val, addr);
        let write_size = val.get_size();
        assert_ne!(write_size, 0);
        let num_full_cells = (write_size-1) / Self::CELL_BITS;  // this is bits / CELL_BITS, but if bits is a multiple of CELL_BITS, it undercounts by 1 (we treat this as N-1 full cells plus a "partial" cell of CELL_BITS bits)
        let bits_in_last_cell = (write_size-1) % Self::CELL_BITS + 1;  // this is bits % CELL_BITS, but if bits is a multiple of CELL_BITS, then we get CELL_BITS rather than 0
        let write_size_sequence = itertools::repeat_n(Self::CELL_BITS, num_full_cells.try_into().unwrap())
            .chain(std::iter::once(bits_in_last_cell));
        for (i,sz) in write_size_sequence.enumerate() {
            assert!(sz > 0);
            let offset_bytes = i as u64 * u64::from(Self::CELL_BYTES);
            let offset_bits = i as u32 * Self::CELL_BITS;
            let write_data = val.extract(sz + offset_bits - 1, offset_bits);
            self.write_within_cell(&addr.bvadd(&BV::from_u64(self.ctx, offset_bytes, Self::INDEX_BITS)), write_data);
        }
    }
}

/// Utility function that `fold()`s an iterator using the iterator's first item as starting point.
/// Panics if the iterator has zero items.
fn fold_from_first<F, T>(mut i: impl Iterator<Item = T>, f: F) -> T
    where F: FnMut(T, T) -> T
{
    let first = i.next().expect("fold_from_first was passed an iterator with zero items");
    i.fold(first, f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solver::Solver;

    #[test]
    fn read_and_write_to_cell_zero() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store data to address 0
        let data_val = 0x12345678;
        let data = BV::from_u64(&ctx, data_val, Memory::CELL_BITS);
        let zero = BV::from_u64(&ctx, 0, Memory::INDEX_BITS);
        mem.write(&zero, data);

        // Ensure that we can read it back again
        let read_bv = mem.read(&zero, Memory::CELL_BITS);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, data_val);
    }

    #[test]
    fn read_and_write_cell_aligned() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store data to a nonzero, but aligned, address
        let data_val = 0x12345678;
        let data = BV::from_u64(&ctx, data_val, Memory::CELL_BITS);
        let aligned = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        mem.write(&aligned, data);

        // Ensure that we can read it back again
        let read_bv = mem.read(&aligned, Memory::CELL_BITS);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, data_val);
    }

    #[test]
    fn read_and_write_small() {
        let _ = env_logger::builder().is_test(true).try_init();
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store 8 bits of data
        let data_val = 0x4F;
        let data = BV::from_u64(&ctx, data_val, 8);
        let addr = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        mem.write(&addr, data);

        // Ensure that we can read it back again
        let read_bv = mem.read(&addr, 8);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, data_val);
    }

    #[test]
    fn read_and_write_unaligned() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store 8 bits of data to offset 1 in a cell
        let data_val = 0x4F;
        let data = BV::from_u64(&ctx, data_val, 8);
        let unaligned = BV::from_u64(&ctx, 0x10001, Memory::INDEX_BITS);
        mem.write(&unaligned, data.clone());

        // Ensure that we can read it back again
        let read_bv = mem.read(&unaligned, 8);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, data_val);
    }

    #[test]
    fn read_and_write_twocells() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store two cells' worth of data
        let data_val_0: u64 = 0x12345678_9abcdef0;
        let data_val_1: u64 = 0x2468ace0_13579bdf;
        let write_val = BV::from_u64(&ctx, data_val_1, 64).concat(&BV::from_u64(&ctx, data_val_0, 64));
        assert_eq!(write_val.get_size(), 2*Memory::CELL_BITS);
        let addr = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        mem.write(&addr, write_val);

        // Ensure that we can read it back again
        let read_bv = mem.read(&addr, 128);
        let read_val_0 = solver.get_a_solution_for_specified_bits_of_bv(&read_bv, 63, 0).unwrap();
        assert_eq!(read_val_0, data_val_0, "\nGot value 0x{:x}, expected 0x{:x}", read_val_0, data_val_0);
        let read_val_1 = solver.get_a_solution_for_specified_bits_of_bv(&read_bv, 127, 64).unwrap();
        assert_eq!(read_val_1, data_val_1);
    }

    #[test]
    fn read_and_write_200bits() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store 200 bits of data
        let data_val_0: u64 = 0x12345678_9abcdef0;
        let data_val_1: u64 = 0x2468ace0_13579bdf;
        let data_val_2: u64 = 0xfedcba98_76543210;
        let data_val_3: u64 = 0xef;
        let write_val = BV::from_u64(&ctx, data_val_3, 8)
            .concat(&BV::from_u64(&ctx, data_val_2, 64))
            .concat(&BV::from_u64(&ctx, data_val_1, 64))
            .concat(&BV::from_u64(&ctx, data_val_0, 64));
        assert_eq!(write_val.get_size(), 200);
        let addr = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        mem.write(&addr, write_val);

        // Ensure that we can read it back again
        let read_bv = mem.read(&addr, 200);
        let read_val_0 = solver.get_a_solution_for_specified_bits_of_bv(&read_bv, 63, 0).unwrap();
        assert_eq!(read_val_0, data_val_0);
        let read_val_1 = solver.get_a_solution_for_specified_bits_of_bv(&read_bv, 127, 64).unwrap();
        assert_eq!(read_val_1, data_val_1);
        let read_val_2 = solver.get_a_solution_for_specified_bits_of_bv(&read_bv, 191, 128).unwrap();
        assert_eq!(read_val_2, data_val_2);
        let read_val_3 = solver.get_a_solution_for_specified_bits_of_bv(&read_bv, 199, 192).unwrap();
        assert_eq!(read_val_3, data_val_3);
    }

    #[test]
    fn write_small_read_big() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store 8 bits of data to offset 1 in a cell
        let data_val = 0x4F;
        let data = BV::from_u64(&ctx, data_val, 8);
        let unaligned = BV::from_u64(&ctx, 0x10001, Memory::INDEX_BITS);
        mem.write(&unaligned, data.clone());

        // Ensure that reading from beginning of the cell adds zeroed low-order bits
        // (we are little-endian)
        let aligned = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        let read_bv = mem.read(&aligned, 16);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x4F00);

        // Ensure that reading extra bits adds zeroed high-order bits
        let read_bv = mem.read(&unaligned, 16);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x004F);

        // Ensure that reading elsewhere gives all zeroes
        let garbage_addr_1 = BV::from_u64(&ctx, 0x10004, Memory::INDEX_BITS);
        let garbage_addr_2 = BV::from_u64(&ctx, 0x10008, Memory::INDEX_BITS);
        let read_bv_1 = mem.read(&garbage_addr_1, 8);
        let read_bv_2 = mem.read(&garbage_addr_2, 8);
        let read_val_1 = solver.get_a_solution_for_bv(&read_bv_1).unwrap();
        let read_val_2 = solver.get_a_solution_for_bv(&read_bv_2).unwrap();
        assert_eq!(read_val_1, 0);
        assert_eq!(read_val_2, 0);
    }

    #[test]
    fn write_big_read_small() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Store 32 bits of data to offset 2 in a cell
        let data_val = 0x12345678;
        let data = BV::from_u64(&ctx, data_val, 32);
        let offset_2 = BV::from_u64(&ctx, 0x10002, Memory::INDEX_BITS);
        mem.write(&offset_2, data.clone());

        // Ensure that reading 8 bits from offset 2 gives the low-order byte
        // (we are little-endian)
        let read_bv = mem.read(&offset_2, 8);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x78);

        // Ensure that reading 8 bits from offset 5 gives the high-order byte
        // (we are little-endian)
        let offset_5 = BV::from_u64(&ctx, 0x10005, Memory::INDEX_BITS);
        let read_bv = mem.read(&offset_5, 8);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x12);

        // Ensure that reading 16 bits from offset 3 gives the middle two bytes
        let offset_3 = BV::from_u64(&ctx, 0x10003, Memory::INDEX_BITS);
        let read_bv = mem.read(&offset_3, 16);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x3456);
    }

    #[test]
    fn partial_overwrite_aligned() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Write an entire cell
        let data = BV::from_u64(&ctx, 0x12345678_12345678, Memory::CELL_BITS);
        let addr = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        mem.write(&addr, data);

        // Write over just the first part
        let overwrite_data_val = 0xdcba;
        let overwrite_data = BV::from_u64(&ctx, overwrite_data_val, 16);
        mem.write(&addr, overwrite_data);

        // Ensure that we can read the smaller overwrite back
        let read_bv = mem.read(&addr, 16);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, overwrite_data_val);

        // Ensure that reading the whole cell back reflects the partial overwrite
        let read_bv = mem.read(&addr, Memory::CELL_BITS);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x12345678_1234dcba);
    }

    #[test]
    fn partial_overwrite_unaligned() {
        let ctx = z3::Context::new(&z3::Config::new());
        let mut mem = Memory::new(&ctx);
        let mut solver = Solver::new(&ctx);

        // Write an entire cell
        let data = BV::from_u64(&ctx, 0x12345678_12345678, Memory::CELL_BITS);
        let addr = BV::from_u64(&ctx, 0x10000, Memory::INDEX_BITS);
        mem.write(&addr, data);

        // Write over just part of the middle
        let overwrite_addr = BV::from_u64(&ctx, 0x10002, Memory::INDEX_BITS);
        let overwrite_data_val = 0xdcba;
        let overwrite_data = BV::from_u64(&ctx, overwrite_data_val, 16);
        mem.write(&overwrite_addr, overwrite_data);

        // Ensure that we can read the smaller overwrite back
        let read_bv = mem.read(&overwrite_addr, 16);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, overwrite_data_val);

        // Ensure that reading the whole cell back reflects the partial overwrite
        let read_bv = mem.read(&addr, Memory::CELL_BITS);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x12345678_dcba5678);

        // Now a different partial read with some original data and some overwritten
        let new_addr = BV::from_u64(&ctx, 0x10003, Memory::INDEX_BITS);
        let read_bv = mem.read(&new_addr, 16);
        let read_val = solver.get_a_solution_for_bv(&read_bv).unwrap();
        assert_eq!(read_val, 0x78dc);
    }
}