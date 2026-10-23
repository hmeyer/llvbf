//! Multi-limb integer arithmetic on top of the emitter.
//!
//! Integers are little-endian sequences of 8-bit limbs. Values stored in stack
//! frame slots are *packed* (limb `i` at `base + i`). Arithmetic is performed in
//! *spaced* work areas (limb `i` at `base + 3*i`) so that every limb has two zero
//! cells after it, which the constant-time `if_zero` idiom needs for carry and
//! borrow detection.
//!
//! All routines allocate their temporaries from a [`Scratch`] bump allocator and
//! leave every scratch cell they used at zero.

use crate::emit::{Cell, Emitter};

/// Bump allocator for scratch cells. Cells handed out are guaranteed zero and must
/// be zero again when released.
pub struct Scratch {
    start: Cell,
    next: Cell,
    pub high_water: Cell,
}

impl Scratch {
    pub fn new(start: Cell) -> Self {
        Scratch {
            start,
            next: start,
            high_water: start,
        }
    }
    pub fn alloc(&mut self, n: usize) -> Cell {
        let c = self.next;
        self.next += n as Cell;
        if self.next > self.high_water {
            self.high_water = self.next;
        }
        c
    }
    pub fn mark(&self) -> Cell {
        self.next
    }
    pub fn release(&mut self, mark: Cell) {
        debug_assert!(mark >= self.start && mark <= self.next);
        self.next = mark;
    }
    pub fn start(&self) -> Cell {
        self.start
    }
    pub fn used(&self) -> usize {
        (self.high_water - self.start) as usize
    }
}

/// A spaced work area holding an `n`-limb integer.
#[derive(Clone, Copy, Debug)]
pub struct Work {
    pub base: Cell,
    pub n: usize,
}

impl Work {
    pub fn limb(&self, i: usize) -> Cell {
        debug_assert!(i < self.n);
        self.base + 3 * i as Cell
    }
    pub fn alloc(sc: &mut Scratch, n: usize) -> Work {
        Work {
            base: sc.alloc(3 * n),
            n,
        }
    }
    /// View the first `n` limbs as a smaller work area.
    pub fn truncated(&self, n: usize) -> Work {
        debug_assert!(n <= self.n);
        Work { base: self.base, n }
    }
}

/// Number of limbs for an integer of the given bit width.
pub fn limbs(bits: u32) -> usize {
    bits.div_ceil(8) as usize
}

// ---------------------------------------------------------------------------
// Loading, storing, constants
// ---------------------------------------------------------------------------

/// Write a constant into a zeroed work area.
pub fn load_const(e: &mut Emitter, w: Work, value: u64) {
    for i in 0..w.n {
        let limb = ((value >> (8 * i)) & 0xff) as u8;
        if limb != 0 {
            e.set_using(w.limb(i), limb, w.limb(i) + 1);
        }
    }
}

/// Copy packed limbs at `src` into zeroed work area `w` (non-destructive).
pub fn load_packed(e: &mut Emitter, sc: &mut Scratch, src: Cell, w: Work) {
    let m = sc.mark();
    let tmp = sc.alloc(1);
    copy_packed(e, src, w, tmp);
    sc.release(m);
}

/// Copy packed limbs at `src` into a zeroed work area using `tmp` (zero, left zero)
/// as the temporary. A temporary next to the source makes the copy cost about the
/// same as a move: the duplicate only travels one cell back.
pub fn copy_packed(e: &mut Emitter, src: Cell, w: Work, tmp: Cell) {
    for i in 0..w.n {
        e.copy_cell(src + i as Cell, w.limb(i), tmp);
    }
}

/// Move a work area into packed limbs at `dst` (clearing dst first). `w` is zero afterwards.
pub fn store_packed(e: &mut Emitter, w: Work, dst: Cell) {
    for i in 0..w.n {
        e.clear(dst + i as Cell);
        e.move_cell(w.limb(i), dst + i as Cell);
    }
}

/// Copy a work area into another zeroed work area, non-destructively.
pub fn copy_work(e: &mut Emitter, _sc: &mut Scratch, src: Work, dst: Work) {
    debug_assert!(dst.n <= src.n);
    // The zero cell after each source limb is the temporary.
    for i in 0..dst.n {
        e.copy_cell(src.limb(i), dst.limb(i), src.limb(i) + 1);
    }
}

/// Move a work area into another (zeroed) work area. `src` is zero afterwards.
pub fn move_work(e: &mut Emitter, src: Work, dst: Work) {
    for i in 0..dst.n.min(src.n) {
        e.move_cell(src.limb(i), dst.limb(i));
    }
    for i in dst.n.min(src.n)..src.n {
        e.clear(src.limb(i));
    }
}

pub fn clear_work(e: &mut Emitter, w: Work) {
    for i in 0..w.n {
        e.clear(w.limb(i));
    }
}

// ---------------------------------------------------------------------------
// Addition / subtraction
// ---------------------------------------------------------------------------

/// Increment limb `i` of `w` with carry propagation into higher limbs.
pub fn inc_with_carry(e: &mut Emitter, w: Work, i: usize) {
    e.inc(w.limb(i));
    if i + 1 < w.n {
        e.if_zero(w.limb(i), |e| inc_with_carry(e, w, i + 1));
    }
}

/// Decrement limb `i` of `w` with borrow propagation into higher limbs.
pub fn dec_with_borrow(e: &mut Emitter, w: Work, i: usize) {
    if i + 1 < w.n {
        e.if_zero(w.limb(i), |e| dec_with_borrow(e, w, i + 1));
    }
    e.dec(w.limb(i));
}

/// dst += src. `src` is destroyed (zero afterwards). `src.n <= dst.n`.
pub fn add_work(e: &mut Emitter, dst: Work, src: Work) {
    debug_assert!(src.n <= dst.n);
    for i in 0..src.n {
        e.while_nonzero(src.limb(i), |e| {
            e.dec(src.limb(i));
            inc_with_carry(e, dst, i);
        });
    }
}

