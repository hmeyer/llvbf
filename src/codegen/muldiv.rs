//! Multiplications and divisions: requests issued by segments and the shared
//! routines that service them.
//!
//! The routine bodies are large (hundreds of kilobytes for 32-bit division), so
//! each (kind, width) is emitted once, after the dispatch chain, like the memory
//! routines. A segment places the operands in the `ARG` cells, raises the routine's
//! request flag and ends; the routine moves the operands into a work area beyond
//! the current frame (the tape above the active frame is free), computes, and
//! leaves the results in `ARG`, from where the next segment moves them into the
//! destination slot.

use super::*;

impl<'a> Gen<'a> {
    fn arith_req(&self, kind: ArithKind, n: usize) -> Cell {
        let k = self
            .plan
            .arith_kinds
            .iter()
            .position(|x| *x == (kind, n))
            .expect("arithmetic kind");
        self.lay.req + (self.plan.access_kinds.len() + k) as Cell
    }

    /// Operands into `ARG`, raise the request, continue with the next segment.
    pub(super) fn emit_arith_request(
        &mut self,
        bb: &Block,
        op: ArithOp,
        seg: &Segment,
    ) -> Result<()> {
        let (a, b) = match &bb.instrs[op.instr].op {
            Op::Binary { a, b, .. } => (a, b),
            _ => bail!("internal error: arithmetic request on a non-binary instruction"),
        };
        let arg = self.lay.arg;
        let n = op.n;
        self.transfer_operand(a, 0, arg, n)?;
        self.transfer_operand(b, 0, arg + n as Cell, n)?;
        let req = self.arith_req(ArithKind::of(op.op).unwrap(), n);
        self.e.inc(req);
        self.set_pc(0, seg.id + 1);
        Ok(())
    }

    /// Emitted at the start of the segment after a request: the result (quotient or
    /// product in the first half of `ARG`, remainder in the second) into the slot.
    pub(super) fn finish_arith(&mut self, bb: &Block, op: ArithOp) -> Result<()> {
        let dest = bb.instrs[op.instr]
            .dest
            .ok_or_else(|| anyhow!("arithmetic without result"))?;
        let (cell, size) = self.slot(dest)?;
        let n = op.n;
        let arg = self.lay.arg;
        let (keep, drop) = if matches!(op.op, BinOp::URem | BinOp::SRem) {
            (arg + n as Cell, arg)
        } else {
            (arg, arg + n as Cell)
        };
        for i in 0..size {
            self.e.clear(cell + i as Cell);
        }
        for i in 0..n.min(size) {
            self.e.move_cell(keep + i as Cell, cell + i as Cell);
        }
        if ArithKind::of(op.op) != Some(ArithKind::Mul) {
            for i in 0..n {
                self.e.clear(drop + i as Cell);
            }
        }
        Ok(())
    }

    pub(super) fn emit_arith_routines(&mut self) {
        let kinds = self.plan.arith_kinds.clone();
        for (kind, n) in kinds {
            self.emit_arith_routine(kind, n);
        }
    }

    /// The shared routine for one (kind, width).
    fn emit_arith_routine(&mut self, kind: ArithKind, n: usize) {
        let lay = self.lay.clone();
        let req = self.arith_req(kind, n);
        let arg = lay.arg;
        self.comment(&format!("arithmetic routine: {:?} {} limbs", kind, n));
        self.e.begin_while(req);
        self.e.dec(req);
        // The work area starts at the scratch area of the (dead) frame above this
        // one. Its scratch cells are zero, but the routine may need more than the
        // frame's scratch holds, so the measured extent is cleared first.
        let ws_base = lay.frame + lay.scratch;
        for i in 0..self.arith_ws {
            self.e.clear(ws_base + i as Cell);
        }
        let mut ws = Scratch::new(ws_base);
        std::mem::swap(&mut self.sc, &mut ws);
        let wa = Work::alloc(&mut self.sc, n);
        let wb = Work::alloc(&mut self.sc, n);
        for i in 0..n {
            self.e.move_cell(arg + i as Cell, wa.limb(i));
            self.e.move_cell(arg + (n + i) as Cell, wb.limb(i));
        }
        match kind {
            ArithKind::Mul => {
                let res = Work::alloc(&mut self.sc, n);
                mul_work(&mut self.e, &mut self.sc, wa, wb, res);
                for i in 0..n {
                    self.e.move_cell(res.limb(i), arg + i as Cell);
                }
            }
            ArithKind::UDiv | ArithKind::SDiv => {
                let q = Work::alloc(&mut self.sc, n);
                let r = Work::alloc(&mut self.sc, n);
                if kind == ArithKind::UDiv {
                    udivrem(&mut self.e, &mut self.sc, wa, wb, q, r);
                } else {
                    sdivrem(&mut self.e, &mut self.sc, wa, wb, q, r);
                }
                for i in 0..n {
                    self.e.move_cell(q.limb(i), arg + i as Cell);
                    self.e.move_cell(r.limb(i), arg + (n + i) as Cell);
                }
            }
        }
        clear_work(&mut self.e, wa);
        clear_work(&mut self.e, wb);
        std::mem::swap(&mut self.sc, &mut ws);
        self.arith_hw = self.arith_hw.max(ws.used());
        self.e.end_while(req);
    }
}
