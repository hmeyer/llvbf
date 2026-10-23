//! Memory accesses: requests issued by segments and the shared routines that service them.

use super::*;

impl<'a> Gen<'a> {
    fn access_kind_index(&self, is_store: bool, width: usize) -> Cell {
        self.plan
            .access_kinds
            .iter()
            .position(|k| *k == (is_store, width))
            .expect("access kind") as Cell
    }

    /// Emitted at the start of the segment after a load request: move the bytes the
    /// memory routine delivered in `DATA` into the destination slot.
    pub(super) fn finish_load(&mut self, block: usize, bb: &Block, a: Access) -> Result<()> {
        let inst = &bb.instrs[a.instr];
        let (cell, size) = match a.role {
            AccessRole::Plain | AccessRole::VaValue => {
                self.slot(inst.dest.ok_or_else(|| anyhow!("load without result"))?)?
            }
            AccessRole::VaPtrLoad | AccessRole::VaCopyLoad => self.extra_slot(block, a.instr)?,
            _ => bail!("internal error: finish_load on a store"),
        };
        if a.off + a.width > size {
            bail!("load chunk out of bounds");
        }
        for k in 0..a.width {
            let dst = cell + (a.off + k) as Cell;
            self.e.clear(dst);
            self.e.move_cell(self.lay.data + k as Cell, dst);
        }
        Ok(())
    }

    /// Issue a memory request: address (and data for stores) into the header cells,
    /// set the request flag, and continue with the next segment.
    pub(super) fn emit_access(
        &mut self,
        block: usize,
        bb: &Block,
        a: Access,
        seg: &Segment,
    ) -> Result<()> {
        let lay = self.lay.clone();
        let n = self.ptr_limbs();
        let inst = &bb.instrs[a.instr];
        self.comment(&format!(
            "memory {} of {} bytes at offset {} ({:?})",
            if a.is_store { "store" } else { "load" },
            a.width,
            a.off,
            a.role
        ));
        let m = self.sc.mark();
        // Address: which operand holds the base pointer depends on the role.
        let w = Work::alloc(&mut self.sc, n);
        match (a.role, &inst.op) {
            (AccessRole::Plain, Op::Load { addr, .. })
            | (AccessRole::Plain, Op::Store { addr, .. }) => self.load_operand(addr, w)?,
            (AccessRole::VaPtrLoad, Op::VaArg { list })
            | (AccessRole::VaPtrStore, Op::VaArg { list }) => self.load_operand(list, w)?,
            (AccessRole::VaValue, _) => {
                let (cell, _) = self.extra_slot(block, a.instr)?;
                load_packed(&mut self.e, &mut self.sc, cell, w);
            }
            (AccessRole::VaStart, Op::Call { args, .. })
            | (AccessRole::VaCopyStore, Op::Call { args, .. }) => self.load_operand(&args[0], w)?,
            (AccessRole::VaCopyLoad, Op::Call { args, .. }) => self.load_operand(&args[1], w)?,
            (AccessRole::VaSpill(_), _) => load_packed(&mut self.e, &mut self.sc, lay.msp, w),
            _ => bail!("internal error: access role does not match instruction"),
        }
        if a.off != 0 {
            add_const(&mut self.e, &mut self.sc, w, a.off as u64);
        }
        self.e.clear(lay.addr_lo);
        self.e.clear(lay.addr_hi);
        self.e.move_cell(w.limb(0), lay.addr_lo);
        if w.n > 1 {
            self.e.move_cell(w.limb(1), lay.addr_hi);
        }
        clear_work(&mut self.e, w);
        self.sc.release(m);
        // Data for stores.
        if a.is_store {
            let m = self.sc.mark();
            let dw = Work::alloc(&mut self.sc, a.width);
            match (a.role, &inst.op) {
                (AccessRole::Plain, Op::Store { value, .. }) => {
                    self.load_operand_bytes(value, a.off, dw)?
                }
                (AccessRole::VaPtrStore, _) => {
                    // The advanced argument pointer: saved pointer + size of the argument slot.
                    let (cell, _) = self.extra_slot(block, a.instr)?;
                    let full = Work::alloc(&mut self.sc, n);
                    load_packed(&mut self.e, &mut self.sc, cell, full);
                    let (adv, slot_align) = va_slot(self.types(), inst.ty);
                    if slot_align > self.ptr_limbs() {
                        bail!("va_arg of an over-aligned type is not supported on this target");
                    }
                    add_const(&mut self.e, &mut self.sc, full, adv as u64);
                    for k in 0..a.width {
                        self.e.move_cell(full.limb(a.off + k), dw.limb(k));
                    }
                    clear_work(&mut self.e, full);
                }
                (AccessRole::VaStart, _) => {
                    let s = self
                        .fp()
                        .va_slot
                        .ok_or_else(|| anyhow!("va_start in non-variadic function"))?;
                    let cell = self.slot_cell(&s);
                    load_packed(&mut self.e, &mut self.sc, cell + a.off as Cell, dw);
                }
                (AccessRole::VaCopyStore, _) => {
                    let (cell, _) = self.extra_slot(block, a.instr)?;
                    load_packed(&mut self.e, &mut self.sc, cell + a.off as Cell, dw);
                }
                (AccessRole::VaSpill(k), Op::Call { fn_ty, args, .. }) => {
                    let (layout, _) = va_spill_layout(self.types(), *fn_ty, args)?;
                    let fixed = args.len() - layout.len();
                    let (aoff, _) = layout[k];
                    let arg = &args[fixed + k];
                    if self.types().is_scalar(arg.ty)
                        && a.off == aoff
                        && a.width >= self.types().size(arg.ty)
                    {
                        self.load_operand(arg, dw)?;
                    } else {
                        self.load_operand_bytes(arg, a.off - aoff, dw)?;
                    }
                }
                _ => bail!("internal error: store role does not match instruction"),
            }
            for k in 0..a.width {
                self.e.clear(lay.data + k as Cell);
                self.e.move_cell(dw.limb(k), lay.data + k as Cell);
            }
            self.sc.release(m);
        }
        let req = lay.req + self.access_kind_index(a.is_store, a.width);
        self.e.set(req, 1);
        self.set_pc(0, seg.id + 1);
        Ok(())
    }

