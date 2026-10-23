//! Lowering of ordinary instructions.

use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Cast {
    ZExt,
    SExt,
    Trunc,
}

impl<'a> Gen<'a> {
    pub(super) fn const_of(&self, op: &Operand) -> Option<u64> {
        if op.as_const().is_some() {
            self.ce().operand_scalar(op).ok()
        } else {
            None
        }
    }

    pub(super) fn emit_instr(&mut self, block: usize, bb: &Block, i: usize) -> Result<()> {
        let inst = &bb.instrs[i];
        let dest = inst.dest;
        match &inst.op {
            Op::Binary { kind, a, b } => self.binop(a, b, dest.unwrap(), *kind),
            Op::ICmp { pred, a, b } => self.icmp(*pred, a, b, dest.unwrap()),
            Op::Cast { kind, x } => {
                let c = match kind {
                    CastKind::ZExt | CastKind::Reinterpret => Cast::ZExt,
                    CastKind::SExt => Cast::SExt,
                    CastKind::Trunc => Cast::Trunc,
                };
                self.cast(x, inst.ty, dest.unwrap(), c)
            }
            Op::Freeze { x } => self.cast(x, inst.ty, dest.unwrap(), Cast::ZExt),
            Op::Select {
                cond,
                if_true,
                if_false,
            } => self.select(cond, if_true, if_false, dest.unwrap()),
            Op::Gep {
                base,
                source_ty,
                indices,
            } => self.gep(base, *source_ty, indices, dest.unwrap()),
            Op::Alloca { ty, count, align } => {
                self.alloca(block, *ty, *count, *align, dest.unwrap())
            }
            Op::ExtractValue { agg, indices } => self.extract_value(agg, indices, dest.unwrap()),
            Op::InsertValue { agg, elem, indices } => {
                self.insert_value(agg, elem, indices, dest.unwrap())
            }
            Op::Call { .. } => self.inline_call(inst),
            // Handled as segment ends.
            Op::Load { .. } | Op::Store { .. } | Op::VaArg { .. } => Ok(()),
        }
    }

    fn binop(&mut self, a: &Operand, b: &Operand, dest: Value, op: BinOp) -> Result<()> {
        let bits = self.op_bits(a)?;
        let n = limbs(bits);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        self.load_operand(a, wa)?;
        let cb = self.const_of(b);
        let mut result = wa;
        match op {
            BinOp::Add | BinOp::Sub => {
                if let Some(c) = cb {
                    let v = if op == BinOp::Sub {
                        c.wrapping_neg()
                    } else {
                        c
                    };
                    add_const(&mut self.e, &mut self.sc, wa, v);
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    if op == BinOp::Add {
                        add_work(&mut self.e, wa, wb);
                    } else {
                        sub_work(&mut self.e, wa, wb);
                    }
                }
            }
            BinOp::Mul => {
                let res = Work::alloc(&mut self.sc, n);
                if let Some(c) = cb {
                    mul_const(&mut self.e, &mut self.sc, wa, c, res);
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    mul_work(&mut self.e, &mut self.sc, wa, wb, res);
                }
                result = res;
            }
            BinOp::UDiv | BinOp::URem | BinOp::SDiv | BinOp::SRem => {
                let wb = Work::alloc(&mut self.sc, n);
                self.load_operand(b, wb)?;
                let q = Work::alloc(&mut self.sc, n);
                let r = Work::alloc(&mut self.sc, n);
                if matches!(op, BinOp::UDiv | BinOp::URem) {
                    udivrem(&mut self.e, &mut self.sc, wa, wb, q, r);
                } else {
                    sdivrem(&mut self.e, &mut self.sc, wa, wb, q, r);
                }
                if matches!(op, BinOp::UDiv | BinOp::SDiv) {
                    clear_work(&mut self.e, r);
                    result = q;
                } else {
                    clear_work(&mut self.e, q);
                    result = r;
                }
            }
            BinOp::And | BinOp::Or | BinOp::Xor => {
                let kind = match op {
                    BinOp::And => 0,
                    BinOp::Or => 1,
                    _ => 2,
                };
                if let Some(c) = cb {
                    bitwise_const(&mut self.e, &mut self.sc, wa, c, kind);
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    bitwise(&mut self.e, &mut self.sc, wa, wb, kind);
                }
            }
            BinOp::Shl | BinOp::LShr | BinOp::AShr => {
                if let Some(c) = cb {
                    let k = c as u32;
                    match op {
                        BinOp::Shl => shl_const(&mut self.e, &mut self.sc, wa, k),
                        BinOp::LShr => lshr_const(&mut self.e, &mut self.sc, wa, k),
                        _ => ashr_const(&mut self.e, &mut self.sc, wa, k),
                    }
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    let kind = match op {
                        BinOp::Shl => 0,
                        BinOp::LShr => 1,
                        _ => 2,
                    };
                    shift_var(&mut self.e, &mut self.sc, wa, wb, kind);
                }
            }
        }
        if bits % 8 != 0 {
            trunc_bits(&mut self.e, &mut self.sc, result, bits);
        }
        self.store_result(dest, result)?;
        self.sc.release(m);
        Ok(())
    }