/// dst -= src. `src` is destroyed (zero afterwards). `src.n <= dst.n`.
pub fn sub_work(e: &mut Emitter, dst: Work, src: Work) {
    debug_assert!(src.n <= dst.n);
    for i in 0..src.n {
        e.while_nonzero(src.limb(i), |e| {
            e.dec(src.limb(i));
            dec_with_borrow(e, dst, i);
        });
    }
}

/// dst += src, preserving `src`: each source limb is rebuilt in the zero cell next
/// to it while it is counted down, so no copy of `src` is needed.
pub fn add_work_keep(e: &mut Emitter, dst: Work, src: Work) {
    debug_assert!(src.n <= dst.n);
    for i in 0..src.n {
        let (x, keep) = (src.limb(i), src.limb(i) + 1);
        e.while_nonzero(x, |e| {
            e.dec(x);
            e.inc(keep);
            inc_with_carry(e, dst, i);
        });
        e.move_cell(keep, x);
    }
}

/// dst -= src, preserving `src` (see [`add_work_keep`]).
pub fn sub_work_keep(e: &mut Emitter, dst: Work, src: Work) {
    debug_assert!(src.n <= dst.n);
    for i in 0..src.n {
        let (x, keep) = (src.limb(i), src.limb(i) + 1);
        e.while_nonzero(x, |e| {
            e.dec(x);
            e.inc(keep);
            dec_with_borrow(e, dst, i);
        });
        e.move_cell(keep, x);
    }
}

/// dst += constant (mod 2^(8n)).
///
/// The add/sub loops cost one carry check per unit of each source limb, so a
/// constant is applied in whichever of its two representations has the smaller
/// limb sum: `x + 0xFFFFFFFF` becomes `x - 1`. Only the nonzero limbs of the
/// constant get a loop at all.
pub fn add_const(e: &mut Emitter, sc: &mut Scratch, dst: Work, value: u64) {
    let bits = 8 * dst.n as u32;
    let v = mask(value, bits);
    if v == 0 {
        return;
    }
    let neg = mask(v.wrapping_neg(), bits);
    let (c, subtract) = if limb_sum(neg, dst.n) < limb_sum(v, dst.n) {
        (neg, true)
    } else {
        (v, false)
    };
    let m = sc.mark();
    let t = Work::alloc(sc, 1);
    for i in 0..dst.n {
        let limb = ((c >> (8 * i.min(7))) & 0xff) as u8;
        if limb == 0 {
            continue;
        }
        e.set_using(t.limb(0), limb, t.limb(0) + 1);
        e.while_nonzero(t.limb(0), |e| {
            e.dec(t.limb(0));
            if subtract {
                dec_with_borrow(e, dst, i);
            } else {
                inc_with_carry(e, dst, i);
            }
        });
    }
    sc.release(m);
}

fn mask(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        v & ((1u64 << bits) - 1)
    }
}

/// Sum of the limb values of a constant: the number of carry-check iterations an
/// add or sub loop would run for it.
fn limb_sum(v: u64, n: usize) -> u64 {
    (0..n.min(8)).map(|i| (v >> (8 * i)) & 0xff).sum()
}

/// Two's complement negation in place.
pub fn negate_work(e: &mut Emitter, sc: &mut Scratch, w: Work) {
    let m = sc.mark();
    let t = sc.alloc(1);
    for i in 0..w.n {
        // t = 255 - limb
        e.add(t, 255);
        e.move_cell_neg(w.limb(i), t);
        e.move_cell(t, w.limb(i));
    }
    inc_with_carry(e, w, 0);
    sc.release(m);
}

// ---------------------------------------------------------------------------
// Tests and comparisons
// ---------------------------------------------------------------------------

/// Destructively test whether `w` is zero: `flag` (must be zero) becomes 1 if so. `w` is cleared.
pub fn is_zero_destructive(e: &mut Emitter, w: Work, flag: Cell) {
    e.inc(flag);
    for i in 0..w.n {
        e.while_nonzero(w.limb(i), |e| {
            e.clear(flag);
            e.clear(w.limb(i));
        });
    }
}

/// Destructively test whether `w` is nonzero: `flag` (must be zero) becomes 1 if so. `w` is cleared.
pub fn is_nonzero_destructive(e: &mut Emitter, w: Work, flag: Cell) {
    for i in 0..w.n {
        e.while_nonzero(w.limb(i), |e| {
            e.clear(flag);
            e.inc(flag);
            e.clear(w.limb(i));
        });
    }
}

/// Non-destructively set `flag` (must be zero) to 1 if `w` is nonzero.
pub fn is_nonzero(e: &mut Emitter, sc: &mut Scratch, w: Work, flag: Cell) {
    let m = sc.mark();
    let f = sc.alloc(1);
    for i in 0..w.n {
        e.if_nonzero(w.limb(i), f, |e| {
            e.clear(flag);
            e.inc(flag);
        });
    }
    sc.release(m);
}

/// Unsigned `a < b`. Both work areas are destroyed; `flag` (zero) receives 0/1.
/// `a` must have `n + 1` limbs (the extra top limb zero), `b` has `n` limbs.
pub fn ult_destructive(e: &mut Emitter, a: Work, b: Work, flag: Cell) {
    debug_assert_eq!(a.n, b.n + 1);
    sub_work(e, a, b);
    let top = a.limb(b.n);
    e.while_nonzero(top, |e| {
        e.inc(flag);
        e.clear(top);
    });
    clear_work(e, a.truncated(b.n));
}

/// Non-destructive unsigned `a < b` into `flag` (zero).
pub fn ult(e: &mut Emitter, sc: &mut Scratch, a: Work, b: Work, flag: Cell) {
    debug_assert_eq!(a.n, b.n);
    let m = sc.mark();
    let ta = Work::alloc(sc, a.n + 1);
    let tb = Work::alloc(sc, b.n);
    copy_work(e, sc, a, ta.truncated(a.n));
    copy_work(e, sc, b, tb);
    ult_destructive(e, ta, tb, flag);
    sc.release(m);
}

