//! An optimizing Brainfuck interpreter.
//!
//! Cells are 8-bit and wrap. The tape grows on demand to the right; moving the
//! head left of cell 0 is an error. On end of input the cell is left unchanged.
//!
//! Besides run-length encoding of `+-<>`, the interpreter recognizes three loop
//! shapes that the code generator relies on for performance and executes them in
//! constant time:
//!
//! * `[-]` clears a cell,
//! * balanced loops such as `[->>+<<]` or `[-<<<+++>>>]` that decrement the loop
//!   cell by one and add constants to other cells (multiply-add loops),
//! * scan loops such as `[>]` or `[<<<]` that only move the head.

use anyhow::{anyhow, bail, Result};
use std::io::{Read, Write};

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Add(u8),
    Move(isize),
    Out,
    In,
    /// Jump past the matching `LoopEnd` if the current cell is zero.
    LoopStart(usize),
    /// Jump back to the matching `LoopStart` if the current cell is nonzero.
    LoopEnd(usize),
    Clear,
    /// For each (offset, factor): tape[p + offset] += tape[p] * factor; then tape[p] = 0.
    MulAdd(Vec<(isize, u8)>),
    /// Move by `stride` until a zero cell is found.
    Scan(isize),
}

/// A compiled Brainfuck program.
pub struct Program {
    ops: Vec<Op>,
}

/// Execution statistics.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Number of (optimized) operations executed.
    pub steps: u64,
    /// Highest tape index touched.
    pub max_cell: usize,
}

impl Program {
    pub fn parse(src: &str) -> Result<Program> {
        // Pass 1: tokenize with RLE.
        let mut ops: Vec<Op> = Vec::new();
        let mut depth_stack: Vec<usize> = Vec::new();
        for c in src.bytes() {
            match c {
                b'+' | b'-' => {
                    let d: u8 = if c == b'+' { 1 } else { 255 };
                    if let Some(Op::Add(a)) = ops.last_mut() {
                        *a = a.wrapping_add(d);
                        if *a == 0 {
                            ops.pop();
                        }
                    } else {
                        ops.push(Op::Add(d));
                    }
                }
                b'<' | b'>' => {
                    let d: isize = if c == b'>' { 1 } else { -1 };
                    if let Some(Op::Move(m)) = ops.last_mut() {
                        *m += d;
                        if *m == 0 {
                            ops.pop();
                        }
                    } else {
                        ops.push(Op::Move(d));
                    }
                }
                b'.' => ops.push(Op::Out),
                b',' => ops.push(Op::In),
                b'[' => {
                    depth_stack.push(ops.len());
                    ops.push(Op::LoopStart(0));
                }
                b']' => {
                    let start = depth_stack
                        .pop()
                        .ok_or_else(|| anyhow!("unmatched ']'"))?;
                    // Try to recognize optimizable loop shapes.
                    let body = &ops[start + 1..];
                    if let Some(opt) = optimize_loop(body) {
                        ops.truncate(start);
                        ops.push(opt);
                    } else {
                        let end = ops.len();
                        ops.push(Op::LoopEnd(start));
                        ops[start] = Op::LoopStart(end);
                    }
                }
                _ => {}
            }
        }
        if !depth_stack.is_empty() {
            bail!("unmatched '['");
        }
        Ok(Program { ops })
    }

    /// Run the program with the given input, returning the output.
    pub fn run_with_input(&self, input: &[u8], max_steps: Option<u64>) -> Result<(Vec<u8>, Stats)> {
        let mut out = Vec::new();
        let mut inp = input;
        let stats = self.run(&mut inp, &mut out, max_steps)?;
        Ok((out, stats))
    }

    /// Run the program and return output and the final tape (for tests and debugging).
    pub fn run_with_tape(&self, input: &[u8], max_steps: Option<u64>) -> Result<(Vec<u8>, Vec<u8>, Stats)> {
        let mut out = Vec::new();
        let mut inp = input;
        let mut tape: Vec<u8> = vec![0; 1 << 16];
        let stats = self.run_on_tape(&mut inp, &mut out, max_steps, &mut tape)?;
        Ok((out, tape, stats))
    }

    /// Run the program against arbitrary reader / writer.
    pub fn run(&self, input: &mut dyn Read, output: &mut dyn Write, max_steps: Option<u64>) -> Result<Stats> {
        let mut tape: Vec<u8> = vec![0; 1 << 16];
        self.run_on_tape(input, output, max_steps, &mut tape)
    }

