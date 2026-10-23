//! End-to-end test of the Rust example in `examples/rust`.
//!
//! It needs a Rust nightly with the `rust-src` component whose LLVM matches the
//! LLVM llvbf is built against; when `cargo +nightly` is unavailable or the build
//! fails for toolchain reasons, the test is skipped with a message.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn rust_example() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = root.join("examples/rust");
    let target_dir =
        std::env::temp_dir().join(format!("llvbf-rust-example-{}", std::process::id()));
    let status = Command::new("cargo")
        .args([
            "+nightly",
            "rustc",
            "--release",
            "--target",
            "msp430-none-elf",
            "-Zbuild-std=core,compiler_builtins",
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .args(["--", "--emit=llvm-ir"])
        .current_dir(&dir)
        .output();
    let out = match status {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "skipping: nightly build failed:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("skipping: cannot run cargo +nightly: {}", e);
            return;
        }
    };
    let _ = out;
    // Find the emitted IR.
    let mut ll = None;
    for entry in walk(&target_dir.join("msp430-none-elf/release")) {
        if entry.extension().map(|e| e == "ll").unwrap_or(false)
            && entry
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("bf_rust_example")
        {
            ll = Some(entry);
        }
    }
    let ll = ll.expect("no .ll produced by the nightly build");
    let opts = llvbf::driver::CcOptions {
        sources: vec![ll],
        ..Default::default()
    };
    let code = match llvbf::driver::cc(&opts) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{:#}", e);
            if msg.contains("cannot parse IR") {
                eprintln!(
                    "skipping: the nightly's LLVM is newer than llvbf's: {}",
                    msg
                );
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