    /// Move the given cells to the same offsets in the region `stride` cells away
    /// (the next frame or memory block). The caller re-anchors the origin afterwards.
    fn hop(&mut self, stride: Cell, cells: &[Cell]) {
        for &c in cells {
            self.e.move_cell(c, c + stride);
        }
    }

    pub(super) fn emit_memory_routines(&mut self) {
        let kinds = self.plan.access_kinds.clone();
        for (k, (is_store, width)) in kinds.iter().enumerate() {
            self.emit_memory_routine(k as Cell, *is_store, *width);
        }
    }

    /// The shared routine servicing all accesses of one (kind, width).
    fn emit_memory_routine(&mut self, k: Cell, is_store: bool, width: usize) {
        let lay = self.lay.clone();
        let f = lay.frame;
        let b = lay.block;
        let req = lay.req + k;
        let data: Vec<Cell> = (0..width as Cell).map(|i| lay.data + i).collect();
        self.comment(&format!(
            "memory routine: {} {} bytes",
            if is_store { "store" } else { "load" },
            width
        ));
        self.e.begin_while(req);
        self.e.dec(req);
        self.e.inc(lay.home);
        // Hop down through the active frames to the guard frame.
        let mut transit = vec![lay.addr_lo, lay.addr_hi];
        if is_store {
            transit.extend(&data);
        }
        self.e.begin_while(lay.active);
        self.hop(-f, &transit);
        self.e.goto(lay.active - f);
        self.e.set_pos(lay.active);
        self.e.end_while(lay.active);
        // Guard frame -> block 0 control area.
        self.hop(-b, &transit);
        let p = self.e.pos();
        self.e.set_pos(p + b);
        // Hop blocks by the high address byte, counting hops.
        let mut transit_blk = vec![lay.addr_lo, lay.addr_hi, lay.hops];
        if is_store {
            transit_blk.extend(&data);
        }
        self.e.begin_while(lay.addr_hi);
        self.e.dec(lay.addr_hi);
        self.e.inc(lay.hops);
        self.hop(-b, &transit_blk);
        self.e.goto(lay.addr_hi - b);
        self.e.set_pos(lay.addr_hi);
        self.e.end_while(lay.addr_hi);
        // Walk the control block {addr_lo, cnt, t, data[0..width]} right through the
        // block's data cells, one cell per unit of the low address byte. Each step
        // rotates the displaced data cell to the left of the block, so walking back
        // the same number of steps restores the memory contents.
        let k = 3 + width as Cell; // size of the walking block
        let wb_start = lay.addr_lo;
        debug_assert_eq!(lay.cnt, wb_start + 1);
        debug_assert_eq!(lay.t, wb_start + 2);
        debug_assert_eq!(lay.data, wb_start + 3);
        debug_assert_eq!(lay.ctl, lay.data + 8);
        let block_end = wb_start + k; // first cell after the walking block
                                      // Move the block so that it ends right before data cell 0: shift by (ctl - block_end).
        let pre = lay.ctl - block_end;
        let cells: Vec<Cell> = (0..k).map(|i| wb_start + i).collect();
        if pre > 0 {
            for &c in cells.iter().rev() {
                self.e.move_cell(c, c + pre);
            }
            let p = self.e.pos();
            self.e.set_pos(p - pre);
        }
        // Now the walking block occupies [wb_start, block_end) and data cell 0 is at block_end.
        self.e.begin_while(wb_start);
        self.e.dec(wb_start);
        self.e.inc(lay.cnt);
        self.e.move_cell(block_end, lay.t); // displaced data cell -> t
        for &c in cells.iter().rev() {
            self.e.move_cell(c, c + 1);
        }
        self.e.move_cell(lay.t + 1, wb_start); // displaced value into the vacated cell
        self.e.goto(wb_start + 1);
        self.e.set_pos(wb_start);
        self.e.end_while(wb_start);
        // The addressed cells are block_end .. block_end + width.
        for i in 0..width as Cell {
            let mem = block_end + i;
            if is_store {
                self.e.clear(mem);
                self.e.move_cell(lay.data + i, mem);
            } else {
                self.e.copy_cell(mem, lay.data + i, lay.t);
            }
        }
        // Walk back.
        self.e.begin_while(lay.cnt);
        self.e.dec(lay.cnt);
        self.e.move_cell(wb_start - 1, lay.t); // displaced value -> t
        for &c in cells.iter() {
            self.e.move_cell(c, c - 1);
        }
        self.e.move_cell(lay.t - 1, block_end - 1); // back to its cell (now right after the shifted block)
        self.e.goto(lay.cnt - 1);
        self.e.set_pos(lay.cnt);
        self.e.end_while(lay.cnt);
        if pre > 0 {
            for &c in cells.iter() {
                self.e.move_cell(c, c - pre);
            }
            let p = self.e.pos();
            self.e.set_pos(p + pre);
        }
        // Hop back up the blocks.
        let mut back = vec![lay.hops];
        if !is_store {
            back.extend(&data);
        }
        self.e.begin_while(lay.hops);
        self.e.dec(lay.hops);
        self.hop(b, &back);
        self.e.goto(lay.hops + b);
        self.e.set_pos(lay.hops);
        self.e.end_while(lay.hops);
        // Block 0 -> guard frame.
        let back_g: Vec<Cell> = if is_store { vec![] } else { data.clone() };
        self.hop(b, &back_g);
        let p = self.e.pos();
        self.e.set_pos(p - b);
        // Hop up through the frames until the home frame.
        self.e.inc(lay.nothome);
        self.e.begin_while(lay.nothome);
        self.e.dec(lay.nothome);
        self.hop(f, &back_g);
        self.e.goto(lay.nothome + f);
        self.e.set_pos(lay.nothome);
        self.e.begin_if_zero(lay.home);
        self.e.inc(lay.nothome);
        self.e.end_if_zero(lay.home);
        self.e.end_while(lay.nothome);
        self.e.dec(lay.home);
        self.e.end_while(req);
    }
}
