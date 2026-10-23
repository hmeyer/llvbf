//! Terminators and phi copies.

use super::*;

impl<'a> Gen<'a> {
    fn block_id(&self, b: BlockId) -> u16 {
        self.fp().block_ids[b]
    }

    /// Emit the phi moves for the edge `from -> to`, then set the pc to `to`.
    /// All incoming values are read into scratch before any phi slot is written,
    /// which makes the copies parallel (phis may read each other's old values).
    fn branch_to(&mut self, from: BlockId, to: BlockId) -> Result<()> {
        let dest = &self.func().blocks[to];
        let m = self.sc.mark();
        let mut pending: Vec<(Value, Work)> = Vec::new();
        for phi in &dest.phis {
            let (_, size) = self.slot(phi.dest)?;
            let w = Work::alloc(&mut self.sc, size);
            let (val, _) = phi
                .incoming
                .iter()
                .find(|(_, b)| *b == from)
                .ok_or_else(|| {
                    anyhow!(
                        "phi {} has no incoming value from %{}",
                        phi.dest,
                        self.func().blocks[from].name
                    )
                })?;
            self.load_any(val, w)?;
            pending.push((phi.dest, w));
        }
        for (v, w) in pending {
            self.store_result(v, w)?;
        }
        self.sc.release(m);
        let id = self.block_id(to);
        self.set_pc(0, id);
        Ok(())
    }

    pub(super) fn emit_terminator(&mut self, block: BlockId, bb: &Block) -> Result<()> {
        match &bb.term {
            Terminator::Ret(v) => self.emit_ret(v),
            Terminator::Br(dest) => self.branch_to(block, *dest),
            Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } => {
                let m = self.sc.mark();
                let c = Work::alloc(&mut self.sc, 1);
                self.load_operand(cond, c)?;
                let flag = self.sc.alloc(1);
                self.e.begin_if_else(c.limb(0), flag);
                self.branch_to(block, *if_false)?;
                self.e.else_if_else(c.limb(0), flag);
                self.branch_to(block, *if_true)?;
                self.e.end_if_else(flag);
                self.e.clear(c.limb(0));
                self.sc.release(m);
                Ok(())
            }
            Terminator::Switch {
                value,
                cases,
                default,
            } => {
                let n = limbs(self.op_bits(value)?);
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, n);
                self.load_operand(value, w)?;
                let done = Work::alloc(&mut self.sc, 1);
                for (c, dest) in cases {
                    let m2 = self.sc.mark();
                    let cw = Work::alloc(&mut self.sc, n);
                    load_const(&mut self.e, cw, *c);
                    let flag = Work::alloc(&mut self.sc, 1);
                    eq(&mut self.e, &mut self.sc, w, cw, flag.limb(0));
                    clear_work(&mut self.e, cw);
                    let f2 = self.sc.alloc(1);
                    self.e.begin_if_nonzero(flag.limb(0), f2);
                    self.e.inc(done.limb(0));
                    self.branch_to(block, *dest)?;
                    self.e.end_if_nonzero(f2);
                    self.e.clear(flag.limb(0));
                    self.sc.release(m2);
                }
                self.e.begin_if_zero(done.limb(0));
                self.branch_to(block, *default)?;
                self.e.end_if_zero(done.limb(0));
                self.e.clear(done.limb(0));
                clear_work(&mut self.e, w);
                self.sc.release(m);
                Ok(())
            }
            Terminator::Unreachable => {
                self.e.clear(self.lay.active);
                Ok(())
            }
        }
    }
}
