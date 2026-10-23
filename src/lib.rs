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
