//! Helpers that need the raw LLVM C API.
//!
//! The `llvm-ir` crate does not record the source element type of *constant*
//! `getelementptr` expressions, so their byte offsets cannot be computed from it
//! alone. This module re-parses the input with `llvm-sys`, walks every constant
//! expression and records, for each constant GEP, its byte offset relative to its
//! base global, computed with LLVM's own `TargetData`.

use anyhow::{anyhow, bail, Result};
use llvm_sys::bit_reader::LLVMParseBitcodeInContext2;
use llvm_sys::core::*;
use llvm_sys::ir_reader::LLVMParseIRInContext;
use llvm_sys::prelude::*;
use llvm_sys::target::*;
use llvm_sys::{LLVMOpcode, LLVMTypeKind};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr::null_mut;

/// Key: (base global name, GEP indices). Value: byte offset.
#[derive(Default, Debug)]
pub struct GepTable {
    map: HashMap<(String, Vec<i64>), i64>,
}

impl GepTable {
    pub fn lookup(&self, base: &str, indices: &[i64]) -> Option<i64> {
        self.map.get(&(base.to_string(), indices.to_vec())).copied()
    }
}

struct Walker {
    td: LLVMTargetDataRef,
    seen: HashSet<usize>,
    table: GepTable,
    conflicts: Vec<String>,
}

impl Walker {
    unsafe fn visit_value(&mut self, v: LLVMValueRef) {
        if v.is_null() || !self.seen.insert(v as usize) {
            return;
        }
        if LLVMIsAConstant(v).is_null() {
            return;
        }
        // Recurse into operands of constant aggregates and expressions.
        if !LLVMIsAConstantExpr(v).is_null()
            || !LLVMIsAConstantStruct(v).is_null()
            || !LLVMIsAConstantArray(v).is_null()
            || !LLVMIsAConstantVector(v).is_null()
        {
            let n = LLVMGetNumOperands(v);
            for i in 0..n {
                self.visit_value(LLVMGetOperand(v, i as u32));
            }
        }
        if !LLVMIsAConstantExpr(v).is_null()
            && LLVMGetConstOpcode(v) == LLVMOpcode::LLVMGetElementPtr
        {
            self.visit_gep(v);
        }
    }

    unsafe fn visit_gep(&mut self, gep: LLVMValueRef) {
        let n = LLVMGetNumOperands(gep) as u32;
        if n < 2 {
            return;
        }
        let base = LLVMGetOperand(gep, 0);
        if LLVMIsAGlobalValue(base).is_null() {
            self.conflicts.push(format!(
                "constant GEP with non-global base: {}",
                print_value(gep)
            ));
            return;
        }
        let base_name = value_name(base);
        let mut indices: Vec<i64> = Vec::new();
        for i in 1..n {
            let op = LLVMGetOperand(gep, i);
            if LLVMIsAConstantInt(op).is_null() {
                self.conflicts.push(format!(
                    "constant GEP with non-integer index: {}",
                    print_value(gep)
                ));
                return;
            }
            indices.push(LLVMConstIntGetSExtValue(op));
        }
        let mut ty = LLVMGetGEPSourceElementType(gep);
        let mut off: i64 = indices[0] * LLVMABISizeOfType(self.td, ty) as i64;
        for &idx in &indices[1..] {
            match LLVMGetTypeKind(ty) {
                LLVMTypeKind::LLVMStructTypeKind => {
                    off += LLVMOffsetOfElement(self.td, ty, idx as u32) as i64;
                    ty = LLVMStructGetTypeAtIndex(ty, idx as u32);
                }
                LLVMTypeKind::LLVMArrayTypeKind | LLVMTypeKind::LLVMVectorTypeKind => {
                    ty = LLVMGetElementType(ty);
                    off += idx * LLVMABISizeOfType(self.td, ty) as i64;
                }
                _ => {
                    self.conflicts.push(format!(
                        "constant GEP indexes into non-aggregate: {}",
                        print_value(gep)
                    ));
                    return;
                }
            }
        }
        let key = (base_name, indices);
        if let Some(prev) = self.table.map.get(&key) {
            if *prev != off {
                self.conflicts.push(format!(
                    "ambiguous constant GEP (same base and indices, different offsets): {}",
                    print_value(gep)
                ));
            }
        } else {
            self.table.map.insert(key, off);
        }
    }
}