    fn run_on_tape(
        &self,
        input: &mut dyn Read,
        output: &mut dyn Write,
        max_steps: Option<u64>,
        tape: &mut Vec<u8>,
    ) -> Result<Stats> {
        let mut p: usize = 0;
        let mut pc: usize = 0;
        let mut stats = Stats::default();
        let ops = &self.ops;
        let n = ops.len();
        let max_steps = max_steps.unwrap_or(u64::MAX);
        let mut outbuf: Vec<u8> = Vec::new();
        macro_rules! ensure {
            ($idx:expr) => {{
                let idx = $idx;
                if idx >= tape.len() {
                    tape.resize((idx + 1).max(tape.len() * 2), 0);
                }
                if idx > stats.max_cell {
                    stats.max_cell = idx;
                }
            }};
        }
        while pc < n {
            stats.steps += 1;
            if stats.steps > max_steps {
                bail!("step limit of {} exceeded", max_steps);
            }
            match &ops[pc] {
                Op::Add(d) => tape[p] = tape[p].wrapping_add(*d),
                Op::Move(m) => {
                    let np = p as isize + *m;
                    if np < 0 {
                        bail!("head moved left of cell 0 (pc={})", pc);
                    }
                    p = np as usize;
                    ensure!(p);
                }
                Op::Out => {
                    outbuf.push(tape[p]);
                    if outbuf.len() >= 4096 || tape[p] == b'\n' {
                        output.write_all(&outbuf)?;
                        output.flush()?;
                        outbuf.clear();
                    }
                }
                Op::In => {
                    output.write_all(&outbuf)?;
                    output.flush()?;
                    outbuf.clear();
                    let mut b = [0u8; 1];
                    match input.read(&mut b) {
                        Ok(1) => tape[p] = b[0],
                        _ => {} // EOF: leave unchanged
                    }
                }
                Op::LoopStart(end) => {
                    if tape[p] == 0 {
                        pc = *end;
                    }
                }
                Op::LoopEnd(start) => {
                    if tape[p] != 0 {
                        pc = *start;
                    }
                }
                Op::Clear => tape[p] = 0,
                Op::MulAdd(terms) => {
                    let v = tape[p];
                    if v != 0 {
                        for (off, f) in terms {
                            let idx = p as isize + *off;
                            if idx < 0 {
                                bail!("head moved left of cell 0 (pc={})", pc);
                            }
                            let idx = idx as usize;
                            ensure!(idx);
                            tape[idx] = tape[idx].wrapping_add(v.wrapping_mul(*f));
                        }
                        tape[p] = 0;
                    }
                }
                Op::Scan(stride) => {
                    while tape[p] != 0 {
                        let np = p as isize + *stride;
                        if np < 0 {
                            bail!("head moved left of cell 0 (pc={})", pc);
                        }
                        p = np as usize;
                        ensure!(p);
                    }
                }
            }
            pc += 1;
        }
        output.write_all(&outbuf)?;
        output.flush()?;
        Ok(stats)
    }

    /// Number of compiled operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// Recognize `[-]`, multiply-add loops and scan loops. `body` excludes the brackets.
fn optimize_loop(body: &[Op]) -> Option<Op> {
    if body.is_empty() {
        return None;
    }
    // Scan loop: body is a single move.
    if body.len() == 1 {
        if let Op::Move(m) = body[0] {
            return Some(Op::Scan(m));
        }
    }
    // Balanced loop: only Add/Move, net movement 0, loop cell delta == -1.
    let mut off: isize = 0;
    let mut deltas: Vec<(isize, u8)> = Vec::new();
    for op in body {
        match op {
            Op::Add(d) => {
                if let Some(e) = deltas.iter_mut().find(|(o, _)| *o == off) {
                    e.1 = e.1.wrapping_add(*d);
                } else {
                    deltas.push((off, *d));
                }
            }
            Op::Move(m) => off += *m,
            _ => return None,
        }
    }
    if off != 0 {
        return None;
    }
    let loop_delta = deltas.iter().find(|(o, _)| *o == 0).map(|e| e.1).unwrap_or(0);
    if loop_delta != 255 {
        return None;
    }
    let terms: Vec<(isize, u8)> = deltas.into_iter().filter(|(o, f)| *o != 0 && *f != 0).collect();
    if terms.is_empty() {
        Some(Op::Clear)
    } else {
        Some(Op::MulAdd(terms))
    }
}

/// Convenience: parse and run a program on the given input.
pub fn run_str(src: &str, input: &[u8]) -> Result<Vec<u8>> {
    let (out, _) = Program::parse(src)?.run_with_input(input, Some(5_000_000_000))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello() {
        let src = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.+++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";
        assert_eq!(run_str(src, b"").unwrap(), b"Hello World!\n");
    }

    #[test]
    fn echo_until_eof() {
        // Read and echo until EOF (cell unchanged on EOF, so clear first).
        let src = ",[.[-],]";
        assert_eq!(run_str(src, b"abc").unwrap(), b"abc");
    }

    #[test]
    fn muladd_loop() {
        // 3 * 5 via multiply loop, then print as char + 'A'
        let src = "+++[->+++++<]>+++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++.";
        assert_eq!(run_str(src, b"").unwrap(), b"P");
    }

    #[test]
    fn scan_loop() {
        let src = format!("+>+>+>>{}<<<<[>]>.", "+".repeat(65));
        let src = src.as_str();
        assert_eq!(run_str(src, b"").unwrap(), b"A");
    }

    #[test]
    fn unmatched() {
        assert!(Program::parse("[").is_err());
        assert!(Program::parse("]").is_err());
    }
}
