//! Low-level Brainfuck emitter.
//!
//! The emitter tracks the head position as an offset relative to a logical
//! origin (normally the base of the current stack frame). All generated code is
//! position independent: it only ever moves the head relatively, which is what
//! makes the shifting stack frames possible.
//!
//! The emitter also provides the constant-time control-flow idioms the rest of
//! the code generator is built on (`if_zero`, `if_nonzero`, `if_else`).

/// A cell position relative to the current logical origin.
pub type Cell = isize;

pub struct Emitter {
    code: Vec<u8>,
    pos: Cell,
    pub comments: bool,
    line_len: usize,
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Emitter {
    pub fn new() -> Self {
        Emitter { code: Vec::new(), pos: 0, comments: false, line_len: 0 }
    }

    pub fn code(&self) -> &[u8] {
        &self.code
    }

    pub fn into_code(self) -> Vec<u8> {
        self.code
    }

    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn pos(&self) -> Cell {
        self.pos
    }

    /// Re-anchor the logical origin: declare the current head position to be `pos`.
    pub fn set_pos(&mut self, pos: Cell) {
        self.pos = pos;
    }

    /// Append raw Brainfuck text. Does not change the tracked position.
    pub fn raw(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.push(b);
        }
    }

    fn push(&mut self, b: u8) {
        self.code.push(b);
        self.line_len += 1;
        if self.line_len >= 100 && !self.comments {
            self.code.push(b'\n');
            self.line_len = 0;
        }
    }

    fn rep(&mut self, b: u8, n: usize) {
        for _ in 0..n {
            self.push(b);
        }
    }

    /// Emit a comment (only when comments are enabled). Brainfuck characters are stripped.
    pub fn comment(&mut self, s: &str) {
        if !self.comments {
            return;
        }
        if self.line_len > 0 {
            self.code.push(b'\n');
        }
        self.code.extend_from_slice(b"# ");
        for &b in s.as_bytes() {
            if !b"+-<>[].,".contains(&b) && b != b'\n' {
                self.code.push(b);
            } else if b == b'\n' {
                self.code.push(b' ');
            }
        }
        self.code.push(b'\n');
        self.line_len = 0;
    }

    pub fn goto(&mut self, cell: Cell) {
        let d = cell - self.pos;
        if d > 0 {
            self.rep(b'>', d as usize);
        } else if d < 0 {
            self.rep(b'<', (-d) as usize);
        }
        self.pos = cell;
    }

    /// Add a constant (mod 256) to a cell.
    pub fn add(&mut self, cell: Cell, delta: i32) {
        let d = delta.rem_euclid(256) as usize;
        if d == 0 {
            return;
        }
        self.goto(cell);
        if d <= 128 {
            self.rep(b'+', d);
        } else {
            self.rep(b'-', 256 - d);
        }
    }

    pub fn inc(&mut self, cell: Cell) {
        self.add(cell, 1);
    }

    pub fn dec(&mut self, cell: Cell) {
        self.add(cell, -1);
    }

    /// Set a cell to zero.
    pub fn clear(&mut self, cell: Cell) {
        self.goto(cell);
        self.raw("[-]");
    }

    /// Set a cell to a constant (clears it first).
    pub fn set(&mut self, cell: Cell, value: u8) {
        self.clear(cell);
        self.add(cell, value as i32);
    }

    /// Set a cell to a constant using a zero scratch cell for a multiplicative
    /// short form. The scratch cell is left zero.
    pub fn set_using(&mut self, cell: Cell, value: u8, scratch: Cell) {
        self.clear(cell);
        if value == 0 {
            return;
        }
        // Find factors a*b + c = value minimizing roughly a+b+c+overhead.
        let mut best: Option<(usize, usize, i32)> = None;
        let mut best_cost = value as usize;
        for a in 2..=16usize {
            for b in 2..=16usize {
                let prod = a * b;
                if prod > 255 {
                    continue;
                }
                let c = value as i32 - prod as i32;
                let cost = a + b + c.unsigned_abs() as usize + 8;
                if c.abs() <= 8 && cost < best_cost {
                    best_cost = cost;
                    best = Some((a, b, c));
                }
            }
        }
        match best {
            Some((a, b, c)) => {
                self.add(scratch, a as i32);
                self.goto(scratch);
                self.raw("[-");
                self.add(cell, b as i32);
                self.goto(scratch);
                self.raw("]");
                self.add(cell, c);
            }
            None => self.add(cell, value as i32),
        }
    }

