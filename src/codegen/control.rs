//! Terminators and phi copies.

use super::*;

impl<'a> Gen<'a> {
    pub(super) fn block_id(&self, name: &Name) -> Result<u16> {
        self.fp()
            .block_ids
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("unknown block %{}", name))
    }

    /// Emit the phi moves for the edge `from -> to`, then set the pc to `to`.
    pub(super) fn branch_to(&mut self, from: &Name, to: &Name) -> Result<()> {
        let f = self.func();
        let dest = f
            .basic_blocks
            .iter()
            .find(|b| &b.name == to)
            .ok_or_else(|| anyhow!("unknown block %{}", to))?;
        let m = self.sc.mark();
        let mut pending: Vec<(Name, Work)> = Vec::new();
        for inst in &dest.instrs {
            if let Instruction::Phi(phi) = inst {
                let (_, size) = self.slot(&phi.dest)?;
                let w = Work::alloc(&mut self.sc, size);
                let (val, _) = phi
                    .incoming_values
                    .iter()
                    .find(|(_, b)| b == from)
                    .ok_or_else(|| {
                        anyhow!("phi %{} has no incoming value from %{}", phi.dest, from)
                    })?;
                if self.ti.is_scalar(&phi.to_type) {
                    self.load_operand(val, w)?;
                } else {
                    self.load_operand_bytes(val, 0, w)?;
                }
                pending.push((phi.dest.clone(), w));
            } else {
                break;
            }
        }
        for (name, w) in pending {
            self.store_result(&name, w)?;
        }
        self.sc.release(m);
        let id = self.block_id(to)?;
        self.set_pc(0, id);
        Ok(())
    }

    pub(super) fn emit_terminator(
        &mut self,
        bb: &llvm_ir::BasicBlock,
        _seg: &Segment,
    ) -> Result<()> {
        match &bb.term {
            Terminator::Ret(r) => self.emit_ret(r),
            Terminator::Br(br) => self.branch_to(&bb.name, &br.dest),
            Terminator::CondBr(cb) => {
                let m = self.sc.mark();
                let c = Work::alloc(&mut self.sc, 1);
                self.load_operand(&cb.condition, c)?;
                let flag = self.sc.alloc(1);
                self.e.begin_if_else(c.limb(0), flag);
                self.branch_to(&bb.name, &cb.false_dest)?;
                self.e.else_if_else(c.limb(0), flag);
                self.branch_to(&bb.name, &cb.true_dest)?;
                self.e.end_if_else(flag);
                self.e.clear(c.limb(0));
                self.sc.release(m);
                Ok(())
            }
            Terminator::Switch(sw) => {
                let bits = self.op_bits(&sw.operand)?;
                let n = limbs(bits);
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, n);
                self.load_operand(&sw.operand, w)?;
                let done = Work::alloc(&mut self.sc, 1);
                for (c, dest) in &sw.dests {
                    let m2 = self.sc.mark();
                    let cw = Work::alloc(&mut self.sc, n);
                    let v = self.ce().scalar(c)?;
                    load_const(&mut self.e, cw, v);
                    let flag = Work::alloc(&mut self.sc, 1);
                    eq(&mut self.e, &mut self.sc, w, cw, flag.limb(0));
                    clear_work(&mut self.e, cw);
                    let f2 = self.sc.alloc(1);
                    self.e.begin_if_nonzero(flag.limb(0), f2);
                    self.e.inc(done.limb(0));
                    self.branch_to(&bb.name, dest)?;
                    self.e.end_if_nonzero(f2);
                    self.e.clear(flag.limb(0));
                    self.sc.release(m2);
                }
                self.e.begin_if_zero(done.limb(0));
                self.branch_to(&bb.name, &sw.default_dest)?;
                self.e.end_if_zero(done.limb(0));
                self.e.clear(done.limb(0));
                clear_work(&mut self.e, w);
                self.sc.release(m);
                Ok(())
            }
            Terminator::Unreachable(_) => {
                self.e.clear(self.lay.active);
                Ok(())
            }
            t => bail!("unsupported terminator {}", t),
        }
    }
}
