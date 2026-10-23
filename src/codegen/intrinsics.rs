//! Compiler builtins (`__llvbf_*`) and LLVM intrinsics that are lowered inline.

use super::*;

/// Boolean helpers on single cells holding 0 or 1. All of them consume their inputs.
mod bits {
    use crate::arith::Scratch;
    use crate::emit::{Cell, Emitter};

    /// dst += a XOR b (a, b consumed).
    pub fn xor(e: &mut Emitter, sc: &mut Scratch, a: Cell, b: Cell, dst: Cell) {
        let m = sc.mark();
        let t = sc.alloc(1);
        e.move_cell(a, t);
        e.move_cell(b, t);
        // t is 0, 1 or 2; the xor is 1 exactly when t == 1.
        e.while_nonzero(t, |e| {
            e.dec(t);
            e.inc(dst);
            e.while_nonzero(t, |e| {
                e.dec(t);
                e.dec(dst);
            });
        });
        sc.release(m);
    }

    /// dst += a AND b (a, b consumed).
    pub fn and(e: &mut Emitter, a: Cell, b: Cell, dst: Cell) {
        e.while_nonzero(a, |e| {
            e.dec(a);
            e.while_nonzero(b, |e| {
                e.dec(b);
                e.inc(dst);
            });
        });
        e.clear(b);
    }

    /// a = NOT a.
    pub fn not(e: &mut Emitter, sc: &mut Scratch, a: Cell) {
        let m = sc.mark();
        let t = sc.alloc(1);
        e.inc(t);
        e.move_cell_neg(a, t);
        e.move_cell(t, a);
        sc.release(m);
    }
}

impl<'a> Gen<'a> {
    pub(super) fn inline_call(&mut self, inst: &Instr) -> Result<()> {
        let (callee, args) = match &inst.op {
            Op::Call { callee, args, .. } => (callee, args),
            _ => unreachable!(),
        };
        let name = intrinsic_name(callee)
            .ok_or_else(|| anyhow!("call treated as inline"))?
            .to_string();
        let args: Vec<&Operand> = args.iter().collect();
        let dest = inst.dest;
        match name.as_str() {
            "__llvbf_putc" => self.putc(args[0]),
            "__llvbf_getc" => {
                self.getc(dest.ok_or_else(|| anyhow!("__llvbf_getc without result"))?)
            }
            "__llvbf_exit" => {
                self.e.clear(self.lay.active);
                Ok(())
            }
            n if n.starts_with("llvm.expect.") => {
                self.cast(args[0], inst.ty, dest.unwrap(), Cast::ZExt)
            }
            n if n.starts_with("llvm.abs.") => self.abs(args[0], dest.unwrap()),
            n if n.starts_with("llvm.smax.")
                || n.starts_with("llvm.smin.")
                || n.starts_with("llvm.umax.")
                || n.starts_with("llvm.umin.") =>
            {
                self.min_max(
                    args[0],
                    args[1],
                    dest.unwrap(),
                    n.starts_with("llvm.s"),
                    n.contains("max"),
                )
            }
            n if n.starts_with("llvm.bswap.") => self.bswap(args[0], dest.unwrap()),
            n if n.starts_with("llvm.fshl.") || n.starts_with("llvm.fshr.") => self.funnel_shift(
                args[0],
                args[1],
                args[2],
                dest.unwrap(),
                n.starts_with("llvm.fshl."),
            ),
            n if n.starts_with("llvm.ctpop.") => self.ctpop(args[0], dest.unwrap()),
            n if n.starts_with("llvm.ctlz.") || n.starts_with("llvm.cttz.") => {
                self.count_zeros(args[0], dest.unwrap(), n.starts_with("llvm.cttz."))
            }
            n if n.contains(".with.overflow.") => self.with_overflow(inst, &args, n),
            n if is_inline_intrinsic(n) => Ok(()), // lifetime, dbg, assume, va_end, ...
            n => bail!("unsupported call to {}", n),
        }
    }

