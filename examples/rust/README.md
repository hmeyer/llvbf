# Rust to Brainfuck

This crate is a `no_std` program that only talks to the outside world through
the three llvbf hooks (`__llvbf_putc`, `__llvbf_getc`, `__llvbf_exit`).

Requirements: a Rust nightly whose LLVM major version matches llvbf's (LLVM 23
at the time of writing; check `rustc +nightly -vV`), with the `rust-src`
component so that `core` can be built for the 16-bit `msp430-none-elf` target.

```
rustup toolchain install nightly -c rust-src
cargo +nightly rustc --release --target msp430-none-elf \
      -Zbuild-std=core,compiler_builtins -- --emit=llvm-ir
llvbf cc target/msp430-none-elf/release/deps/bf_rust_example-*.ll -o rust.bf
llvbf run rust.bf
```

Why it works: with `lto = "fat"`, `--emit=llvm-ir` writes one self-contained
module after link-time optimization, so the parts of `core` the program uses
(for example the bounds-check panic path) are included and the only external
symbols left are the llvbf hooks and the `llvm.memset` intrinsic, which
`llvbf cc` resolves by linking its C runtime. `-C panic=abort` keeps unwinding
out of the picture; the `#[panic_handler]` prints `panic` and exits.

A 32-bit build (`--target i686-unknown-linux-gnu`, no `build-std` needed) also
works: link the runtime with `llvbf cc --target i686-unknown-none ...`.
Arithmetic is then four limbs wide instead of two.
