//! Differential fuzz test: random straight-line C programs over `uint32_t`,
//! `int32_t` and `uint8_t` arithmetic, loops and small arrays are compiled with
//! the host C compiler and with llvbf; both outputs must match byte for byte.
//!
//! The programs stick to constructs whose C semantics do not depend on the
//! width of `int` (which is 16 bits on llvbf's target and 32 bits on the host):
//! all arithmetic is done in `uint32_t`/`int32_t`, every literal is pinned with
//! `UINT32_C`/`INT32_C` (a bare `1234u` would be a 16-bit `unsigned int` on the
//! target), shifts are masked, divisors are made nonzero, signed operands are kept
//! small, and values are printed with the `PRI*32` macros. The seed is fixed, so
//! failures are reproducible.
//!
//! Needs `cc` on the host and the LLVM 23 tools llvbf itself needs.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

/// xorshift64*: small, deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

struct Gen {
    rng: Rng,
    src: String,
    /// Names of live `uint32_t` variables.
    vars: Vec<String>,
    next_var: usize,
    depth: usize,
}

impl Gen {
    fn fresh(&mut self) -> String {
        let n = format!("v{}", self.next_var);
        self.next_var += 1;
        n
    }

    /// A `uint32_t` expression of bounded depth.
    fn expr(&mut self) -> String {
        self.depth += 1;
        let r = if self.depth > 3 || self.rng.below(3) == 0 {
            self.leaf()
        } else {
            match self.rng.below(12) {
                0 => format!("({} + {})", self.expr(), self.expr()),
                1 => format!("({} - {})", self.expr(), self.expr()),
                2 => format!("({} * {})", self.expr(), self.expr()),
                3 => format!("({} / ({} | UINT32_C(1)))", self.expr(), self.expr()),
                4 => format!("({} % ({} | UINT32_C(1)))", self.expr(), self.expr()),
                5 => format!("({} & {})", self.expr(), self.expr()),
                6 => format!("({} | {})", self.expr(), self.expr()),
                7 => format!("({} ^ {})", self.expr(), self.expr()),
                8 => format!("({} << ({} & UINT32_C(31)))", self.expr(), self.expr()),
                9 => format!("({} >> ({} & UINT32_C(31)))", self.expr(), self.expr()),
                10 => format!(
                    "(uint32_t)({} {} {})",
                    self.expr(),
                    self.rng.pick(&["<", "<=", "==", "!=", ">", ">="]),
                    self.expr()
                ),
                _ => {
                    // Signed comparison / division on small signed values.
                    let a = self.expr();
                    let b = self.expr();
                    format!("(uint32_t)(((int32_t)({} & UINT32_C(0xffff)) - INT32_C(32768)) {} ((int32_t)({} & UINT32_C(0xffff)) - INT32_C(32768) | 1))", a, self.rng.pick(&["<", "/", "%", ">="]), b)
                }
            }
        };
        self.depth -= 1;
        r
    }

    fn leaf(&mut self) -> String {
        match self.rng.below(4) {
            0 if !self.vars.is_empty() => self.rng.pick(&self.vars.clone()).clone(),
            1 => format!("UINT32_C({})", self.rng.below(1 << 16)),
            2 => format!("UINT32_C({})", self.rng.next() as u32),
            _ => format!(
                "arr[{} & UINT32_C(7)]",
                if self.vars.is_empty() {
                    "UINT32_C(3)".to_string()
                } else {
                    self.rng.pick(&self.vars.clone()).clone()
                }
            ),
        }
    }

    fn stmt(&mut self) {
        match self.rng.below(6) {
            0 | 1 => {
                let v = self.fresh();
                let e = self.expr();
                let _ = writeln!(self.src, "    uint32_t {} = {};", v, e);
                self.vars.push(v);
            }
            2 => {
                let e = self.expr();
                let idx = self.expr();
                let _ = writeln!(self.src, "    arr[{} & UINT32_C(7)] = {};", idx, e);
            }
            3 if !self.vars.is_empty() => {
                let v = self.rng.pick(&self.vars.clone()).clone();
                let e = self.expr();
                let _ = writeln!(self.src, "    {} = {};", v, e);
            }
            4 if !self.vars.is_empty() => {
                // A loop with a bounded trip count and a conditional update.
                let v = self.rng.pick(&self.vars.clone()).clone();
                let n = self.rng.below(6) + 1;
                let e = self.expr();
                let c = self.expr();
                let _ = writeln!(self.src, "    for (uint32_t i = 0; i < UINT32_C({}); i++) {{ if (({} & UINT32_C(1)) == 0) {} = {} + i; else arr[i & UINT32_C(7)] ^= {}; }}", n, c, v, e, v);
            }
            _ => {
                let e = self.expr();
                let _ = writeln!(self.src, "    printf(\"%\" PRIu32 \" %\" PRId32 \" %02\" PRIx32 \"\\n\", {}, (int32_t)({}), {} & UINT32_C(0xff));", e, e, e);
            }
        }
    }
}