    fn icmp(&mut self, pred: Pred, a: &Operand, b: &Operand, dest: Value) -> Result<()> {
        let bits = self.op_bits(a)?;
        let n = limbs(bits);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        let wb = Work::alloc(&mut self.sc, n);
        self.load_operand(a, wa)?;
        self.load_operand(b, wb)?;
        let flag = Work::alloc(&mut self.sc, 1);
        let fc = flag.limb(0);
        if pred.is_signed() {
            if bits % 8 != 0 {
                // Sign bit of a sub-byte value (i1): move it to the top of the limb.
                let k = 8 - (bits % 8);
                shl_const(&mut self.e, &mut self.sc, wa, k);
                shl_const(&mut self.e, &mut self.sc, wb, k);
            }
            flip_sign(&mut self.e, wa);
            flip_sign(&mut self.e, wb);
        }
        // Everything is expressed with `eq` and unsigned `<`; the negated forms
        // compute the opposite predicate into `t` and take 1 - t.
        let negate = |this: &mut Self, t: Cell| {
            this.e.inc(fc);
            this.e.move_cell_neg(t, fc);
        };
        match pred {
            Pred::Eq => eq(&mut self.e, &mut self.sc, wa, wb, fc),
            Pred::Ne => {
                let t = self.sc.alloc(1);
                eq(&mut self.e, &mut self.sc, wa, wb, t);
                negate(self, t);
            }
            Pred::Ult | Pred::Slt => ult(&mut self.e, &mut self.sc, wa, wb, fc),
            Pred::Ugt | Pred::Sgt => ult(&mut self.e, &mut self.sc, wb, wa, fc),
            Pred::Ule | Pred::Sle => {
                let t = self.sc.alloc(1);
                ult(&mut self.e, &mut self.sc, wb, wa, t);
                negate(self, t);
            }
            Pred::Uge | Pred::Sge => {
                let t = self.sc.alloc(1);
                ult(&mut self.e, &mut self.sc, wa, wb, t);
                negate(self, t);
            }
        }
        clear_work(&mut self.e, wa);
        clear_work(&mut self.e, wb);
        self.store_result(dest, flag)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn cast(&mut self, x: &Operand, to: TypeId, dest: Value, kind: Cast) -> Result<()> {
        let from_bits = self.op_bits(x)?;
        let to_bits = self.types().bits(to)?;
        let nf = limbs(from_bits);
        let nt = limbs(to_bits);
        let m = self.sc.mark();
        let w = match kind {
            Cast::ZExt => {
                let w = Work::alloc(&mut self.sc, nt.max(nf));
                self.load_operand(x, w.truncated(nf))?;
                if to_bits < from_bits {
                    trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                }
                w.truncated(nt)
            }
            Cast::SExt => {
                let src = Work::alloc(&mut self.sc, nf);
                self.load_operand(x, src)?;
                let w = Work::alloc(&mut self.sc, nt);
                sext_bits(&mut self.e, &mut self.sc, src, from_bits, w);
                if to_bits % 8 != 0 {
                    trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                }
                w
            }
            Cast::Trunc => {
                let w = Work::alloc(&mut self.sc, nf);
                self.load_operand(x, w)?;
                trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                w.truncated(nt)
            }
        };
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn select(
        &mut self,
        cond: &Operand,
        if_true: &Operand,
        if_false: &Operand,
        dest: Value,
    ) -> Result<()> {
        let size = self.types().size(if_true.ty);
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_any(if_false, w)?;
        let c = Work::alloc(&mut self.sc, 1);
        self.load_operand(cond, c)?;
        let flag = self.sc.alloc(1);
        self.e.begin_if_nonzero(c.limb(0), flag);
        clear_work(&mut self.e, w);
        self.load_any(if_true, w)?;
        self.e.end_if_nonzero(flag);
        self.e.clear(c.limb(0));
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn gep(
        &mut self,
        base: &Operand,
        source_ty: TypeId,
        indices: &[Operand],
        dest: Value,
    ) -> Result<()> {
        let n = self.ptr_limbs();
        let pbits = 8 * n as u32;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        self.load_operand(base, w)?;
        let mut ty = source_ty;
        for (k, idx) in indices.iter().enumerate() {
            // The first index scales by the whole source type; later ones step into it.
            let scale = if k == 0 {
                self.types().size(ty)
            } else {
                match self.types().kind(ty) {
                    TypeKind::Struct { .. } => {
                        let c = self
                            .const_of(idx)
                            .ok_or_else(|| anyhow!("non-constant struct index in GEP"))?;
                        let (ety, off) = self.types().element(ty, c as usize)?;
                        add_const(&mut self.e, &mut self.sc, w, off as u64);
                        ty = ety;
                        continue;
                    }
                    TypeKind::Array { .. } => {
                        let ety = self.types().array_elem(ty)?;
                        ty = ety;
                        self.types().size(ety)
                    }
                    _ => bail!("GEP into unsupported type {}", self.types().name(ty)),
                }
            };
            let ibits = self.op_bits(idx)?;
            if let Some(c) = self.const_of(idx) {
                let scaled = (sext(c, ibits) as i64).wrapping_mul(scale as i64);
                add_const(
                    &mut self.e,
                    &mut self.sc,
                    w,
                    mask_bits(scaled as u64, pbits),
                );
            } else {
                let ni = limbs(ibits);
                let m2 = self.sc.mark();
                let iw = Work::alloc(&mut self.sc, ni);
                self.load_operand(idx, iw)?;
                let ext = Work::alloc(&mut self.sc, n);
                if ni >= n {
                    move_work(&mut self.e, iw.truncated(n), ext);
                    clear_work(&mut self.e, iw);
                } else {
                    sext_bits(&mut self.e, &mut self.sc, iw, ibits, ext);
                }
                if scale == 1 {
                    add_work(&mut self.e, w, ext);
                } else {
                    let prod = Work::alloc(&mut self.sc, n);
                    mul_const(&mut self.e, &mut self.sc, ext, scale as u64, prod);
                    add_work(&mut self.e, w, prod);
                }
                self.sc.release(m2);
            }
        }
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn alloca(
        &mut self,
        block: usize,
        ty: TypeId,
        count: u64,
        align: usize,
        dest: Value,
    ) -> Result<()> {
        if block != 0 {
            bail!("alloca outside the entry block (dynamic allocation) is not supported");
        }
        let size = self.types().size(ty) as u64 * count;
        // Keep the memory stack pointer aligned to two pointers (variadic spill
        // areas rely on it).
        let align = (align as u64).max(2 * self.ptr_limbs() as u64);
        let size = size.div_ceil(align) * align;
        let n = self.ptr_limbs();
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        load_packed(&mut self.e, &mut self.sc, self.lay.msp, w);
        add_const(
            &mut self.e,
            &mut self.sc,
            w,
            mask_bits(size.wrapping_neg(), 8 * n as u32),
        );
        let w2 = Work::alloc(&mut self.sc, n);
        copy_work(&mut self.e, &mut self.sc, w, w2);
        self.store_to_cells(self.lay.msp, n, w);
        self.store_result(dest, w2)?;
        self.sc.release(m);
        Ok(())
    }

    /// Type and byte offset of a nested aggregate field.
    fn agg_field(&self, ty: TypeId, indices: &[u32]) -> Result<(TypeId, usize)> {
        let mut t = ty;
        let mut off = 0;
        for &i in indices {
            let (et, o) = self.types().element(t, i as usize)?;
            off += o;
            t = et;
        }
        Ok((t, off))
    }

    fn extract_value(&mut self, agg: &Operand, indices: &[u32], dest: Value) -> Result<()> {
        let (fty, off) = self.agg_field(agg.ty, indices)?;
        let size = self.types().size(fty);
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_operand_bytes(agg, off, w)?;
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn insert_value(
        &mut self,
        agg: &Operand,
        elem: &Operand,
        indices: &[u32],
        dest: Value,
    ) -> Result<()> {
        let size = self.types().size(agg.ty);
        let (fty, off) = self.agg_field(agg.ty, indices)?;
        let fsize = self.types().size(fty);
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_operand_bytes(agg, 0, w)?;
        for k in 0..fsize {
            self.e.clear(w.limb(off + k));
        }
        let fw = Work::alloc(&mut self.sc, fsize);
        self.load_any(elem, fw)?;
        for k in 0..fsize {
            self.e.move_cell(fw.limb(k), w.limb(off + k));
        }
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }
}

pub(super) fn sext(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        (((v << (64 - bits)) as i64) >> (64 - bits)) as u64
    }
}