/// Non-destructive unsigned `a < constant` into `flag` (zero).
pub fn ult_const(e: &mut Emitter, sc: &mut Scratch, a: Work, c: u64, flag: Cell) {
    let m = sc.mark();
    let ta = Work::alloc(sc, a.n + 1);
    let tb = Work::alloc(sc, a.n);
    copy_work(e, sc, a, ta.truncated(a.n));
    load_const(e, tb, c);
    ult_destructive(e, ta, tb, flag);
    sc.release(m);
}

/// Non-destructive `a == b` into `flag` (zero).
pub fn eq(e: &mut Emitter, sc: &mut Scratch, a: Work, b: Work, flag: Cell) {
    let m = sc.mark();
    let ta = Work::alloc(sc, a.n);
    let tb = Work::alloc(sc, b.n);
    copy_work(e, sc, a, ta);
    copy_work(e, sc, b, tb);
    sub_work(e, ta, tb);
    is_zero_destructive(e, ta, flag);
    sc.release(m);
}

/// Toggle the sign bit of the top limb (maps signed order onto unsigned order).
pub fn flip_sign(e: &mut Emitter, w: Work) {
    e.add(w.limb(w.n - 1), 128);
}

/// Non-destructively compute the sign bit (top bit) of `w` into `flag` (zero).
pub fn sign_bit(e: &mut Emitter, sc: &mut Scratch, w: Work, flag: Cell) {
    let m = sc.mark();
    let top = Work {
        base: w.limb(w.n - 1),
        n: 1,
    };
    let ta = Work::alloc(sc, 2);
    let tb = Work::alloc(sc, 1);
    copy_work(e, sc, top, ta.truncated(1));
    load_const(e, tb, 128);
    // ta < 128  <=>  sign bit clear
    let f = sc.alloc(1);
    ult_destructive(e, ta, tb, f);
    e.inc(flag);
    e.while_nonzero(f, |e| {
        e.dec(f);
        e.dec(flag);
    });
    sc.release(m);
}

// ---------------------------------------------------------------------------
// Shifts
// ---------------------------------------------------------------------------

/// x = x / 2 in place, remainder (0/1) added to `rem`. Requires scratch.
pub fn divmod2(e: &mut Emitter, sc: &mut Scratch, x: Cell, rem: Cell) {
    let m = sc.mark();
    let h = sc.alloc(1);
    let a = sc.alloc(1);
    let t = sc.alloc(1);
    // x[- t+ a[t- h+ a-] t[a+ t-]]
    e.while_nonzero(x, |e| {
        e.dec(x);
        e.inc(t);
        e.while_nonzero(a, |e| {
            e.dec(t);
            e.inc(h);
            e.dec(a);
        });
        e.while_nonzero(t, |e| {
            e.inc(a);
            e.dec(t);
        });
    });
    e.move_cell(h, x);
    e.move_cell(a, rem);
    sc.release(m);
}

/// Logical shift right by one. The bit shifted out is added to `out` (may be any cell, or none).
pub fn shr1(e: &mut Emitter, sc: &mut Scratch, w: Work, out: Option<Cell>) {
    let m = sc.mark();
    let carry = sc.alloc(1);
    for i in (0..w.n).rev() {
        let r = sc.alloc(1);
        divmod2(e, sc, w.limb(i), r);
        // add 128 * carry_in to this limb
        e.while_nonzero(carry, |e| {
            e.dec(carry);
            e.add(w.limb(i), 128);
        });
        if i > 0 {
            e.move_cell(r, carry);
        } else {
            match out {
                Some(o) => e.move_cell(r, o),
                None => e.clear(r),
            }
        }
    }
    sc.release(m);
}

/// Shift left by one. The bit shifted out is added to `out` (if given).
pub fn shl1(e: &mut Emitter, sc: &mut Scratch, w: Work, out: Option<Cell>) {
    let m = sc.mark();
    let carry = sc.alloc(1);
    let h = Work::alloc(sc, 1); // spaced single cell for if_zero
    let hc = h.limb(0);
    for i in 0..w.n {
        let x = w.limb(i);
        let cout = sc.alloc(1);
        e.while_nonzero(x, |e| {
            e.dec(x);
            e.add(hc, 2);
            e.if_zero(hc, |e| e.inc(cout));
        });
        e.move_cell(carry, hc);
        e.move_cell(hc, x);
        if i + 1 < w.n {
            e.move_cell(cout, carry);
        } else {
            match out {
                Some(o) => e.move_cell(cout, o),
                None => e.clear(cout),
            }
        }
    }
    sc.release(m);
}

/// `q = x / 2^k`, `r = x % 2^k` for a limb `x` (destroyed) and `1 <= k <= 7`; `q`, `r` zero.
///
/// A down-counter starting at 2^k is decremented per unit of `x`; each time it
/// reaches zero it is reloaded and the quotient incremented, so the whole shift by
/// `k` bits costs one pass over the limb instead of `k`.
pub fn divmod_pow2(e: &mut Emitter, sc: &mut Scratch, x: Cell, k: u32, q: Cell, r: Cell) {
    debug_assert!((1..=7).contains(&k));
    let m = sc.mark();
    let d = Work::alloc(sc, 1).limb(0); // spaced: `if_zero` needs two zero cells after it
    let step = 1i32 << k;
    e.add(d, step);
    e.while_nonzero(x, |e| {
        e.dec(x);
        e.dec(d);
        e.if_zero(d, |e| {
            e.add(d, step);
            e.inc(q);
        });
    });
    // r = 2^k - d
    e.add(r, step);
    e.while_nonzero(d, |e| {
        e.dec(d);
        e.dec(r);
    });
    sc.release(m);
}

