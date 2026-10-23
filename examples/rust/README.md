# Rust to Brainfuck

This crate is a `no_std` program that only talks to the outside world through
the three llvbf hooks (`__llvbf_putc`, `__llvbf_getc`, `__llvbf_exit`).

Requirements: stable Rust with the `riscv32i-unknown-none-elf` target (a Tier 2
target with a precompiled `core`), and a rustc whose LLVM is not newer than the
one llvbf is built against (`rustc -vV` shows it; llvbf uses LLVM 23).

```
rustup target add riscv32i-unknown-none-elf
cargo rustc --release --target riscv32i-unknown-none-elf -- --emit=llvm-ir
llvbf cc target/riscv32i-unknown-none-elf/release/deps/bf_rust_example-*.ll -o rust.bf
llvbf run rust.bf
```

Why it works: with `lto = "fat"`, `--emit=llvm-ir` writes one self-contained
module after link-time optimization, so the parts of `core` the program uses
(for example the bounds-check panic path) are included and the only external
symbols left are the llvbf hooks and the `llvm.memset` intrinsic, which
`llvbf cc` resolves by linking its C runtime. `-C panic=abort` keeps unwinding
out of the picture; the `#[panic_handler]` prints `panic` and exits.

The 16-bit mode also works, but needs a nightly with `rust-src`:
`cargo +nightly rustc --release --target msp430-none-elf -Zbuild-std=core,compiler_builtins -- --emit=llvm-ir`,
then `llvbf cc --target msp430 ...`. Arithmetic is two limbs wide instead of four.
