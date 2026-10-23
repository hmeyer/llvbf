//! End-to-end tests: compile each program in `examples/` to Brainfuck with the
//! `llvbf cc` pipeline, run it in the bundled interpreter and compare the output
//! with `examples/expected/<name>.txt`.
//!
//! These tests need `clang`, `llvm-link` and `opt` (LLVM 18) on the PATH or in
//! `LLVBF_LLVM_BIN`.

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_example(name: &str, input: &[u8]) -> String {
    let mut opts = llvbf::driver::CcOptions::default();
    opts.sources = vec![root().join("examples").join(format!("{}.c", name))];
    let code = llvbf::driver::cc(&opts).unwrap_or_else(|e| panic!("compiling {}: {:#}", name, e));
    let src = String::from_utf8(code).unwrap();
    let prog = llvbf::bfi::Program::parse(&src).unwrap();
    let (out, _) = prog
        .run_with_input(input, Some(5_000_000_000))
        .unwrap_or_else(|e| panic!("running {}: {:#}", name, e));
    String::from_utf8(out).unwrap()
}

fn check(name: &str, input: &[u8]) {
    let expected = std::fs::read_to_string(
        root()
            .join("examples/expected")
            .join(format!("{}.txt", name)),
    )
    .unwrap_or_else(|_| panic!("missing expected output for {}", name));
    assert_eq!(run_example(name, input), expected, "output of {}", name);
}

macro_rules! example_tests {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                check(stringify!($name), b"");
            }
        )*
    };
}

example_tests!(hello, fib, sort, strings, list, calc, long_math, sieve, builtins);

#[test]
fn cat() {
    let input = std::fs::read(root().join("examples/cat.input")).unwrap();
    check("cat", &input);
}