/// Logical shift right by `k` bits (`1 <= k <= 7`) in one pass per limb.
pub fn shr_bits(e: &mut Emitter, sc: &mut Scratch, w: Work, k: u32) {
    if k == 1 {
        return shr1(e, sc, w, None);
    }
    let m = sc.mark();
    let carry = sc.alloc(1); // low k bits of the limb above
    for i in (0..w.n).rev() {
        let x = w.limb(i);
        let q = sc.alloc(1);
        let r = sc.alloc(1);
        divmod_pow2(e, sc, x, k, q, r);
        e.move_cell(q, x);
        e.while_nonzero(carry, |e| {
            e.dec(carry);
            e.add(x, 1 << (8 - k));
        });
        if i > 0 {
            e.move_cell(r, carry);
        } else {
            e.clear(r);
        }
    }
    sc.release(m);
}

/// Shift left by `k` bits (`1 <= k <= 7`) in one pass per limb.
pub fn shl_bits(e: &mut Emitter, sc: &mut Scratch, w: Work, k: u32) {
    if k == 1 {
        return shl1(e, sc, w, None);
    }
    let m = sc.mark();
    let carry = sc.alloc(1); // high k bits of the limb below
    for i in 0..w.n {
        let x = w.limb(i);
        let q = sc.alloc(1);
        let r = sc.alloc(1);
        divmod_pow2(e, sc, x, 8 - k, q, r);
        e.while_nonzero(r, |e| {
            e.dec(r);
            e.add(x, 1 << k);
        });
        e.move_cell(carry, x);
        if i + 1 < w.n {
            e.move_cell(q, carry);
        } else {
            e.clear(q);
        }
    }
    sc.release(m);
}

/// Shift left by a constant.
pub fn shl_const(e: &mut Emitter, sc: &mut Scratch, w: Work, k: u32) {
    let n = w.n;
    if k as usize >= 8 * n {
        clear_work(e, w);
        return;
    }
    let limb_shift = (k / 8) as usize;
    if limb_shift > 0 {
        for i in (0..n).rev() {
            if i >= limb_shift {
                e.clear(w.limb(i));
                e.move_cell(w.limb(i - limb_shift), w.limb(i));
            } else {
                e.clear(w.limb(i));
            }
        }
    }
    if !k.is_multiple_of(8) {
        shl_bits(e, sc, w, k % 8);
    }
}

/// Logical shift right by a constant.
pub fn lshr_const(e: &mut Emitter, sc: &mut Scratch, w: Work, k: u32) {
    let n = w.n;
    if k as usize >= 8 * n {
        clear_work(e, w);
        return;
    }
    let limb_shift = (k / 8) as usize;
    if limb_shift > 0 {
        for i in 0..n {
            e.clear(w.limb(i));
            if i + limb_shift < n {
                e.move_cell(w.limb(i + limb_shift), w.limb(i));
            }
        }
    }
    if !k.is_multiple_of(8) {
        shr_bits(e, sc, w, k % 8);
    }
}

/// Arithmetic shift right by a constant.
pub fn ashr_const(e: &mut Emitter, sc: &mut Scratch, w: Work, k: u32) {
    let n = w.n;
    let k = (k as usize).min(8 * n - 1) as u32;
    if k == 0 {
        return;
    }
    let m = sc.mark();
    let sign = sc.alloc(1);
    sign_bit(e, sc, w, sign);
    lshr_const(e, sc, w, k);
    // OR in the top k bits if negative: they are zero now, so add the mask.
    let bits = 8 * n as u32;
    let mask: u64 = if bits == 64 {
        u64::MAX << (bits - k)
    } else {
        ((1u64 << bits) - 1) & (u64::MAX << (bits - k))
    };
    e.while_nonzero(sign, |e| {
        e.dec(sign);
        for i in 0..n {
            let limb = ((mask >> (8 * i)) & 0xff) as u8;
            if limb != 0 {
                e.add(w.limb(i), limb as i32);
            }
        }
    });
    sc.release(m);
}

/// Shift by a variable amount held in work area `cnt` (destroyed).
/// `kind`: 0 = shl, 1 = lshr, 2 = ashr.
pub fn shift_var(e: &mut Emitter, sc: &mut Scratch, w: Work, cnt: Work, kind: u8) {
    let m = sc.mark();
    // Only the low limb of the count matters for well-defined shifts (< 64).
    for i in 1..cnt.n {
        e.clear(cnt.limb(i));
    }
    let c = cnt.limb(0);
    let sign = sc.alloc(1);
    if kind == 2 {
        sign_bit(e, sc, w, sign);
    }
    e.while_nonzero(c, |e| {
        e.dec(c);
        match kind {
            0 => shl1(e, sc, w, None),
            1 => shr1(e, sc, w, None),
            _ => {
                shr1(e, sc, w, None);
                let m2 = sc.mark();
                let t = sc.alloc(1);
                e.copy_cell(sign, t, sc.alloc(1));
                e.while_nonzero(t, |e| {
                    e.dec(t);
                    e.add(w.limb(w.n - 1), 128);
                });
                sc.release(m2);
            }
        }
    });
    e.clear(sign);
    sc.release(m);
}

// ---------------------------------------------------------------------------
// Multiplication and division
// ---------------------------------------------------------------------------

