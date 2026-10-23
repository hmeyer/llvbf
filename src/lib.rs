//! llvbf: an LLVM backend targeting Brainfuck.
//!
//! See `README.md` for the architecture.

pub mod arith;
pub mod bfi;
pub mod codegen;
pub mod driver;
pub mod emit;
pub mod ir;
pub mod loader;
pub mod plan;

use anyhow::Result;
use std::path::Path;

/// Compile an LLVM IR (`.ll`) or bitcode (`.bc`) file to Brainfuck.
pub fn compile_file(path: &Path, opts: &codegen::Options) -> Result<Vec<u8>> {
    let module = loader::load(path)?;
    codegen::compile(&module, opts)
}

/// Print how the bytes of a Brainfuck program compiled with `--comments` are
/// distributed over the constructs that produced them (by IR opcode or routine),
/// and the largest individual regions.
pub fn size_report(src: &str) {
    use std::collections::HashMap;
    let is_bf = |b: u8| b"+-<>[].,".contains(&b);
    let mut by_kind: HashMap<String, (usize, usize)> = HashMap::new(); // bytes, count
    let mut regions: Vec<(usize, String)> = Vec::new();
    let mut total = 0usize;
    let mut current = String::from("(start)");
    let mut current_bytes = 0usize;
    let flush = |current: &str,
                 bytes: usize,
                 by_kind: &mut HashMap<String, (usize, usize)>,
                 regions: &mut Vec<(usize, String)>| {
        let kind = size_kind(current);
        let e = by_kind.entry(kind).or_insert((0, 0));
        e.0 += bytes;
        e.1 += 1;
        regions.push((bytes, current.to_string()));
    };
    for line in src.lines() {
        if let Some(c) = line.strip_prefix("# ") {
            flush(&current, current_bytes, &mut by_kind, &mut regions);
            current = c.to_string();
            current_bytes = 0;
        } else {
            let n = line.bytes().filter(|&b| is_bf(b)).count();
            current_bytes += n;
            total += n;
        }
    }
    flush(&current, current_bytes, &mut by_kind, &mut regions);
    let mut kinds: Vec<(String, (usize, usize))> = by_kind.into_iter().collect();
    kinds.sort_by_key(|k| std::cmp::Reverse(k.1 .0));
    println!(
        "{} Brainfuck bytes\n\n{:>10} {:>6} {:>6} {:>9}  kind",
        total, "bytes", "%", "count", "avg"
    );
    for (k, (b, c)) in kinds.iter().take(40) {
        println!(
            "{:>10} {:>5.1}% {:>6} {:>9}  {}",
            b,
            100.0 * *b as f64 / total.max(1) as f64,
            c,
            b / c.max(&1),
            k
        );
    }
    regions.sort_by_key(|r| std::cmp::Reverse(r.0));
    println!("\nlargest regions:");
    for (b, name) in regions.iter().take(15) {
        println!("{:>10}  {}", b, name);
    }
}

/// The construct a comment marker belongs to: the opcode of an IR instruction, the
/// terminator kind, or the routine name.
fn size_kind(marker: &str) -> String {
    let m = marker.trim();
    if let Some(rest) = m.split_once(" = ").map(|x| x.1) {
        // "%5 = add nsw i32 ..." -> "add"; "%7 = tail call ..." -> "call"
        let mut words = rest.split_whitespace();
        let mut w = words.next().unwrap_or("?");
        if w == "tail" || w == "musttail" || w == "notail" {
            w = words.next().unwrap_or("?");
        }
        return format!("instr {}", w);
    }
    let first = m.split_whitespace().next().unwrap_or("?");
    match first {
        "memory" | "call:" | "call" | "dispatch:" | "segment" | "_start:" | "_exit"
        | "initialize" | "start" | "variadic" | "llvbf" => m
            .split(':')
            .next()
            .unwrap_or(m)
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" "),
        "store" | "br" | "ret" | "switch" | "unreachable" | "tail" => {
            format!("term/void {}", if first == "tail" { "call" } else { first })
        }
        _ => first.to_string(),
    }
}
