//! Terminators and phi copies.

use super::*;

impl<'a> Gen<'a> {
    fn block_id(&self, b: BlockId) -> u16 {
        self.fp().block_ids[b]
    }

    /// Emit the phi copies for the edge `from -> to`, then set the pc to `to`.
    ///
    /// Normally each phi is written straight from its incoming slot (a move when the
    /// incoming value dies here). If an incoming value is itself a phi of the target
    /// block the copies form a parallel assignment, so all values are read into
    /// scratch before any phi slot is written.
    fn branch_to(&mut self, from: BlockId, to: BlockId) -> Result<()> {
        let dest = &self.func().blocks[to];
        let mut edge: Vec<(Value, &Operand)> = Vec::with_capacity(dest.phis.len());
        for phi in &dest.phis {
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
            edge.push((phi.dest, val));
        }
        let targets: Vec<Value> = edge.iter().map(|(d, _)| *d).collect();
        let hazard = edge.iter().any(|(d, val)| {
            val.as_value()
                .is_some_and(|v| v != *d && targets.contains(&v))
        });
        let m = self.sc.mark();
        if hazard {
            let mut pending: Vec<(Value, Work)> = Vec::new();
            for (dest, val) in &edge {
                let (_, size) = self.slot(*dest)?;
                let w = Work::alloc(&mut self.sc, size);
                self.load_any(val, w)?;
                pending.push((*dest, w));
            }
            for (v, w) in pending {
                self.store_result(v, w)?;
            }
        } else {
            for (dest, val) in &edge {
                self.phi_copy(*dest, val)?;
            }
        }
        self.sc.release(m);
        let id = self.block_id(to);
        self.set_pc(0, id);
        Ok(())
    }

    /// Write one phi from its incoming operand without going through scratch.
    fn phi_copy(&mut self, dest: Value, val: &Operand) -> Result<()> {
        let (dcell, size) = self.slot(dest)?;
        match &val.kind {
            OperandKind::Value(v) if *v == dest => Ok(()), // loop-carried value unchanged
            OperandKind::Value(v) => {
                let (scell, ssize) = self.slot(*v)?;
                let n = size.min(ssize);
                let movable = self.consume_use(*v);
                for i in 0..size {
                    self.e.clear(dcell + i as Cell);
                }
                let tmp = self.lay.copy_tmp;
                for i in 0..n {
                    if movable {
                        self.e.move_cell(scell + i as Cell, dcell + i as Cell);
                    } else {
                        self.e.copy_cell(scell + i as Cell, dcell + i as Cell, tmp);
                    }
                }
                Ok(())
            }
            OperandKind::Const(c) => {
                let bytes = if self.types().is_scalar(val.ty) {
                    let x = self.ce().operand_scalar(val)?;
                    (0..size)
                        .map(|i| {
                            if i < 8 {
                                ((x >> (8 * i)) & 0xff) as u8
                            } else {
                                0
                            }
                        })
                        .collect::<Vec<u8>>()
                } else {
                    self.ce().bytes(c, val.ty)?
                };
                let tmp = self.lay.copy_tmp;
                for (i, &b) in bytes.iter().enumerate().take(size) {
                    self.e.set_using(dcell + i as Cell, b, tmp);
                }
                Ok(())
            }
        }
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