/// res = a * b (mod 2^(8n)). `a` and `b` are destroyed. `res` must be a zeroed work area.
///
/// The bits of `b` are extracted once and the doublings `a << k` (k = 0..7) are
/// computed once; bit k of limb j of `b` then adds `a << k`, shifted up by j limbs,
/// into the result. Shifting by whole limbs is free, so `a` is shifted only 7 times
/// instead of once per bit.
pub fn mul_work(e: &mut Emitter, sc: &mut Scratch, a: Work, b: Work, res: Work) {
    let n = res.n;
    let m = sc.mark();
    // Bits of b: bit k of limb j at bits + 8j + k.
    let bits = sc.alloc(8 * n);
    for j in 0..n {
        to_bits(e, sc, b.limb(j), bits + 8 * j as Cell);
    }
    // doublings[k] = a << k (mod 2^(8n)); the last one takes `a` itself.
    let mut doublings: Vec<Work> = Vec::with_capacity(8);
    for k in 0..8 {
        if k == 7 {
            doublings.push(a);
        } else {
            let d = Work::alloc(sc, n);
            copy_work(e, sc, a, d);
            doublings.push(d);
        }
        if k < 7 {
            shl1(e, sc, a, None);
        }
    }
    // a now holds a << 7, i.e. doublings[7].
    for j in 0..n {
        for (k, doubling) in doublings.iter().enumerate() {
            let bit = bits + (8 * j + k) as Cell;
            e.while_nonzero(bit, |e| {
                e.dec(bit);
                // res[j..] += doubling
                let upper = Work {
                    base: res.limb(j),
                    n: n - j,
                };
                add_work_keep(e, upper, doubling.truncated(n - j));
            });
        }
    }
    for d in doublings {
        clear_work(e, d);
    }
    sc.release(m);
}

/// res = a * constant. `a` is destroyed, `res` must be zero.
///
/// Horner's scheme over the non-adjacent form of the constant (digits -1, 0, 1, no
/// two adjacent nonzero): res = 2*res + digit*a, runs of zero digits taken as one
/// multi-bit shift. The constant is read as signed, so `x * -10` costs two
/// subtractions rather than thirty additions.
pub fn mul_const(e: &mut Emitter, sc: &mut Scratch, a: Work, c: u64, res: Work) {
    let n = res.n;
    let bits = 8 * n as u32;
    let c = mask(c, bits);
    if c == 0 {
        clear_work(e, a);
        return;
    }
    let mut v: i128 = c as i128;
    if bits < 64 && c >> (bits - 1) == 1 {
        v -= 1i128 << bits;
    } else if bits == 64 && (c as i64) < 0 {
        v = c as i64 as i128;
    }
    // Non-adjacent form, least significant digit first.
    let mut digits: Vec<i8> = Vec::new();
    while v != 0 {
        if v & 1 == 1 {
            let d = 2 - (v & 3) as i8; // 1 or -1
            digits.push(d);
            v -= d as i128;
        } else {
            digits.push(0);
        }
        v /= 2;
    }
    let mut run = 0;
    for (k, &d) in digits.iter().enumerate().rev() {
        if d == 0 {
            run += 1;
            continue;
        }
        if k + 1 < digits.len() {
            shl_const(e, sc, res, run + 1);
        }
        run = 0;
        if d > 0 {
            add_work_keep(e, res, a);
        } else {
            sub_work_keep(e, res, a);
        }
    }
    if run > 0 {
        shl_const(e, sc, res, run);
    }
    clear_work(e, a);
}

/// Unsigned division: q = a / b, r = a % b. `a` and `b` destroyed; `q`, `r` zeroed work areas of n limbs.
///
/// Binary long division, MSB first, with the dividend's bits extracted once (so the
/// dividend is never shifted), quotient bits set in place (so the quotient is never
/// shifted) and only the remainder shifted, which stays below the divisor. Each step
/// subtracts the divisor from the remainder (which has an extra limb) and adds it
/// back when the result went negative. Steps while the remainder is still zero and
/// the incoming bit is zero are skipped at runtime, so small dividends are cheap.
pub fn udivrem(e: &mut Emitter, sc: &mut Scratch, a: Work, b: Work, q: Work, r: Work) {
    let n = a.n;
    let m = sc.mark();
    // Remainder with an extra limb that becomes 255 on a negative result, and the
    // per-step temporaries right next to it (the step code is emitted 8n times, so
    // its walking distances matter most); the dividend bits come after.
    let rr = Work::alloc(sc, n + 1);
    let started = sc.alloc(1); // becomes 1 at the first nonzero bit
    let temps_size = n + 12;
    let temps = sc.alloc(temps_size);
    let bits = sc.alloc(8 * n);
    for j in 0..n {
        to_bits(e, sc, a.limb(j), bits + 8 * j as Cell);
    }
    for i in (0..8 * n).rev() {
        let bit = bits + i as Cell;
        let mut ts = Scratch::new(temps);
        // started |= bit
        let t = ts.alloc(1);
        e.copy_cell(bit, t, ts.alloc(1));
        e.while_nonzero(t, |e| {
            e.dec(t);
            e.clear(started);
            e.inc(started);
        });
        // if started: rr = 2*rr + bit; rr -= b; if negative add b back else set bit i of q
        let f = ts.alloc(1);
        e.copy_cell(started, f, ts.alloc(1));
        e.while_nonzero(f, |e| {
            e.dec(f);
            shl1(e, &mut ts, rr.truncated(n), None);
            e.move_cell(bit, rr.limb(0));
            sub_work_keep(e, rr, b);
            let positive = ts.alloc(1);
            e.inc(positive);
            e.if_nonzero(rr.limb(n), ts.alloc(1), |e| {
                e.dec(positive);
                add_work_keep(e, rr, b); // restores rr and clears the top limb
            });
            e.while_nonzero(positive, |e| {
                e.dec(positive);
                e.add(q.limb(i / 8), 1 << (i % 8));
            });
        });
        e.clear(bit); // the bit was consumed when started, and is zero otherwise
        assert!(ts.used() <= temps_size, "division temporaries overflow");
    }
    e.clear(started);
    clear_work(e, b);
    move_work(e, rr.truncated(n), r);
    sc.release(m);
}