unsafe fn value_name(v: LLVMValueRef) -> String {
    let mut len: usize = 0;
    let p = LLVMGetValueName2(v, &mut len);
    let bytes = std::slice::from_raw_parts(p as *const u8, len);
    String::from_utf8_lossy(bytes).into_owned()
}

unsafe fn print_value(v: LLVMValueRef) -> String {
    let s = LLVMPrintValueToString(v);
    let out = CStr::from_ptr(s).to_string_lossy().into_owned();
    LLVMDisposeMessage(s);
    out
}

/// Build the constant-GEP offset table for an LLVM IR (`.ll`) or bitcode (`.bc`) file.
pub fn load_gep_table(path: &Path) -> Result<GepTable> {
    let bytes = std::fs::read(path)?;
    let is_bc = bytes.starts_with(b"BC\xC0\xDE") || bytes.starts_with(b"\xDE\xC0\x17\x0B");
    unsafe {
        let ctx = LLVMContextCreate();
        let cpath = CString::new(path.to_string_lossy().as_bytes())?;
        let mut buf: LLVMMemoryBufferRef = null_mut();
        let mut msg: *mut std::os::raw::c_char = null_mut();
        if LLVMCreateMemoryBufferWithContentsOfFile(cpath.as_ptr(), &mut buf, &mut msg) != 0 {
            let m = CStr::from_ptr(msg).to_string_lossy().into_owned();
            LLVMDisposeMessage(msg);
            LLVMContextDispose(ctx);
            bail!("cannot read {}: {}", path.display(), m);
        }
        let mut module: LLVMModuleRef = null_mut();
        if is_bc {
            let rc = LLVMParseBitcodeInContext2(ctx, buf, &mut module);
            LLVMDisposeMemoryBuffer(buf);
            if rc != 0 {
                LLVMContextDispose(ctx);
                bail!("cannot parse bitcode {}", path.display());
            }
        } else {
            // Takes ownership of buf.
            if LLVMParseIRInContext(ctx, buf, &mut module, &mut msg) != 0 {
                let m = CStr::from_ptr(msg).to_string_lossy().into_owned();
                LLVMDisposeMessage(msg);
                LLVMContextDispose(ctx);
                bail!("cannot parse IR {}: {}", path.display(), m);
            }
        }
        let td = LLVMGetModuleDataLayout(module);
        let mut w = Walker {
            td,
            seen: HashSet::new(),
            table: GepTable::default(),
            conflicts: Vec::new(),
        };
        let mut g = LLVMGetFirstGlobal(module);
        while !g.is_null() {
            let init = LLVMGetInitializer(g);
            if !init.is_null() {
                w.visit_value(init);
            }
            g = LLVMGetNextGlobal(g);
        }
        let mut f = LLVMGetFirstFunction(module);
        while !f.is_null() {
            let mut bb = LLVMGetFirstBasicBlock(f);
            while !bb.is_null() {
                let mut inst = LLVMGetFirstInstruction(bb);
                while !inst.is_null() {
                    let n = LLVMGetNumOperands(inst);
                    for i in 0..n {
                        w.visit_value(LLVMGetOperand(inst, i as u32));
                    }
                    inst = LLVMGetNextInstruction(inst);
                }
                bb = LLVMGetNextBasicBlock(bb);
            }
            f = LLVMGetNextFunction(f);
        }
        LLVMDisposeModule(module);
        LLVMContextDispose(ctx);
        if !w.conflicts.is_empty() {
            return Err(anyhow!(
                "unsupported constant expressions:\n  {}",
                w.conflicts.join("\n  ")
            ));
        }
        Ok(w.table)
    }
}