    pub fn out(&mut self, cell: Cell) {
        self.goto(cell);
        self.push(b'.');
    }

    pub fn input(&mut self, cell: Cell) {
        self.goto(cell);
        self.push(b',');
    }

    /// `while (cell != 0) { body }`. The body may move the head freely; it is brought
    /// back to `cell` before the loop test.
    pub fn while_nonzero(&mut self, cell: Cell, body: impl FnOnce(&mut Emitter)) {
        self.goto(cell);
        self.push(b'[');
        body(self);
        self.goto(cell);
        self.push(b']');
    }

    /// dst += src; src = 0.
    pub fn move_cell(&mut self, src: Cell, dst: Cell) {
        debug_assert_ne!(src, dst);
        self.while_nonzero(src, |e| {
            e.dec(src);
            e.inc(dst);
        });
    }

    /// dst1 += src; dst2 += src; src = 0.
    pub fn move_cell2(&mut self, src: Cell, dst1: Cell, dst2: Cell) {
        self.while_nonzero(src, |e| {
            e.dec(src);
            e.inc(dst1);
            e.inc(dst2);
        });
    }

    /// dst -= src; src = 0.
    pub fn move_cell_neg(&mut self, src: Cell, dst: Cell) {
        self.while_nonzero(src, |e| {
            e.dec(src);
            e.dec(dst);
        });
    }

    /// dst += src, preserving src. `tmp` must be zero and is left zero.
    pub fn copy_cell(&mut self, src: Cell, dst: Cell, tmp: Cell) {
        debug_assert!(src != dst && src != tmp && dst != tmp);
        self.move_cell2(src, dst, tmp);
        self.move_cell(tmp, src);
    }

    /// `if (x == 0) { body }` in constant time without destroying `x`.
    ///
    /// Requires cells `x+1` and `x+2` to be zero; they are zero again afterwards.
    /// The body must not touch `x+1` and `x+2`, and must leave the head where it found it.
    pub fn if_zero(&mut self, x: Cell, body: impl FnOnce(&mut Emitter)) {
        self.dec(x + 2);
        self.goto(x);
        self.push(b'[');
        self.goto(x + 1);
        self.push(b']');
        // Real head: x+1 if x != 0, x if x == 0. Nominally treat it as x+1.
        self.set_pos(x + 1);
        self.push(b'>');
        self.push(b'+');
        self.push(b'[');
        self.push(b'-');
        // Body executes with the head really at x+1.
        self.set_pos(x + 1);
        body(self);
        self.goto(x + 1);
        self.push(b'>');
        self.push(b'+');
        self.push(b']');
        self.set_pos(x + 2);
    }

    /// `if (x != 0) { body }` in constant time without destroying `x`.
    ///
    /// Requires `x+1`, `x+2` and `flag` to be zero scratch cells (left zero).
    pub fn if_nonzero(&mut self, x: Cell, flag: Cell, body: impl FnOnce(&mut Emitter)) {
        self.inc(flag);
        self.if_zero(x, |e| e.dec(flag));
        self.goto(flag);
        self.push(b'[');
        self.push(b'-');
        body(self);
        self.goto(flag);
        self.push(b']');
    }

    /// `if (x == 0) { then } else { otherwise }` in constant time without destroying `x`.
    ///
    /// Requires `x+1`, `x+2` and `flag` to be zero scratch cells (left zero).
    pub fn if_else(
        &mut self,
        x: Cell,
        flag: Cell,
        then: impl FnOnce(&mut Emitter),
        otherwise: impl FnOnce(&mut Emitter),
    ) {
        self.inc(flag);
        self.if_zero(x, |e| {
            e.dec(flag);
            then(e);
        });
        self.goto(flag);
        self.push(b'[');
        self.push(b'-');
        otherwise(self);
        self.goto(flag);
        self.push(b']');
    }

    // ----- begin/end variants of the idioms, for callers that cannot use closures -----

    pub fn begin_while(&mut self, cell: Cell) {
        self.goto(cell);
        self.push(b'[');
    }