/// Signed division. `a`, `b` destroyed; `q`, `r` zeroed work areas.
pub fn sdivrem(e: &mut Emitter, sc: &mut Scratch, a: Work, b: Work, q: Work, r: Work) {
    let m = sc.mark();
    let sa = sc.alloc(1);
    let sb = sc.alloc(1);
    sign_bit(e, sc, a, sa);
    sign_bit(e, sc, b, sb);
    let t = sc.alloc(1);
    // Negate operands if negative, remembering the signs.
    e.copy_cell(sa, t, sc.alloc(1));
    e.while_nonzero(t, |e| {
        e.dec(t);
        negate_work(e, sc, a);
    });
    e.copy_cell(sb, t, sc.alloc(1));
    e.while_nonzero(t, |e| {
        e.dec(t);
        negate_work(e, sc, b);
    });
    udivrem(e, sc, a, b, q, r);
    // quotient negative iff signs differ; remainder has the sign of a.
    e.copy_cell(sa, t, sc.alloc(1));
    e.while_nonzero(t, |e| {
        e.dec(t);
        negate_work(e, sc, r);
    });
    let x = sc.alloc(1);
    e.move_cell(sa, x);
    e.move_cell(sb, x);
    // x in {0,1,2}: negate q if x == 1
    let u = sc.alloc(1);
    e.while_nonzero(x, |e| {
        e.dec(x);
        e.inc(u);
        e.while_nonzero(x, |e| {
            e.dec(x);
            e.dec(u);
        });
    });
    e.while_nonzero(u, |e| {
        e.dec(u);
        negate_work(e, sc, q);
    });
    sc.release(m);
}

// ---------------------------------------------------------------------------
// Bitwise operations
// ---------------------------------------------------------------------------

/// Decompose a limb into 8 bit cells (bit k at `bits + k`), destroying the limb.
///
/// The limb is first moved next to the bit cells (the scratch cells handed out right
/// after `bits` are usually adjacent to it), so the halving loops stay short.
pub fn to_bits(e: &mut Emitter, sc: &mut Scratch, x: Cell, bits: Cell) {
    let m = sc.mark();
    let t = sc.alloc(1);
    e.move_cell(x, t);
    for k in 0..8 {
        if k < 7 {
            divmod2(e, sc, t, bits + k);
        } else {
            e.move_cell(t, bits + 7);
        }
    }
    sc.release(m);
}

/// Bitwise op per limb. `kind`: 0 = and, 1 = or, 2 = xor. `b` is destroyed, result replaces `a`.
pub fn bitwise(e: &mut Emitter, sc: &mut Scratch, a: Work, b: Work, kind: u8) {
    let m = sc.mark();
    let ba = sc.alloc(8);
    let bb = sc.alloc(8);
    let acc = sc.alloc(1);
    let t = sc.alloc(1);
    for i in 0..a.n {
        to_bits(e, sc, a.limb(i), ba);
        to_bits(e, sc, b.limb(i), bb);
        // Recombine from the top bit down: acc = 2*acc + (x op y), next to the bits.
        for k in (0..8).rev() {
            if k < 7 {
                double_cell(e, acc, t);
            }
            let (x, y) = (ba + k, bb + k);
            match kind {
                0 => {
                    e.while_nonzero(x, |e| {
                        e.dec(x);
                        e.while_nonzero(y, |e| {
                            e.dec(y);
                            e.inc(acc);
                        });
                    });
                    e.clear(y);
                }
                1 => {
                    e.while_nonzero(x, |e| {
                        e.dec(x);
                        e.clear(y);
                        e.inc(acc);
                    });
                    e.while_nonzero(y, |e| {
                        e.dec(y);
                        e.inc(acc);
                    });
                }
                _ => {
                    e.move_cell(x, t);
                    e.move_cell(y, t);
                    // t in {0,1,2}; xor bit is (t == 1)
                    e.while_nonzero(t, |e| {
                        e.dec(t);
                        e.inc(acc);
                        e.while_nonzero(t, |e| {
                            e.dec(t);
                            e.dec(acc);
                        });
                    });
                }
            }
        }
        e.move_cell(acc, a.limb(i));
    }
    sc.release(m);
}

/// x = 2*x using the zero cell `t` (left zero).
fn double_cell(e: &mut Emitter, x: Cell, t: Cell) {
    e.while_nonzero(x, |e| {
        e.dec(x);
        e.add(t, 2);
    });
    e.move_cell(t, x);
}

/// Bitwise op with a constant mask, per limb. Limbs where the mask is 0x00 or 0xFF are
/// handled without bit decomposition.
pub fn bitwise_const(e: &mut Emitter, sc: &mut Scratch, a: Work, c: u64, kind: u8) {
    let m = sc.mark();
    let ba = sc.alloc(8);
    let acc = sc.alloc(1);
    let t = sc.alloc(1);
    for i in 0..a.n {
        let limb = ((c >> (8 * i)) & 0xff) as u8;
        let x = a.limb(i);
        match (kind, limb) {
            (0, 0x00) => e.clear(x),
            (0, 0xff) => {}
            (1, 0x00) => {}
            (1, 0xff) => e.set(x, 0xff),
            (2, 0x00) => {}
            (2, 0xff) => {
                // 255 - x
                e.add(t, 255);
                e.move_cell_neg(x, t);
                e.move_cell(t, x);
            }
            _ => {
                to_bits(e, sc, x, ba);
                for k in (0..8).rev() {
                    if k < 7 {
                        double_cell(e, acc, t);
                    }
                    let mb = (limb >> k) & 1 == 1;
                    let bit = ba + k;
                    match (kind, mb) {
                        (0, true) | (1, false) | (2, false) => e.move_cell(bit, acc),
                        (0, false) => e.clear(bit),
                        (1, true) => {
                            e.clear(bit);
                            e.inc(acc);
                        }
                        (2, true) => {
                            e.inc(acc);
                            e.move_cell_neg(bit, acc);
                        }
                        _ => unreachable!(),
                    }
                }
                e.move_cell(acc, x);
            }
        }
    }
    sc.release(m);
}

// ---------------------------------------------------------------------------
// Width conversions
// ---------------------------------------------------------------------------

