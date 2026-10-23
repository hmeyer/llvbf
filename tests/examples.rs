//! End-to-end tests: compile each program in `examples/` to Brainfuck with the
//! `llvbf cc` pipeline, run it in the bundled interpreter and compare the output
//! with `examples/expected/<name>.txt`.
//!
//! These tests need `clang`, `llvm-link` and `opt` (LLVM 23) on the PATH, in
//! `/usr/lib/llvm-23/bin` or in `LLVBF_LLVM_BIN`.

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_example(name: &str, input: &[u8], target: Option<&str>) -> String {
    let mut opts = llvbf::driver::CcOptions {
        sources: vec![root().join("examples").join(format!("{}.c", name))],
        ..Default::default()
    };
    if let Some(t) = target {
        opts.target = t.to_string();
    }
    let code = llvbf::driver::cc(&opts).unwrap_or_else(|e| panic!("compiling {}: {:#}", name, e));
    let src = String::from_utf8(code).unwrap();
    let prog = llvbf::bfi::Program::parse(&src).unwrap();
    let (out, _) = prog
        .run_with_input(input, Some(5_000_000_000))
        .unwrap_or_else(|e| panic!("running {}: {:#}", name, e));
    String::from_utf8(out).unwrap()
}

fn check(name: &str, input: &[u8], target: Option<&str>) {
    let expected = std::fs::read_to_string(
        root()
            .join("examples/expected")
            .join(format!("{}.txt", name)),
    )
    .unwrap_or_else(|_| panic!("missing expected output for {}", name));
    assert_eq!(
        run_example(name, input, target),
        expected,
        "output of {} for {:?}",
        name,
        target
    );
}

macro_rules! example_tests {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                check(stringify!($name), b"", None);
            }
        )*
    };
}

example_tests!(hello, fib, sort, strings, list, calc, long_math, sieve, builtins);

#[test]
fn cat() {
    let input = std::fs::read(root().join("examples/cat.input")).unwrap();
    check("cat", &input, None);
}

/// The 16-bit mode (`--target msp430`) on the examples whose output does not depend
/// on the width of `int`.
mod msp430 {
    use super::*;

    macro_rules! msp430_tests {
        ($($name:ident),* $(,)?) => {
            $(
                #[test]
                fn $name() {
                    check(stringify!($name), b"", Some("msp430"));
                }
            )*
        };
    }

    msp430_tests!(hello, fib, sort, strings, list, calc, sieve);

    #[test]
    fn cat() {
        let input = std::fs::read(root().join("examples/cat.input")).unwrap();
        check("cat", &input, Some("msp430"));
    }
}
