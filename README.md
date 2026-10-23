# llvbf — an LLVM backend for Brainfuck

[![CI](https://github.com/hmeyer/llvbf/actions/workflows/ci.yml/badge.svg)](https://github.com/hmeyer/llvbf/actions/workflows/ci.yml)

`llvbf` compiles LLVM IR to Brainfuck. Anything that can be lowered to LLVM IR
for a 32-bit (or 16-bit) target without floating point can therefore be
compiled to Brainfuck; the bundled driver does this for C with `clang`, and
[`examples/rust`](examples/rust) shows how to do it for Rust on stable.

```
$ cat hello.c
#include <stdio.h>
int main(void) { printf("Hello, Brainfuck world!\n"); return 0; }

$ llvbf cc hello.c -o hello.bf
$ llvbf run hello.bf
Hello, Brainfuck world!
```

The generated programs use plain 8-bit wrapping cells and a tape that grows to
the right, so they run on any standard interpreter. They are large (a few MB)
and slow on naive interpreters; see [Performance](#performance).

## Status

Working:

* integers of 1, 8, 16, 32 and 64 bits (all arithmetic, comparisons, shifts, bitwise ops, casts),
* pointers, `getelementptr`, loads and stores of any width and alignment, globals with initializers,
* structs and arrays (in memory and as SSA aggregates), `alloca` in the entry block,
* all control flow (`br`, `switch`, `phi`, `select`, `unreachable`), direct, indirect and
  recursive calls, variadic functions (`printf` works),
* intrinsics: `memcpy/memmove/memset`, `abs`, `smin/smax/umin/umax`, `bswap`, `fshl/fshr`,
  `ctpop/ctlz/cttz`, `*.with.overflow`, `expect`, `va_start/va_end/va_copy`, lifetime and debug markers,
* a small C runtime: `stdio` (`printf` family, `putchar`, `getchar`, `puts`, `fgets`), `string.h`,
  `stdlib.h` (bump `malloc`, `exit`, `atoi`, `strtol`, `rand`, `qsort`), `ctype.h`, `assert.h`.

Not supported: floating point, vectors, atomics, exceptions, dynamic `alloca`, inline assembly,
external globals. Memory is at most 64 KiB (pointers are 32 bits wide but only the low 16 are
used for addressing). End of input is reported as `0` (or `255` on interpreters that use -1),
which the runtime maps to `EOF`.

## Building

`llvbf` is a Rust crate that reads LLVM modules through `inkwell`/`llvm-sys` and therefore
needs the LLVM 23 development files. llvbf tracks the newest LLVM release so that current
`clang` and current Rust nightlies can feed it; LLVM can read IR from older releases, but not
from newer ones.

```
# Ubuntu/Debian: LLVM 23 from apt.llvm.org (https://apt.llvm.org/)
sudo apt-get install clang-23 llvm-23 llvm-23-dev libpolly-23-dev libzstd-dev
cargo build --release    # finds llvm-config-23; otherwise set LLVM_SYS_231_PREFIX=/usr/lib/llvm-23
cargo test               # unit tests + end-to-end tests of examples/*.c (needs clang-23)
```

The driver looks for `clang`, `llvm-link` and `opt` in `--llvm-bin`, `LLVBF_LLVM_BIN`,
`/usr/lib/llvm-23/bin`, then as `clang-23` and finally as plain `clang`.

## Usage

```
llvbf cc <sources.c...> -o out.bf [--run] [-Os|-O1|...] [--mem-size N] [--comments] [--keep-ir out.ll]
llvbf compile <input.ll|input.bc> -o out.bf [--mem-size N] [--comments] [--entry NAME]
llvbf run <program.bf> [--stats]
```

`llvbf cc` runs `clang -target riscv32-unknown-elf -Os -ffreestanding` on each source and on the
runtime in `runtime/`, links everything with `llvm-link`, runs `opt` (internalize + O2 with
clang's size attributes + dead code elimination) and lowers the result. `--target msp430` selects
the **16-bit mode**: `int` and pointers are 16 bits, which halves the arithmetic and makes the
output about 2.2x smaller, at the price of every program's assumption that `int` holds 100000.
`llvbf compile` takes IR produced any other way; the only requirements are 16- or 32-bit pointers
(only the first 64 KiB of the address space exist) and no floating point. `--comments` annotates
the output with the IR it came from, which is the main debugging aid.

`llvbf run` is an optimizing interpreter: it executes `[-]`, balanced multiply/move loops and
scan loops in constant time. The generated code relies on this heavily (see Performance).

## Compiling C to Brainfuck, step by step

Prerequisites: LLVM 23 with clang (see [Building](#building)) and a built `llvbf`
(`cargo build --release` puts it at `target/release/llvbf`).

1. Write a C program against the subset of libc that `runtime/include` provides
   (`stdio.h`, `stdlib.h`, `string.h`, `ctype.h`, `assert.h`, `inttypes.h`, plus clang's
   freestanding headers such as `stdint.h` and `stdarg.h`). No floating point, no files,
   no `errno`-style OS interfaces; `stdin`/`stdout` are the Brainfuck `,` and `.`.

   ```c
   #include <stdio.h>
   int main(void) {
       int c, n = 0;
       while ((c = getchar()) != EOF) n += (c == '\n');
       printf("%d lines\n", n);
       return 0;
   }
   ```

2. Compile. `llvbf cc` accepts several `.c` files and already-compiled `.ll`/`.bc` files:

   ```
   llvbf cc count.c -o count.bf
   ```

   Useful options: `--target msp430` for the 16-bit mode (smaller, faster output, 16-bit
   `int`), `-O0`…`-O3` (default `-Os`), `--mem-size N` (addressable memory in bytes, default
   16384, maximum 65536), `--comments` (annotate the output with the IR it came from),
   `--keep-ir out.ll` (save the optimized IR that was lowered), `-I`/`-D` (passed to clang).

3. Run it, with `llvbf run` or any 8-bit Brainfuck interpreter:

   ```
   printf 'one\ntwo\n' | llvbf run count.bf
   2 lines
   ```

   Other interpreters: the program needs 8-bit wrapping cells and a tape of roughly
   1.2 × `--mem-size` cells plus about a thousand cells per level of call depth (the hello
   example uses 23 000 cells with the default memory; `llvbf run --stats` prints the exact
   number), so the classic 30 000-cell tape is enough for small programs with the default
   settings but not for `--mem-size 65536`. Ideally the interpreter optimizes balanced loops
   such as `[->>>+<<<]`, which the generated code relies on for speed. End of input must
   leave the cell unchanged or set it to 0 or 255.

What can go wrong: a call to a libc function the runtime does not have fails at link time
("call to undefined function @foo"); floating point, vectors or other unsupported IR fail
in the loader with the offending LLVM instruction printed; programs that need more than
64 KiB of memory or recurse thousands of levels deep run out of memory or tape.

## Compiling Rust to Brainfuck, step by step

Prerequisites: a stable Rust toolchain with the `riscv32i-unknown-none-elf` target, whose
LLVM is not newer than llvbf's (`rustc -vV` shows the LLVM version; llvbf uses LLVM 23
and reads IR from older LLVMs, never from newer ones).

```
rustup target add riscv32i-unknown-none-elf
```

1. Create a `no_std` library crate. The complete example is in [`examples/rust`](examples/rust);
   the essential parts are:

   ```toml
   # Cargo.toml
   [lib]
   crate-type = ["staticlib"]

   [profile.release]
   panic = "abort"      # no unwinding machinery
   opt-level = "s"
   lto = "fat"          # makes the emitted IR self-contained (core gets linked in)
   codegen-units = 1
   ```

   ```rust
   #![no_std]
   #![no_main]

   extern "C" {
       fn __llvbf_putc(c: i32);   // write a byte
       fn __llvbf_getc() -> i32;  // read a byte (0 at end of input)
       fn __llvbf_exit();         // stop the program
   }

   #[no_mangle]
   pub extern "C" fn main() -> i32 {
       for b in "Hello from Rust\n".bytes() {
           unsafe { __llvbf_putc(b as i32) }
       }
       0
   }

   #[panic_handler]
   fn panic(_: &core::panic::PanicInfo) -> ! {
       unsafe { __llvbf_exit() }
       loop {}
   }
   ```

   Everything in `core` that does not need floats or an allocator is available (iterators,
   slices, `Option`/`Result`, formatting with `core::fmt` works but is large). `alloc` is
   possible with a bump allocator on top of the runtime's `malloc`; `std` is not.

2. Emit LLVM IR instead of an object file:

   ```
   cargo rustc --release --target riscv32i-unknown-none-elf -- --emit=llvm-ir
   ```

   The module lands in `target/riscv32i-unknown-none-elf/release/deps/<crate>-<hash>.ll`.
   Because of `lto = "fat"` it contains the parts of `core` the program uses; the only
   external symbols left are the three hooks and the `llvm.memset`/`memcpy` intrinsics.

3. Lower it with `llvbf cc`, which links the C runtime for those intrinsics (and for
   `malloc`, `printf`, … if you declare and call them from Rust):

   ```
   llvbf cc target/riscv32i-unknown-none-elf/release/deps/*.ll -o hello.bf
   llvbf run hello.bf
   ```

Pitfalls: a `Hello` through `core::fmt` pulls in a lot of code (expect a few MB of
Brainfuck); bounds checks and overflow checks stay in and panic through your handler;
`usize` is 32 bits but only 64 KiB of memory exist; the 16-bit mode needs a nightly with
`rust-src` (`-Zbuild-std=core,compiler_builtins --target msp430-none-elf`), see
`examples/rust/README.md`.

### Other frontends

Anything that emits LLVM IR for a 32-bit target without floating point should work the same
way: Zig (`-target riscv32-freestanding -femit-llvm-ir`), C++ without exceptions and RTTI
(`clang++ -fno-exceptions -fno-rtti`, through `llvbf cc`), or hand-written `.ll`. The
contract is the three hooks above plus the C runtime for `memcpy`/`memset`.

## How it works

### Pipeline

```
C ──clang──▶ LLVM IR ──llvm-link/opt──▶ one module ──plan──▶ segments, slots, globals ──codegen──▶ Brainfuck
```

* `ir.rs` is the *snapshot IR*: a plain, owned, lifetime-free Rust model of exactly what the
  lowering consumes (interned types with sizes and offsets, typed operands, constants with
  pre-folded `getelementptr` offsets, instructions as a `match`-able enum).
* `loader/` builds it from an LLVM module through `inkwell`'s safe API. The five things `inkwell`
  does not expose (switch case values, constant expressions, array constant elements, metadata
  arguments, the function type of an indirect call) are wrapped in `loader/raw.rs`, the only
  `unsafe` code in the crate; each wrapper checks its preconditions and documents what it relies
  on. The loader is the only place that changes when LLVM does, and constructs the backend cannot
  lower are rejected here with the LLVM text of the offending value.
* `plan.rs` assigns a 16-bit id to every *segment* (see below), a frame slot to every SSA value and
  an address to every global.
* `emit.rs` is the Brainfuck emitter with position tracking and the constant-time `if_zero` idiom.
* `arith.rs` implements multi-limb integer arithmetic on 8-bit cells.
* `codegen/` emits the program: frame layout and dispatch loop (`mod.rs`), calls (`calls.rs`),
  memory (`memory.rs`), terminators and phis (`control.rs`), instructions (`instr.rs`),
  intrinsics (`intrinsics.rs`).
* `bfi.rs` is the interpreter, `driver.rs` the `cc` pipeline.

### Tape layout

```
 blk(N-1) ... blk(1) blk(0) | guard | frame 0 | frame 1 | frame 2 | ...
 ◀── memory, 256 bytes per block ──▶          ◀── stack of uniform frames, grows right ──▶
```

Each **memory block** is a control area (a copy of the frame header prefix) followed by 256 data
cells; byte address `a` lives in block `a >> 8` at offset `a & 255`. Blocks are stored in
*descending* order so that block 0 is adjacent to the stack. Globals are placed from address 16
up, the heap (`__llvbf_heap_start`) follows them, and a memory stack for `alloca` and variadic
argument spills grows down from the top of memory.

Every **frame** has the same size `F` and the same layout:

| cells | purpose |
|---|---|
| `ACTIVE` | 1 while the frame is live. It is the cell the main loop tests, and the marker the memory routine uses to find the bottom of the stack. |
| `HOME`, `NOTHOME` | mark the frame that issued a memory request |
| `PC_LO/HI`, `CH_LO/HI` | the block id to execute next, and its working copy for the dispatch chain |
| `REQ[k]`, `ADDR_LO/HI`, `HOPS`, `CNT`, `T`, `DATA[0..8]` | the memory request: kind flags, address, and the bytes being transferred (also the "walking block", see below) |
| `MSP` | memory stack pointer (a hidden, per-frame copy) |
| scratch | zero cells used by arithmetic routines; sized by a measuring pass |
| `RET`, params, locals | return value, parameters (laid out by signature), one packed slot per SSA value |

All code is **position independent**: the emitter only tracks offsets relative to the current
frame base. A call therefore shifts the whole coordinate system by `F` and costs no tape movement
at all — the callee's code simply addresses the same offsets one frame further right. This is the
"shifting stack frame" idea from [brainstem](https://github.com/hmeyer/brainstem), made uniform so
that indirect calls and returns need no knowledge of the other side's frame.

### Dispatch loop and segments

Brainfuck has no jumps, so the program is one loop that dispatches on a program counter, as in
[ELVM](https://github.com/shinh/elvm):

```
init globals; enter frame 0; ACTIVE = 1; PC = start
ACTIVE [
    copy PC into CH;  for each id (hi byte, then lo byte): if CH == id { body of segment id }
    for each access kind: REQ[k] [ memory routine k ]
]
```

Each LLVM basic block is split into **segments** at every call and at every memory access, since
both end the current dispatch iteration: a call switches frames, a memory access is serviced by
the shared routine at the end of the iteration. A segment sets `PC` to its successor, so the
continuation after a call or load is just the next segment (`id + 1`). The id tests are the
`if_zero` idiom from ELVM: `CH` is decremented between candidates, so every test is constant time
and non-destructive.

A subtle point: a segment that ends in a call or return shifts the frame *inside* the nested
`if_zero` bodies of the dispatch chain, whose epilogues then run on the new frame's cells. The call
and return sequences therefore prepare those two cells in the destination frame
(`prepare_dispatch_cells`), which is all the epilogues need.

### Calls, returns, varargs

A call copies the arguments into the callee frame's parameter area, copies `MSP`, sets the callee's
`ACTIVE` and `PC` (the callee's entry id — a function pointer *is* its entry segment id), sets its
own `PC` to the continuation, and shifts. `ret` writes the value into its own `RET` area, clears
`ACTIVE` and shifts back; the caller's continuation segment moves `RET` of the frame above into the
destination slot. Recursion needs nothing special. A synthetic start frame calls the entry function
and clears its `ACTIVE` when it returns, which ends the main loop.

Variadic arguments are spilled by the caller into the memory stack in pointer-sized slots (aligned
to at most two pointers, which is what clang's inline `va_arg` expansion assumes on 32-bit
targets) and a hidden pointer parameter is passed. `va_start` stores it; on the 16-bit target,
where clang emits the `va_arg` instruction, llvbf lowers it to three ordinary memory accesses.

### Memory accesses

A load or store sets `ADDR`, (`DATA`), and a `REQ` flag; the routine for that (kind, width) then:

1. hops down through the active frames (`ACTIVE [ move request cells one frame down ]`) until the
   guard frame, carrying the request in the fixed header cells — all moves are balanced loops;
2. hops `ADDR_HI` blocks further, counting hops in `HOPS`;
3. walks the control block `{ADDR_LO, CNT, T, DATA}` right through the data cells, one cell per
   unit of `ADDR_LO`, rotating each displaced data cell behind it (brainstem's technique);
4. copies the addressed bytes into / out of `DATA`, then walks and hops back, and finally climbs
   the frames until it finds the one with `HOME` set.

Accesses are split into aligned chunks of 1, 2, 4 or 8 bytes so that one chunk never crosses a
block boundary. Each distinct (kind, width) gets one shared routine; the routine is emitted once,
not per access.

### Arithmetic

Values are little-endian sequences of 8-bit limbs. Arithmetic runs in *spaced* scratch areas (limb
`i` at `3i`) so that every limb has two zero neighbours, which the `if_zero` idiom needs for carry
and borrow detection in constant time per step. Addition and subtraction are O(256) per limb;
multiplication is shift-and-add; division is restoring long division; signed operations map onto
unsigned ones by flipping sign bits; bitwise operations decompose limbs into bits. Everything is
unit-tested against native arithmetic (`cargo test`).

## Performance

Numbers for the examples, compiled with `-Os` and run with `llvbf run`, for the default 32-bit
target and the 16-bit mode:

| program | riscv32 size / ops | msp430 size / ops |
|---|---|---|
| fib(0..15), recursive, printf | 10.3 MB / 103 M | 4.6 MB / 66 M |
| bubble sort | 10.3 MB / 9.5 M | 4.6 MB / 7.3 M |
| strings | 11.4 MB / 17.5 M | 5.0 MB / 14.8 M |
| sieve to 200 | 10.3 MB / 56 M | 4.6 MB / 43 M |

Almost all of the size is head movement (`>>>…`) between frame slots and scratch, because every
SSA value has its own slot and frames are several hundred cells wide. Almost all of the time is
spent in the carry loops of multi-limb arithmetic and in the dispatch chain; the 32-bit target
doubles the limb count but only costs 1.2–1.6x in executed operations.

The code assumes an interpreter that runs balanced loops such as `[->>>+<<<]` in constant time.
On a naive interpreter every copy costs time proportional to the value times the distance, and
programs become hundreds of times slower; they remain correct.

## Testing

`cargo test` runs four layers, and CI (`.github/workflows/ci.yml`) runs them with `cargo fmt
--check` and `cargo clippy -D warnings` on every push and pull request:

* unit tests of the interpreter, the emitter idioms and the multi-limb arithmetic (the
  arithmetic is checked against native integer arithmetic on fixed and random operands);
* end-to-end tests compiling `examples/*.c` and comparing with `examples/expected/*.txt`;
* the Rust example (skipped when no suitable nightly is installed);
* a differential fuzz test (`tests/fuzz_arith.rs`): random C programs over 32-bit arithmetic,
  loops and arrays are compiled with the host C compiler and with llvbf, and the outputs must
  match. `LLVBF_FUZZ_COUNT` and `LLVBF_FUZZ_SEED` select the batch; a failure names the seed and
  keeps the program.

## Design notes and related work

* [brainstem](https://github.com/hmeyer/brainstem): static, frame-relative addressing and shifting
  stack frames; `llvbf` keeps this for registers but makes frames uniform.
* [braingraft](https://github.com/hmeyer/braingraft): the previous attempt at LLVM IR → brainstem;
  `llvbf` targets Brainfuck directly because LLVM's control flow (arbitrary CFGs, recursion,
  indirect calls) does not map onto structured `if`/`while` without a dispatch loop anyway.
* [brainfuckC](https://github.com/hmeyer/brainfuckC): recursion by re-executing function bodies
  under a return-position counter; the dispatch loop replaces this with ordinary frames.
* [asm2bf](https://github.com/kspalaiologos/asmbf): a register machine with memory walks; it
  needs 16-bit cells, whereas `llvbf` sticks to 8-bit cells and pays with multi-limb arithmetic.
* [ELVM](https://github.com/shinh/elvm): the pc-dispatch loop, the block-structured memory and the
  one-shared-memory-routine idea come from its Brainfuck backend. ELVM keeps all registers at fixed
  tape positions and every local in memory; `llvbf` keeps locals in frames, so most instructions
  never touch memory.

Why not an in-tree LLVM target? LLVM's code generator (SelectionDAG, register allocation, machine
instructions) models a register machine with jumps. Brainfuck has neither, so a real `Target` would
emit an assembly language that still needs everything in this repository to become Brainfuck.
Consuming IR directly is simpler and is what every working esolang backend does.

Why `riscv32` as the default C target? Its `int` is 32 bits, so existing C and Rust code behaves
as written (the 16-bit `int` of msp430 is a reliable source of "compiler bugs" that are not), its
data layout is plain, it is a first-class backend in LLVM, clang, Rust and Zig, and Rust ships a
precompiled `core` for it, so Rust programs compile on stable. msp430 stays available as the
16-bit mode for anyone who wants the smaller and faster output.

## Future work

* Slot pooling by liveness and moving (rather than copying) values on their last use would cut
  the output size by an order of magnitude; the first version deliberately keeps one slot per value.
* Structured control flow (relooper) for loops inside a function would avoid the dispatch chain
  for the hottest edges.
* Exploit the known-zero upper limbs of 32-bit pointers (the address space is 64 KiB).
* Other frontends: Zig and C++ (without exceptions and RTTI) should work like Rust does; the
  runtime hooks are just `__llvbf_putc`, `__llvbf_getc` and `__llvbf_exit`.

## License

MIT.
