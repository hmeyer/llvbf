# llvbf — an LLVM backend for Brainfuck

`llvbf` compiles LLVM IR to Brainfuck. Anything that can be lowered to LLVM IR
with a 16-bit (or 32-bit) pointer target can therefore be compiled to
Brainfuck; the bundled driver does this for C with `clang`, and
[`examples/rust`](examples/rust) shows how to do it for Rust.

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
external globals. Memory is at most 64 KiB. End of input is reported as `0` (or `255` on
interpreters that use -1), which the runtime maps to `EOF`.

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

`llvbf cc` runs `clang -target msp430 -Os -ffreestanding` on each source and on the runtime in
`runtime/`, links everything with `llvm-link`, runs `opt` (internalize + `-Os` + dead code
elimination) and lowers the result. `llvbf compile` takes IR produced any other way; the only
requirements are a data layout with 16-bit pointers (32-bit works, but only the first 64 KiB of
the address space exist) and no floating point. `--comments` annotates the output with the IR it
came from, which is the main debugging aid.

`llvbf run` is an optimizing interpreter: it executes `[-]`, balanced multiply/move loops and
scan loops in constant time. The generated code relies on this heavily (see Performance).

## How it works

### Pipeline

```
C ──clang──▶ LLVM IR ──llvm-link/opt──▶ one module ──plan──▶ segments, slots, globals ──codegen──▶ Brainfuck
```

* `ir.rs` is the *snapshot IR*: a plain, owned, lifetime-free Rust model of exactly what the
  lowering consumes (interned types with sizes and offsets, typed operands, constants with
  pre-folded `getelementptr` offsets, instructions as a `match`-able enum).
* `loader.rs` builds it from an LLVM module with `inkwell`, using the LLVM C API for the details
  `inkwell` does not expose. It is the only module with `unsafe` code and the only place that
  changes when LLVM does; constructs the backend cannot lower are rejected here with the LLVM
  text of the offending value.
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

Variadic arguments are spilled by the caller into the memory stack (2-byte slots, as the msp430 ABI
expects) and a hidden pointer parameter is passed; `va_start` stores it and `va_arg` is three
ordinary memory accesses.

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

## Rust (and other frontends)

Any LLVM frontend works if it can target 16-bit pointers and emit a module with no floats.
For Rust, `examples/rust` is a `no_std` crate built with a nightly, `-Zbuild-std=core` for
`msp430-none-elf`, `panic = "abort"` and `lto = "fat"`; the LTO'd `--emit=llvm-ir` output is
self-contained and `llvbf cc` links the C runtime for `memset`. The nightly's LLVM must not be
newer than llvbf's (`rustc +nightly -vV` shows it). `cargo test` runs this example when such a
nightly is installed and skips it otherwise.

## Performance

Numbers for the examples, compiled with `-Os` and run with `llvbf run`:

| program | Brainfuck size | ops executed |
|---|---|---|
| hello (printf) | 4.5 MB | 1.3 M |
| fib(0..15), recursive | 4.6 MB | 66 M |
| sieve to 200 | 4.6 MB | 45 M |

Almost all of the size is head movement (`>>>…`) between frame slots and scratch, because every
SSA value has its own slot and frames are several hundred cells wide. Almost all of the time is
spent in the carry loops of multi-limb arithmetic and in the dispatch chain.

The code assumes an interpreter that runs balanced loops such as `[->>>+<<<]` in constant time.
On a naive interpreter every copy costs time proportional to the value times the distance, and
programs become hundreds of times slower; they remain correct.

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

Why `msp430` as the C target? It is the only in-tree clang target with 16-bit `int` and 16-bit
pointers and no vector units, which keeps arithmetic at two limbs. Any target works as long as the
IR has no floats; `-target i386` gives 32-bit `int` and roughly four times slower arithmetic.

## Future work

* Slot pooling by liveness and moving (rather than copying) values on their last use would cut
  the output size by an order of magnitude; the first version deliberately keeps one slot per value.
* Structured control flow (relooper) for loops inside a function would avoid the dispatch chain
  for the hottest edges.
* 16-bit cell mode (one limb per `int`).
* Other frontends: Zig and C++ (without exceptions and RTTI) should work like Rust does; the
  runtime hooks are just `__llvbf_putc`, `__llvbf_getc` and `__llvbf_exit`.

## License

MIT.