fn generate(seed: u64) -> String {
    let mut g = Gen {
        rng: Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1),
        src: String::new(),
        vars: Vec::new(),
        next_var: 0,
        depth: 0,
    };
    g.src
        .push_str("#include <stdio.h>\n#include <stdint.h>\n#include <inttypes.h>\n");
    g.src.push_str(
        "static uint32_t helper(uint32_t a, uint32_t b) { return (a * UINT32_C(3)) ^ (b >> 1); }\n",
    );
    g.src
        .push_str("uint32_t arr[8] = {UINT32_C(1), UINT32_C(2), UINT32_C(3), UINT32_C(5), UINT32_C(8), UINT32_C(13), UINT32_C(21), UINT32_C(34)};\n");
    g.src.push_str("int main(void) {\n");
    let v = g.fresh();
    let _ = writeln!(g.src, "    uint32_t {} = helper(arr[1], arr[6]);", v);
    g.vars.push(v);
    for _ in 0..12 {
        g.stmt();
    }
    g.src
        .push_str("    for (int k = 0; k < 8; k++) printf(\"%\" PRIu32 \" \", arr[k]);\n");
    // Make sure every variable is observable.
    for v in g.vars.clone() {
        let _ = writeln!(
            g.src,
            "    printf(\"%\" PRIu32 \"\\n\", helper({}, UINT32_C(7)));",
            v
        );
    }
    g.src.push_str("    return 0;\n}\n");
    g.src
}

fn run_native(dir: &std::path::Path, src: &std::path::Path) -> Option<String> {
    let exe = dir.join("native");
    let cc = Command::new("cc")
        .args(["-O0", "-w"])
        .arg(src)
        .arg("-o")
        .arg(&exe)
        .output()
        .ok()?;
    if !cc.status.success() {
        panic!("host cc failed:\n{}", String::from_utf8_lossy(&cc.stderr));
    }
    let out = Command::new(&exe).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_llvbf(src: &std::path::Path) -> String {
    let opts = llvbf::driver::CcOptions {
        sources: vec![src.to_path_buf()],
        ..Default::default()
    };
    let code = llvbf::driver::cc(&opts)
        .unwrap_or_else(|e| panic!("llvbf cc failed on {}: {:#}", src.display(), e));
    let prog = llvbf::bfi::Program::parse(std::str::from_utf8(&code).unwrap()).unwrap();
    let (out, _) = prog
        .run_with_input(b"", Some(20_000_000_000))
        .unwrap_or_else(|e| panic!("running {}: {:#}", src.display(), e));
    String::from_utf8(out).unwrap()
}

#[test]
fn differential_random_programs() {
    if Command::new("cc").arg("--version").output().is_err() {
        eprintln!("skipping: no host C compiler");
        return;
    }
    let count: u64 = std::env::var("LLVBF_FUZZ_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let base_seed: u64 = std::env::var("LLVBF_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let dir = std::env::temp_dir().join(format!("llvbf-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for seed in base_seed..base_seed + count {
        let src_text = generate(seed);
        let src = dir.join(format!("p{}.c", seed));
        std::fs::write(&src, &src_text).unwrap();
        let Some(expected) = run_native(&dir, &src) else {
            eprintln!("skipping: cannot run the native build");
            return;
        };
        let actual = run_llvbf(&src);
        assert_eq!(
            actual, expected,
            "seed {} differs (program kept at {}; rerun with LLVBF_FUZZ_SEED={} LLVBF_FUZZ_COUNT=1)",
            seed, src.display(), seed
        );
    }
    let _ = std::fs::remove_dir_all(PathBuf::from(&dir));
}