    fn putc(&mut self, arg: &Operand) -> Result<()> {
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, 1);
        self.load_operand(arg, w)?;
        self.e.out(w.limb(0));
        self.e.clear(w.limb(0));
        self.sc.release(m);
        Ok(())
    }

    fn getc(&mut self, dest: Value) -> Result<()> {
        let (cell, _) = self.define_slot(dest)?;
        // The cell is zero, so an interpreter that leaves it unchanged at EOF yields 0.
        self.e.input(cell);
        Ok(())
    }

    fn abs(&mut self, arg: &Operand, dest: Value) -> Result<()> {
        let n = limbs(self.op_bits(arg)?);
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        self.load_operand(arg, w)?;
        let s = self.sc.alloc(1);
        sign_bit(&mut self.e, &mut self.sc, w, s);
        self.e.begin_while(s);
        self.e.dec(s);
        negate_work(&mut self.e, &mut self.sc, w);
        self.e.end_while(s);
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn min_max(
        &mut self,
        a: &Operand,
        b: &Operand,
        dest: Value,
        signed: bool,
        want_max: bool,
    ) -> Result<()> {
        let n = limbs(self.op_bits(a)?);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        let wb = Work::alloc(&mut self.sc, n);
        self.load_operand(a, wa)?;
        self.load_operand(b, wb)?;
        if signed {
            flip_sign(&mut self.e, wa);
            flip_sign(&mut self.e, wb);
        }
        let lt = Work::alloc(&mut self.sc, 1); // a < b
        ult(&mut self.e, &mut self.sc, wa, wb, lt.limb(0));
        if signed {
            flip_sign(&mut self.e, wa);
            flip_sign(&mut self.e, wb);
        }
        // Pick the operand: if a < b the maximum is b, otherwise a (and vice versa for min).
        let res = Work::alloc(&mut self.sc, n);
        let flag = self.sc.alloc(1);
        self.e.begin_if_else(lt.limb(0), flag);
        // a >= b
        let (keep, drop) = if want_max { (wa, wb) } else { (wb, wa) };
        move_work(&mut self.e, keep, res);
        clear_work(&mut self.e, drop);
        self.e.else_if_else(lt.limb(0), flag);
        // a < b
        let (keep, drop) = if want_max { (wb, wa) } else { (wa, wb) };
        move_work(&mut self.e, keep, res);
        clear_work(&mut self.e, drop);
        self.e.end_if_else(flag);
        self.e.clear(lt.limb(0));
        self.store_result(dest, res)?;
        self.sc.release(m);
        Ok(())
    }

    fn bswap(&mut self, arg: &Operand, dest: Value) -> Result<()> {
        let n = limbs(self.op_bits(arg)?);
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        self.load_operand(arg, w)?;
        let r = Work::alloc(&mut self.sc, n);
        for k in 0..n {
            self.e.move_cell(w.limb(k), r.limb(n - 1 - k));
        }
        self.store_result(dest, r)?;
        self.sc.release(m);
        Ok(())
    }

    /// fshl(a, b, k) = (a << k) | (b >> (bits - k)); fshr(a, b, k) = (a << (bits - k)) | (b >> k).
    fn funnel_shift(
        &mut self,
        a: &Operand,
        b: &Operand,
        k: &Operand,
        dest: Value,
        left: bool,
    ) -> Result<()> {
        let bits = self.op_bits(a)?;
        if bits % 8 != 0 {
            bail!("funnel shift on non-byte width i{}", bits);
        }
        let n = limbs(bits);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        let wb = Work::alloc(&mut self.sc, n);
        self.load_operand(a, wa)?;
        self.load_operand(b, wb)?;
        if let Some(c) = self.const_of(k) {
            let k = (c % bits as u64) as u32;
            if k == 0 {
                // Degenerate: the result is a (fshl) or b (fshr).
                if left {
                    clear_work(&mut self.e, wb);
                } else {
                    clear_work(&mut self.e, wa);
                    move_work(&mut self.e, wb, wa);
                }
            } else {
                let (ka, kb) = if left { (k, bits - k) } else { (bits - k, k) };
                shl_const(&mut self.e, &mut self.sc, wa, ka);
                lshr_const(&mut self.e, &mut self.sc, wb, kb);
                // The two parts occupy disjoint bits, so adding equals or-ing.
                add_work(&mut self.e, wa, wb);
            }
        } else {
            // k mod bits (bits is a power of two), and the complementary amount bits - k.
            let kn = limbs(self.op_bits(k)?);
            let kw = Work::alloc(&mut self.sc, kn);
            self.load_operand(k, kw)?;
            bitwise_const(&mut self.e, &mut self.sc, kw, (bits - 1) as u64, 0);
            let comp = Work::alloc(&mut self.sc, kn);
            load_const(&mut self.e, comp, bits as u64);
            let kcopy = Work::alloc(&mut self.sc, kn);
            copy_work(&mut self.e, &mut self.sc, kw, kcopy);
            sub_work(&mut self.e, comp, kcopy);
            // A shift by `bits` (k == 0) clears the operand, which is what the formula needs.
            let (ka, kb) = if left { (kw, comp) } else { (comp, kw) };
            shift_var(&mut self.e, &mut self.sc, wa, ka, 0);
            shift_var(&mut self.e, &mut self.sc, wb, kb, 1);
            add_work(&mut self.e, wa, wb);
        }
        self.store_result(dest, wa)?;
        self.sc.release(m);
        Ok(())
    }

    /// Decompose an operand into one cell per bit (bit k of limb i at `bits + 8*i + k`).
    fn operand_bits(&mut self, arg: &Operand, n: usize) -> Result<Cell> {
        let w = Work::alloc(&mut self.sc, n);
        self.load_operand(arg, w)?;
        let bits = self.sc.alloc(8 * n);
        for i in 0..n {
            to_bits(&mut self.e, &mut self.sc, w.limb(i), bits + 8 * i as Cell);
        }
        Ok(bits)
    }

    fn ctpop(&mut self, arg: &Operand, dest: Value) -> Result<()> {
        let n = limbs(self.op_bits(arg)?);
        let m = self.sc.mark();
        let bits = self.operand_bits(arg, n)?;
        let res = Work::alloc(&mut self.sc, n);
        for k in 0..8 * n {
            let b = bits + k as Cell;
            self.e.begin_while(b);
            self.e.dec(b);
            inc_with_carry(&mut self.e, res, 0);
            self.e.end_while(b);
        }
        self.store_result(dest, res)?;
        self.sc.release(m);
        Ok(())
    }

    /// Count leading (ctlz) or trailing (cttz) zero bits.
    fn count_zeros(&mut self, arg: &Operand, dest: Value, trailing: bool) -> Result<()> {
        let n = limbs(self.op_bits(arg)?);
        let m = self.sc.mark();
        let bits = self.operand_bits(arg, n)?;
        let res = Work::alloc(&mut self.sc, n);
        // `counting` stays 1 until the first one bit is seen.
        let counting = self.sc.alloc(1);
        self.e.inc(counting);
        let order: Vec<usize> = if trailing {
            (0..8 * n).collect()
        } else {
            (0..8 * n).rev().collect()
        };
        for k in order {
            let b = bits + k as Cell;
            // f = counting AND NOT bit; if the bit is set, stop counting.
            let f = self.sc.alloc(1);
            let tmp = self.sc.alloc(1);
            self.e.copy_cell(counting, f, tmp);
            self.e.begin_while(b);
            self.e.dec(b);
            self.e.clear(counting);
            self.e.clear(f);
            self.e.end_while(b);
            self.e.begin_while(f);
            self.e.dec(f);
            inc_with_carry(&mut self.e, res, 0);
            self.e.end_while(f);
        }
        self.e.clear(counting);
        self.store_result(dest, res)?;
        self.sc.release(m);
        Ok(())
    }

    /// `llvm.{u,s}{add,sub}.with.overflow` and `llvm.umul.with.overflow`: the result is
    /// the aggregate `{iN, i1}` stored as N/8 limbs followed by the overflow flag cell.
    fn with_overflow(&mut self, inst: &Instr, args: &[&Operand], name: &str) -> Result<()> {
        let dest = inst.dest.unwrap();
        let n = limbs(self.op_bits(args[0])?);
        let (cell, _) = self.define_slot(dest)?;
        let (_, flag_off) = self.types().element(inst.ty, 1)?;
        let m = self.sc.mark();
        let ovf = self.sc.alloc(1);
        if name.starts_with("llvm.uadd.") {
            // Add with one extra limb: the extra limb is the carry out.
            let wa = Work::alloc(&mut self.sc, n + 1);
            let wb = Work::alloc(&mut self.sc, n);
            self.load_operand(args[0], wa.truncated(n))?;
            self.load_operand(args[1], wb)?;
            add_work(&mut self.e, wa, wb);
            self.e.move_cell(wa.limb(n), ovf);
            self.store_to_cells(cell, n, wa.truncated(n));
        } else if name.starts_with("llvm.usub.") {
            // Overflow iff a < b.
            let wa = Work::alloc(&mut self.sc, n);
            let wb = Work::alloc(&mut self.sc, n);
            self.load_operand(args[0], wa)?;
            self.load_operand(args[1], wb)?;
            ult(&mut self.e, &mut self.sc, wa, wb, ovf);
            sub_work(&mut self.e, wa, wb);
            self.store_to_cells(cell, n, wa);
        } else if name.starts_with("llvm.sadd.") || name.starts_with("llvm.ssub.") {
            // Signed addition overflows iff the operands have the same sign and the
            // result's sign differs; subtraction iff the operands differ in sign and
            // the result's sign differs from the first operand's.
            let sub = name.starts_with("llvm.ssub.");
            let wa = Work::alloc(&mut self.sc, n);
            let wb = Work::alloc(&mut self.sc, n);
            self.load_operand(args[0], wa)?;
            self.load_operand(args[1], wb)?;
            let sa = self.sc.alloc(1);
            let sa2 = self.sc.alloc(1);
            let sb = self.sc.alloc(1);
            let sr = self.sc.alloc(1);
            sign_bit(&mut self.e, &mut self.sc, wa, sa);
            self.e.copy_cell(sa, sa2, self.sc.alloc(1));
            sign_bit(&mut self.e, &mut self.sc, wb, sb);
            if sub {
                sub_work(&mut self.e, wa, wb);
            } else {
                add_work(&mut self.e, wa, wb);
            }
            sign_bit(&mut self.e, &mut self.sc, wa, sr);
            let signs_differ = self.sc.alloc(1);
            bits::xor(&mut self.e, &mut self.sc, sa, sb, signs_differ);
            if !sub {
                bits::not(&mut self.e, &mut self.sc, signs_differ);
            }
            let result_differs = self.sc.alloc(1);
            bits::xor(&mut self.e, &mut self.sc, sa2, sr, result_differs);
            bits::and(&mut self.e, signs_differ, result_differs, ovf);
            self.store_to_cells(cell, n, wa);
        } else if name.starts_with("llvm.umul.") {
            // Multiply in double width; overflow iff the upper half is nonzero.
            let wa = Work::alloc(&mut self.sc, 2 * n);
            let wb = Work::alloc(&mut self.sc, 2 * n);
            self.load_operand(args[0], wa.truncated(n))?;
            self.load_operand(args[1], wb.truncated(n))?;
            let res = Work::alloc(&mut self.sc, 2 * n);
            mul_work(&mut self.e, &mut self.sc, wa, wb, res);
            let hi = Work {
                base: res.limb(n),
                n,
            };
            is_nonzero_destructive(&mut self.e, hi, ovf);
            self.store_to_cells(cell, n, res.truncated(n));
        } else {
            bail!("unsupported intrinsic {}", name);
        }
        self.e.move_cell(ovf, cell + flag_off as Cell);
        self.sc.release(m);
        Ok(())
    }
}
