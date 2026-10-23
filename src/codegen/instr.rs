//! Lowering of ordinary instructions.

use super::*;

impl<'a> Gen<'a> {
    pub(super) fn const_of(&self, op: &Operand) -> Option<u64> {
        match op {
            Operand::ConstantOperand(c) => self.ce().scalar(c).ok(),
            _ => None,
        }
    }

    pub(super) fn emit_instr(&mut self, bb: &llvm_ir::BasicBlock, i: usize) -> Result<()> {
        let inst = &bb.instrs[i];
        match inst {
            Instruction::Add(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Add),
            Instruction::Sub(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Sub),
            Instruction::Mul(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Mul),
            Instruction::UDiv(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::UDiv),
            Instruction::SDiv(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::SDiv),
            Instruction::URem(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::URem),
            Instruction::SRem(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::SRem),
            Instruction::And(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::And),
            Instruction::Or(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Or),
            Instruction::Xor(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Xor),
            Instruction::Shl(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Shl),
            Instruction::LShr(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::LShr),
            Instruction::AShr(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::AShr),
            Instruction::ICmp(x) => self.icmp(x),
            Instruction::ZExt(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::SExt(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::SExt),
            Instruction::Trunc(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::Trunc),
            Instruction::PtrToInt(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::IntToPtr(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::BitCast(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::AddrSpaceCast(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::Freeze(x) => {
                let ty = self.op_type(&x.operand);
                self.cast(&x.operand, &ty, &x.dest, Cast::ZExt)
            }
            Instruction::Select(x) => self.select(x),
            Instruction::Phi(_) => Ok(()),
            Instruction::GetElementPtr(x) => self.gep(x),
            Instruction::Alloca(x) => self.alloca(bb, x),
            Instruction::ExtractValue(x) => self.extract_value(x),
            Instruction::InsertValue(x) => self.insert_value(x),
            Instruction::Call(c) => self.inline_call(c),
            Instruction::Load(_) | Instruction::Store(_) | Instruction::VAArg(_) => Ok(()), // handled as segment ends
            other => bail!("unsupported instruction {}", other),
        }
    }

    pub(super) fn binop(&mut self, a: &Operand, b: &Operand, dest: &Name, op: BinOp) -> Result<()> {
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

    pub(super) fn icmp(&mut self, x: &instruction::ICmp) -> Result<()> {
        let bits = self.op_bits(&x.operand0)?;
        let n = limbs(bits);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        let wb = Work::alloc(&mut self.sc, n);
        self.load_operand(&x.operand0, wa)?;
        self.load_operand(&x.operand1, wb)?;
        let flag = Work::alloc(&mut self.sc, 1);
        let fc = flag.limb(0);
        use IntPredicate::*;
        let signed = matches!(x.predicate, SGT | SGE | SLT | SLE);
        if signed {
            if bits % 8 != 0 {
                // Sign bit of a sub-byte value (i1): move it to the top of the limb.
                let k = 8 - (bits % 8);
                shl_const(&mut self.e, &mut self.sc, wa, k);
                shl_const(&mut self.e, &mut self.sc, wb, k);
            }
            flip_sign(&mut self.e, wa);
            flip_sign(&mut self.e, wb);
        }
        match x.predicate {
            EQ => eq(&mut self.e, &mut self.sc, wa, wb, fc),
            NE => {
                let t = self.sc.alloc(1);
                eq(&mut self.e, &mut self.sc, wa, wb, t);
                self.e.inc(fc);
                self.e.move_cell_neg(t, fc);
            }
            ULT | SLT => ult(&mut self.e, &mut self.sc, wa, wb, fc),
            UGT | SGT => ult(&mut self.e, &mut self.sc, wb, wa, fc),
            ULE | SLE => {
                let t = self.sc.alloc(1);
                ult(&mut self.e, &mut self.sc, wb, wa, t);
                self.e.inc(fc);
                self.e.move_cell_neg(t, fc);
            }
            UGE | SGE => {
                let t = self.sc.alloc(1);
                ult(&mut self.e, &mut self.sc, wa, wb, t);
                self.e.inc(fc);
                self.e.move_cell_neg(t, fc);
            }
        }
        clear_work(&mut self.e, wa);
        clear_work(&mut self.e, wb);
        self.store_result(&x.dest, flag)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn cast(
        &mut self,
        op: &Operand,
        to: &TypeRef,
        dest: &Name,
        kind: Cast,
    ) -> Result<()> {
        let from_bits = self.op_bits(op)?;
        let to_bits = self.ti.scalar_bits(to)?;
        let nf = limbs(from_bits);
        let nt = limbs(to_bits);
        let m = self.sc.mark();
        let w = match kind {
            Cast::ZExt => {
                let w = Work::alloc(&mut self.sc, nt.max(nf));
                self.load_operand(op, w.truncated(nf))?;
                if to_bits < from_bits {
                    trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                }
                w.truncated(nt)
            }
            Cast::SExt => {
                let src = Work::alloc(&mut self.sc, nf);
                self.load_operand(op, src)?;
                let w = Work::alloc(&mut self.sc, nt);
                sext_bits(&mut self.e, &mut self.sc, src, from_bits, w);
                if to_bits % 8 != 0 {
                    trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                }
                w
            }
            Cast::Trunc => {
                let w = Work::alloc(&mut self.sc, nf);
                self.load_operand(op, w)?;
                trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                w.truncated(nt)
            }
        };
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn select(&mut self, x: &instruction::Select) -> Result<()> {
        let ty = self.op_type(&x.true_value);
        let size = self.ti.size_of(&ty)?;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        let scalar = self.ti.is_scalar(&ty);
        if scalar {
            self.load_operand(&x.false_value, w)?;
        } else {
            self.load_operand_bytes(&x.false_value, 0, w)?;
        }
        let c = Work::alloc(&mut self.sc, 1);
        self.load_operand(&x.condition, c)?;
        let flag = self.sc.alloc(1);
        self.e.begin_if_nonzero(c.limb(0), flag);
        clear_work(&mut self.e, w);
        if scalar {
            self.load_operand(&x.true_value, w)?;
        } else {
            self.load_operand_bytes(&x.true_value, 0, w)?;
        }
        self.e.end_if_nonzero(flag);
        self.e.clear(c.limb(0));
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn gep(&mut self, x: &instruction::GetElementPtr) -> Result<()> {
        let n = self.ptr_limbs();
        let pbits = 8 * n as u32;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        self.load_operand(&x.address, w)?;
        let mut ty = x.source_element_type.clone();
        for (k, idx) in x.indices.iter().enumerate() {
            let scale: usize;
            if k == 0 {
                scale = self.ti.size_of(&ty)?;
            } else {
                let rty = self.ti.resolve(&ty)?.clone();
                match rty {
                    Type::StructType { .. } => {
                        let c = self
                            .const_of(idx)
                            .ok_or_else(|| anyhow!("non-constant struct index in GEP"))?;
                        let (ety, off) = self.ti.element(&ty, c as usize)?;
                        add_const(&mut self.e, &mut self.sc, w, off as u64);
                        ty = ety;
                        continue;
                    }
                    Type::ArrayType { .. } => {
                        let ety = self.ti.array_element(&ty)?;
                        scale = self.ti.size_of(&ety)?;
                        ty = ety;
                    }
                    t => bail!("GEP into unsupported type {}", t),
                }
            }
            if let Some(c) = self.const_of(idx) {
                let ibits = self.op_bits(idx)?;
                let sc_val = (sext(c, ibits) as i64).wrapping_mul(scale as i64);
                add_const(
                    &mut self.e,
                    &mut self.sc,
                    w,
                    mask_bits(sc_val as u64, pbits),
                );
            } else {
                let ibits = self.op_bits(idx)?;
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
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn alloca(
        &mut self,
        bb: &llvm_ir::BasicBlock,
        x: &instruction::Alloca,
    ) -> Result<()> {
        if bb.name != self.func().basic_blocks[0].name {
            bail!("alloca outside the entry block (dynamic allocation) is not supported");
        }
        let count = match &x.num_elements {
            Operand::ConstantOperand(c) => self.ce().scalar(c)?,
            _ => bail!("alloca with a variable element count is not supported"),
        };
        let size = self.ti.size_of(&x.allocated_type)? as u64 * count;
        let align = (x.alignment as u64).max(2);
        let size = (size + align - 1) / align * align;
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
        self.store_result(&x.dest, w2)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn agg_field(&self, ty: &TypeRef, indices: &[u32]) -> Result<(TypeRef, usize)> {
        let mut t = ty.clone();
        let mut off = 0;
        for &i in indices {
            let (et, o) = self.ti.element(&t, i as usize)?;
            off += o;
            t = et;
        }
        Ok((t, off))
    }

    pub(super) fn extract_value(&mut self, x: &instruction::ExtractValue) -> Result<()> {
        let ty = self.op_type(&x.aggregate);
        let (fty, off) = self.agg_field(&ty, &x.indices)?;
        let size = self.ti.size_of(&fty)?;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_operand_bytes(&x.aggregate, off, w)?;
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    pub(super) fn insert_value(&mut self, x: &instruction::InsertValue) -> Result<()> {
        let ty = self.op_type(&x.aggregate);
        let size = self.ti.size_of(&ty)?;
        let (fty, off) = self.agg_field(&ty, &x.indices)?;
        let fsize = self.ti.size_of(&fty)?;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_operand_bytes(&x.aggregate, 0, w)?;
        for k in 0..fsize {
            self.e.clear(w.limb(off + k));
        }
        let fw = Work::alloc(&mut self.sc, fsize);
        if self.ti.is_scalar(&fty) {
            self.load_operand(&x.element, fw)?;
        } else {
            self.load_operand_bytes(&x.element, 0, fw)?;
        }
        for k in 0..fsize {
            self.e.move_cell(fw.limb(k), w.limb(off + k));
        }
        self.store_result(&x.dest, w)?;
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

#[derive(Clone, Copy, PartialEq)]
pub(super) enum BinOp {
    Add,
    Sub,
    Mul,
    UDiv,
    SDiv,
    URem,
    SRem,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Cast {
    ZExt,
    SExt,
    Trunc,
}
