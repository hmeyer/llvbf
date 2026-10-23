//! End-to-end test of the Rust example in `examples/rust`.
//!
//! It needs the `riscv32i-unknown-none-elf` target of the stable toolchain
//! (`rustup target add riscv32i-unknown-none-elf`) and a rustc whose LLVM is not
//! newer than the LLVM llvbf is built against; otherwise the test is skipped with
//! a message.

use std::path::PathBuf;
use std::process::Command;

const TARGET: &str = "riscv32i-unknown-none-elf";

#[test]
fn rust_example() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = root.join("examples/rust");
    let target_dir =
        std::env::temp_dir().join(format!("llvbf-rust-example-{}", std::process::id()));
    let build = Command::new("cargo")
        .args(["rustc", "--release", "--target", TARGET])
        .arg("--target-dir")
        .arg(&target_dir)
        .args(["--", "--emit=llvm-ir"])
        .current_dir(&dir)
        .output();
    match build {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "skipping: Rust build failed (is the {} target installed?):\n{}",
                TARGET,
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("skipping: cannot run cargo: {}", e);
            return;
        }
    }
    // Find the emitted IR.
    let ll = walk(&target_dir.join(TARGET).join("release"))
        .into_iter()
        .find(|p| {
            p.extension().map(|e| e == "ll").unwrap_or(false)
                && p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("bf_rust_example")
        })
        .unwrap_or_else(|| {
            let listing: Vec<String> = walk(&target_dir)
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            panic!(
                "no .ll produced by the Rust build; files under {}:\n{}",
                target_dir.display(),
                listing.join("\n")
            )
        });
    let opts = llvbf::driver::CcOptions {
        sources: vec![ll],
        ..Default::default()
    };
    let code = match llvbf::driver::cc(&opts) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{:#}", e);
            if msg.contains("cannot parse IR") {
                eprintln!("skipping: rustc's LLVM is newer than llvbf's: {}", msg);
                return;
            }
            panic!("{}", msg);
        }
    };
    let prog = llvbf::bfi::Program::parse(std::str::from_utf8(&code).unwrap()).unwrap();
    let (out, _) = prog.run_with_input(b"", Some(2_000_000_000)).unwrap();
    let expected = std::fs::read_to_string(root.join("examples/expected/rust.txt")).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), expected);
    let _ = std::fs::remove_dir_all(&target_dir);
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                v.extend(walk(&p));
            } else {
                v.push(p);
            }
        }
    }
    v
}