    pub fn end_while(&mut self, cell: Cell) {
        self.goto(cell);
        self.push(b']');
    }

    /// Open an `if (x == 0)` region; close with [`end_if_zero`]. Same requirements as [`if_zero`].
    pub fn begin_if_zero(&mut self, x: Cell) {
        self.dec(x + 2);
        self.goto(x);
        self.push(b'[');
        self.goto(x + 1);
        self.push(b']');
        self.set_pos(x + 1);
        self.push(b'>');
        self.push(b'+');
        self.push(b'[');
        self.push(b'-');
        self.set_pos(x + 1);
    }

    pub fn end_if_zero(&mut self, x: Cell) {
        self.goto(x + 1);
        self.push(b'>');
        self.push(b'+');
        self.push(b']');
        self.set_pos(x + 2);
    }

    /// Open an `if (x != 0)` region using zero scratch `flag`; close with [`end_if_nonzero`].
    pub fn begin_if_nonzero(&mut self, x: Cell, flag: Cell) {
        self.inc(flag);
        self.begin_if_zero(x);
        self.dec(flag);
        self.end_if_zero(x);
        self.goto(flag);
        self.push(b'[');
        self.push(b'-');
    }

    pub fn end_if_nonzero(&mut self, flag: Cell) {
        self.goto(flag);
        self.push(b']');
    }

    /// Open an if/else on `x == 0`: the code emitted until [`else_if_else`] runs when x == 0,
    /// the code until [`end_if_else`] runs otherwise.
    pub fn begin_if_else(&mut self, x: Cell, flag: Cell) {
        self.inc(flag);
        self.begin_if_zero(x);
        self.dec(flag);
    }

    pub fn else_if_else(&mut self, x: Cell, flag: Cell) {
        self.end_if_zero(x);
        self.goto(flag);
        self.push(b'[');
        self.push(b'-');
    }

    pub fn end_if_else(&mut self, flag: Cell) {
        self.goto(flag);
        self.push(b']');
    }

    /// Execute `body` while the head is logically at `cell`, asserting it returns there.
    pub fn at(&mut self, cell: Cell, body: impl FnOnce(&mut Emitter)) {
        self.goto(cell);
        body(self);
        self.goto(cell);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bfi::Program;

    /// Run emitted code on a fresh tape and return the tape.
    pub fn run_tape(e: &Emitter) -> Vec<u8> {
        let src = String::from_utf8(e.code().to_vec()).unwrap();
        let (_, tape, _) = Program::parse(&src).unwrap().run_with_tape(b"", Some(100_000_000)).unwrap();
        tape
    }

    #[test]
    fn set_and_copy() {
        let mut e = Emitter::new();
        e.set(0, 200);
        e.copy_cell(0, 1, 2);
        e.set_using(5, 123, 6);
        e.set_using(7, 255, 6);
        e.set_using(8, 1, 6);
        let t = run_tape(&e);
        assert_eq!(&t[..9], &[200, 200, 0, 0, 0, 123, 0, 255, 1]);
    }

    #[test]
    fn if_zero_both_ways() {
        for x in [0u8, 1, 7, 255] {
            let mut e = Emitter::new();
            e.set(0, x);
            // cells 1,2 zero; result into 5
            e.if_zero(0, |e| e.add(5, 42));
            e.if_nonzero(0, 3, |e| e.add(6, 17));
            e.if_else(0, 3, |e| e.add(7, 1), |e| e.add(7, 2));
            let t = run_tape(&e);
            assert_eq!(t[0], x);
            assert_eq!(&t[1..4], &[0, 0, 0]);
            if x == 0 {
                assert_eq!(&t[5..8], &[42, 0, 1]);
            } else {
                assert_eq!(&t[5..8], &[0, 17, 2]);
            }
        }
    }

    #[test]
    fn nested_if_zero() {
        let mut e = Emitter::new();
        e.set(0, 0);
        e.set(3, 0);
        e.if_zero(0, |e| e.if_zero(3, |e| e.add(6, 9)));
        let t = run_tape(&e);
        assert_eq!(t[6], 9);
        assert_eq!(&t[1..3], &[0, 0]);
        assert_eq!(&t[4..6], &[0, 0]);
    }
}