/// Sign-extend `src` (n limbs) into zeroed `dst` (more limbs). `src` destroyed.
pub fn sext_work(e: &mut Emitter, sc: &mut Scratch, src: Work, dst: Work) {
    let m = sc.mark();
    let sign = sc.alloc(1);
    sign_bit(e, sc, src, sign);
    move_work(e, src, dst.truncated(src.n));
    e.while_nonzero(sign, |e| {
        e.dec(sign);
        for i in src.n..dst.n {
            e.add(dst.limb(i), 255);
        }
    });
    sc.release(m);
}

/// Sign-extend a value whose meaningful width is `bits` (e.g. 1 for i1) that is stored
/// zero-extended in `src`. Result in zeroed `dst`.
pub fn sext_bits(e: &mut Emitter, sc: &mut Scratch, src: Work, bits: u32, dst: Work) {
    if bits.is_multiple_of(8) {
        sext_work(e, sc, src, dst);
        return;
    }
    // Only i1 is supported among non-byte widths.
    debug_assert_eq!(bits, 1);
    let m = sc.mark();
    let f = sc.alloc(1);
    e.move_cell(src.limb(0), f);
    clear_work(e, src);
    e.while_nonzero(f, |e| {
        e.dec(f);
        for i in 0..dst.n {
            e.add(dst.limb(i), 255);
        }
    });
    sc.release(m);
}

