//! llvbf: an LLVM backend targeting Brainfuck.
//!
//! See `README.md` for the architecture.

pub mod arith;
pub mod assist;
pub mod bfi;
pub mod codegen;
pub mod driver;
pub mod emit;
pub mod plan;
pub mod types;

use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Compile an LLVM IR (`.ll`) or bitcode (`.bc`) file to Brainfuck.
pub fn compile_file(path: &Path, opts: &codegen::Options) -> Result<Vec<u8>> {
    let geps = assist::load_gep_table(path)?;
    let is_bc = path.extension().map(|e| e == "bc").unwrap_or(false);
    let module = if is_bc {
        llvm_ir::Module::from_bc_path(path)
    } else {
        llvm_ir::Module::from_ir_path(path)
    }
    .map_err(|e| anyhow!(e))
    .with_context(|| format!("parsing {}", path.display()))?;
    codegen::compile(&module, &geps, opts)
}
