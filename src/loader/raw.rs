//! The only `unsafe` code in llvbf: the handful of LLVM C API calls that
//! `inkwell` does not wrap. Every function here has a safe signature, takes
//! `inkwell` values (so the underlying references are valid for `'ctx`), checks
//! the preconditions LLVM itself does not check, and documents what it relies on.
//!
//! Keep this module small. Anything that can be done through `inkwell`'s safe
//! API belongs in `loader/mod.rs`.

use inkwell::types::{AnyTypeEnum, BasicTypeEnum, FunctionType};
use inkwell::values::{
    ArrayValue, AsValueRef, BasicValueEnum, CallSiteValue, InstructionOpcode, InstructionValue,
};
use llvm_sys::core::*;
use llvm_sys::{LLVMOpcode, LLVMTypeKind, LLVMValueKind};

/// A constant expression, decoded.
pub enum ConstExpr<'ctx> {
    /// `getelementptr` with constant integer indices.
    Gep {
        base: BasicValueEnum<'ctx>,
        source_ty: BasicTypeEnum<'ctx>,
        indices: Vec<i64>,
    },
    /// `ptrtoint`, `inttoptr`, `bitcast`, `addrspacecast`, `trunc`: the operand's bits.
    Cast(BasicValueEnum<'ctx>),
    /// Anything else (reported, never lowered).
    Other(String),
}

/// The case values of a `switch`, in successor order (successor 1 onwards; successor 0
/// is the default). Returns an empty vector for any other instruction.
///
/// LLVM stores the case values out of line; `inkwell` exposes only the destinations.
/// Relies on `LLVMGetSwitchCaseValue(inst, i)` taking a *successor* index `i >= 1`.
pub fn switch_case_values(inst: InstructionValue<'_>) -> Vec<u64> {
    if inst.get_opcode() != InstructionOpcode::Switch {
        return Vec::new();
    }
    let num_successors = inst.get_num_operands().saturating_sub(1);
    (1..num_successors)
        .map(|i| unsafe {
            LLVMConstIntGetZExtValue(LLVMGetSwitchCaseValue(inst.as_value_ref(), i))
        })
        .collect()
}

/// Decode a constant expression. Returns `None` if `v` is not a constant expression.
///
/// `inkwell` can tell that a value is constant but not which expression it is.
pub fn const_expr(v: BasicValueEnum<'_>) -> Option<ConstExpr<'_>> {
    let r = v.as_value_ref();
    unsafe {
        if LLVMIsAConstantExpr(r).is_null() {
            return None;
        }
        // All operands of a constant expression are constants of basic types.
        let operand = |i: u32| BasicValueEnum::new(LLVMGetOperand(r, i));
        Some(match LLVMGetConstOpcode(r) {
            LLVMOpcode::LLVMGetElementPtr => {
                let n = LLVMGetNumOperands(r) as u32;
                let mut indices = Vec::with_capacity(n as usize);
                for i in 1..n {
                    let op = LLVMGetOperand(r, i);
                    if LLVMIsAConstantInt(op).is_null() {
                        return Some(ConstExpr::Other(print(r)));
                    }
                    indices.push(LLVMConstIntGetSExtValue(op));
                }
                let source_ty = BasicTypeEnum::new(LLVMGetGEPSourceElementType(r));
                ConstExpr::Gep {
                    base: operand(0),
                    source_ty,
                    indices,
                }
            }
            LLVMOpcode::LLVMPtrToInt
            | LLVMOpcode::LLVMIntToPtr
            | LLVMOpcode::LLVMBitCast
            | LLVMOpcode::LLVMAddrSpaceCast
            | LLVMOpcode::LLVMTrunc => ConstExpr::Cast(operand(0)),
            _ => ConstExpr::Other(print(r)),
        })
    }
}

/// The elements of a constant array (including `zeroinitializer`, `undef` and
/// constant data arrays). Returns `None` if the array is not a constant.
///
/// `inkwell` has element access for struct constants but not for arrays.
/// Relies on `LLVMGetAggregateElement` returning null for out-of-range indices only.
pub fn array_elements(a: ArrayValue<'_>) -> Option<Vec<BasicValueEnum<'_>>> {
    if !a.is_const() {
        return None;
    }
    let n = a.get_type().len();
    let r = a.as_value_ref();
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let e = unsafe { LLVMGetAggregateElement(r, i) };
        if e.is_null() {
            return None;
        }
        out.push(unsafe { BasicValueEnum::new(e) });
    }
    Some(out)
}

/// Whether operand `i` of an instruction is metadata (which `inkwell`'s typed
/// operand accessor cannot represent). Out-of-range indices return `false`.
pub fn operand_is_metadata(inst: InstructionValue<'_>, i: u32) -> bool {
    if i >= inst.get_num_operands() {
        return false;
    }
    unsafe {
        let op = LLVMGetOperand(inst.as_value_ref(), i);
        LLVMGetValueKind(op) == LLVMValueKind::LLVMMetadataAsValueValueKind
            || LLVMGetTypeKind(LLVMTypeOf(op)) == LLVMTypeKind::LLVMMetadataTypeKind
    }
}

/// The function type a call site calls through (needed for indirect calls, where
/// there is no callee `FunctionValue` to ask).
pub fn called_function_type(call: CallSiteValue<'_>) -> FunctionType<'_> {
    unsafe { AnyTypeEnum::new(LLVMGetCalledFunctionType(call.as_value_ref())).into_function_type() }
}

fn print(r: llvm_sys::prelude::LLVMValueRef) -> String {
    unsafe {
        let s = LLVMPrintValueToString(r);
        let out = std::ffi::CStr::from_ptr(s)
            .to_string_lossy()
            .trim()
            .to_string();
        LLVMDisposeMessage(s);
        out
    }
}