/// Truncate `w` (any limbs) to `bits` bits in place (masking the top limb if needed).
pub fn trunc_bits(e: &mut Emitter, sc: &mut Scratch, w: Work, bits: u32) {
    let n = limbs(bits);
    for i in n..w.n {
        e.clear(w.limb(i));
    }
    if !bits.is_multiple_of(8) {
        let mask = (1u64 << bits) - 1;
        bitwise_const(e, sc, w.truncated(n), mask, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::tests::run_tape;

    const SC: Cell = 100;

    fn mk() -> (Emitter, Scratch) {
        (Emitter::new(), Scratch::new(SC))
    }

    fn finish(e: &Emitter, sc: &Scratch) -> Vec<u8> {
        let t = run_tape(e);
        // scratch must be clean
        let hw = sc.high_water as usize;
        assert!(
            t[SC as usize..hw + 1].iter().all(|&x| x == 0),
            "scratch not clean: {:?}",
            &t[SC as usize..hw + 1]
        );
        t
    }

    fn packed(t: &[u8], base: usize, n: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            v |= (t[base + i] as u64) << (8 * i);
        }
        v
    }

    fn mask(n: usize) -> u64 {
        if n >= 8 {
            u64::MAX
        } else {
            (1u64 << (8 * n)) - 1
        }
    }

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn samples(n: usize) -> Vec<u64> {
        let mut s = 0x1234_5678_9abc_def1u64;
        let mut v = vec![
            0, 1, 2, 127, 128, 255, 256, 0x7fff, 0x8000, 0xffff, 0xff00, 0x0102,
        ];
        for _ in 0..6 {
            v.push(rng(&mut s));
        }
        v.into_iter().map(|x| x & mask(n)).collect()
    }

    /// Run `op` for operands a,b (n limbs), result at packed cell 0.
    fn binop(
        n: usize,
        a: u64,
        b: u64,
        op: impl Fn(&mut Emitter, &mut Scratch, Work, Work, Work),
    ) -> u64 {
        let (mut e, mut sc) = mk();
        let wa = Work::alloc(&mut sc, n + 1);
        let wb = Work::alloc(&mut sc, n);
        let wr = Work::alloc(&mut sc, n);
        load_const(&mut e, wa.truncated(n), a);
        load_const(&mut e, wb, b);
        op(&mut e, &mut sc, wa, wb, wr);
        store_packed(&mut e, wr, 0);
        clear_work(&mut e, wa);
        clear_work(&mut e, wb);
        let t = finish(&e, &sc);
        packed(&t, 0, n)
    }

    #[test]
    fn add_sub() {
        for n in [1usize, 2, 4] {
            for &a in &samples(n) {
                for &b in &samples(n) {
                    let r = binop(n, a, b, |e, _sc, wa, wb, wr| {
                        move_work(e, wa.truncated(wr.n), wr);
                        add_work(e, wr, wb);
                    });
                    assert_eq!(r, a.wrapping_add(b) & mask(n), "add n={} {} {}", n, a, b);
                    let r = binop(n, a, b, |e, _sc, wa, wb, wr| {
                        move_work(e, wa.truncated(wr.n), wr);
                        sub_work(e, wr, wb);
                    });
                    assert_eq!(r, a.wrapping_sub(b) & mask(n), "sub n={} {} {}", n, a, b);
                }
            }
        }
    }

    #[test]
    fn compare() {
        for n in [1usize, 2] {
            for &a in &samples(n) {
                for &b in &samples(n) {
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        ult(e, sc, wa.truncated(wr.n), wb, wr.limb(0));
                    });
                    assert_eq!(r, (a < b) as u64, "ult n={} {} {}", n, a, b);
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        eq(e, sc, wa.truncated(wr.n), wb, wr.limb(0));
                    });
                    assert_eq!(r, (a == b) as u64, "eq n={} {} {}", n, a, b);
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        let x = wa.truncated(wr.n);
                        flip_sign(e, x);
                        flip_sign(e, wb);
                        ult(e, sc, x, wb, wr.limb(0));
                    });
                    let sa = sign_extend(a, n);
                    let sb = sign_extend(b, n);
                    assert_eq!(r, (sa < sb) as u64, "slt n={} {} {}", n, a, b);
                }
            }
        }
    }

    fn sign_extend(v: u64, n: usize) -> i64 {
        let bits = 8 * n as u32;
        if bits >= 64 {
            v as i64
        } else {
            ((v << (64 - bits)) as i64) >> (64 - bits)
        }
    }

    #[test]
    fn multiply() {
        for n in [1usize, 2] {
            for &a in &samples(n) {
                for &b in &samples(n) {
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        mul_work(e, sc, wa.truncated(wr.n), wb, wr);
                    });
                    assert_eq!(r, a.wrapping_mul(b) & mask(n), "mul n={} {} {}", n, a, b);
                }
                for c in [0u64, 1, 2, 3, 10, 255, 256, 1000] {
                    let r = binop(n, a, c, |e, sc, wa, wb, wr| {
                        clear_work(e, wb);
                        mul_const(e, sc, wa.truncated(wr.n), c, wr);
                    });
                    assert_eq!(r, a.wrapping_mul(c) & mask(n), "mulc n={} {} {}", n, a, c);
                }
            }
        }
    }

    #[test]
    fn divide() {
        for n in [1usize, 2] {
            for &a in &samples(n) {
                for &b in &samples(n) {
                    if b == 0 {
                        continue;
                    }
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        let rem = Work::alloc(sc, wr.n);
                        udivrem(e, sc, wa.truncated(wr.n), wb, wr, rem);
                        clear_work(e, rem);
                    });
                    assert_eq!(r, a / b, "udiv n={} {} {}", n, a, b);
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        let q = Work::alloc(sc, wr.n);
                        udivrem(e, sc, wa.truncated(wr.n), wb, q, wr);
                        clear_work(e, q);
                    });
                    assert_eq!(r, a % b, "urem n={} {} {}", n, a, b);
                    let sa = sign_extend(a, n);
                    let sb = sign_extend(b, n);
                    if sb == -1 && sa == (1i64 << (8 * n - 1)).wrapping_neg() {
                        continue; // overflow, UB in LLVM
                    }
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        let rem = Work::alloc(sc, wr.n);
                        sdivrem(e, sc, wa.truncated(wr.n), wb, wr, rem);
                        clear_work(e, rem);
                    });
                    assert_eq!(r, (sa / sb) as u64 & mask(n), "sdiv n={} {} {}", n, sa, sb);
                    let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                        let q = Work::alloc(sc, wr.n);
                        sdivrem(e, sc, wa.truncated(wr.n), wb, q, wr);
                        clear_work(e, q);
                    });
                    assert_eq!(r, (sa % sb) as u64 & mask(n), "srem n={} {} {}", n, sa, sb);
                }
            }
        }
    }

    #[test]
    fn shifts() {
        for n in [1usize, 2, 3] {
            for &a in &samples(n) {
                for k in [0u32, 1, 3, 7, 8, 9, 15] {
                    if k as usize >= 8 * n {
                        continue;
                    }
                    let r = binop(n, a, 0, |e, sc, wa, _wb, wr| {
                        move_work(e, wa.truncated(wr.n), wr);
                        shl_const(e, sc, wr, k);
                    });
                    assert_eq!(r, (a << k) & mask(n), "shl n={} {} {}", n, a, k);
                    let r = binop(n, a, 0, |e, sc, wa, _wb, wr| {
                        move_work(e, wa.truncated(wr.n), wr);
                        lshr_const(e, sc, wr, k);
                    });
                    assert_eq!(r, a >> k, "lshr n={} {} {}", n, a, k);
                    let r = binop(n, a, 0, |e, sc, wa, _wb, wr| {
                        move_work(e, wa.truncated(wr.n), wr);
                        ashr_const(e, sc, wr, k);
                    });
                    assert_eq!(
                        r,
                        (sign_extend(a, n) >> k) as u64 & mask(n),
                        "ashr n={} {} {}",
                        n,
                        a,
                        k
                    );
                    for kind in 0..3u8 {
                        let r = binop(n, a, k as u64, |e, sc, wa, wb, wr| {
                            move_work(e, wa.truncated(wr.n), wr);
                            shift_var(e, sc, wr, wb, kind);
                        });
                        let exp = match kind {
                            0 => (a << k) & mask(n),
                            1 => a >> k,
                            _ => (sign_extend(a, n) >> k) as u64 & mask(n),
                        };
                        assert_eq!(r, exp, "shift_var kind={} n={} {} {}", kind, n, a, k);
                    }
                }
            }
        }
    }

    #[test]
    fn bitwise_ops() {
        for n in [1usize, 2] {
            for &a in &samples(n) {
                for &b in &samples(n) {
                    for kind in 0..3u8 {
                        let exp = match kind {
                            0 => a & b,
                            1 => a | b,
                            _ => a ^ b,
                        };
                        let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                            move_work(e, wa.truncated(wr.n), wr);
                            bitwise(e, sc, wr, wb, kind);
                        });
                        assert_eq!(r, exp, "bitwise kind={} n={} {} {}", kind, n, a, b);
                        let r = binop(n, a, b, |e, sc, wa, wb, wr| {
                            move_work(e, wa.truncated(wr.n), wr);
                            clear_work(e, wb);
                            bitwise_const(e, sc, wr, b, kind);
                        });
                        assert_eq!(r, exp, "bitwise_const kind={} n={} {} {}", kind, n, a, b);
                    }
                }
            }
        }
    }

    #[test]
    fn extend_negate() {
        for &a in &samples(1) {
            let r = binop(2, a, 0, |e, sc, wa, _wb, wr| {
                sext_work(e, sc, wa.truncated(1), wr);
            });
            assert_eq!(r, sign_extend(a, 1) as u64 & mask(2), "sext {}", a);
            let r = binop(2, a, 0, |e, sc, wa, _wb, wr| {
                move_work(e, wa.truncated(2), wr);
                negate_work(e, sc, wr);
            });
            assert_eq!(r, a.wrapping_neg() & mask(2), "neg {}", a);
            let r = binop(1, a, 0, |e, sc, wa, _wb, wr| {
                sign_bit(e, sc, wa.truncated(1), wr.limb(0));
            });
            assert_eq!(r, (a >> 7) & 1, "sign {}", a);
        }
    }
}
