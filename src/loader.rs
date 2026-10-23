//! Builds the snapshot IR (`ir.rs`) from an LLVM module.
//!
//! `inkwell` is used to own the context and module and to iterate over
//! functions, blocks and instructions; the details that `inkwell` does not
//! expose (switch cases, GEP source types, constant expressions, the module's
//! `TargetData`) are read through the LLVM C API on the underlying references.
//! This is the only module in the crate that contains `unsafe` code.

use crate::ir::*;
use anyhow::{anyhow, bail, Context as _, Result};
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::module::Module as LlvmModule;
use inkwell::values::{AsValueRef, InstructionOpcode as Opc};
use llvm_sys::core::*;
use llvm_sys::prelude::*;
use llvm_sys::target::*;
use llvm_sys::{LLVMIntPredicate, LLVMOpcode, LLVMTypeKind, LLVMValueKind};
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::Path;

/// Load an LLVM IR (`.ll`) or bitcode (`.bc`) file.
pub fn load(path: &Path) -> Result<Module> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let is_bc = bytes.starts_with(b"BC\xC0\xDE") || bytes.starts_with(b"\xDE\xC0\x17\x0B");
    let ctx = Context::create();
    let module = if is_bc {
        LlvmModule::parse_bitcode_from_path(path, &ctx)
    } else {
        let buf = MemoryBuffer::create_from_file(path).map_err(|e| anyhow!(e.to_string()))?;
        ctx.create_module_from_ir(buf)
    }
    .map_err(|e| anyhow!("{}", e.to_string()))
    .with_context(|| format!("parsing {}", path.display()))?;
    Loader::new(&module).load()
}

struct Loader<'m, 'ctx> {
    module: &'m LlvmModule<'ctx>,
    td: LLVMTargetDataRef,
    types: TypeTable,
    type_ids: HashMap<LLVMTypeRef, TypeId>,
    func_ids: HashMap<LLVMValueRef, FuncId>,
    global_ids: HashMap<LLVMValueRef, GlobalId>,
}

/// Per-function conversion state.
struct FnCtx {
    values: HashMap<LLVMValueRef, Value>,
    blocks: HashMap<LLVMBasicBlockRef, BlockId>,
}

unsafe fn cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn value_name(v: LLVMValueRef) -> String {
    let mut len = 0usize;
    let p = LLVMGetValueName2(v, &mut len);
    String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned()
}

unsafe fn print_value(v: LLVMValueRef) -> String {
    let s = LLVMPrintValueToString(v);
    let out = cstr(s).trim().to_string();
    LLVMDisposeMessage(s);
    out
}

unsafe fn print_type(t: LLVMTypeRef) -> String {
    let s = LLVMPrintTypeToString(t);
    let out = cstr(s);
    LLVMDisposeMessage(s);
    out
}

impl<'m, 'ctx> Loader<'m, 'ctx> {
    fn new(module: &'m LlvmModule<'ctx>) -> Self {
        let td = unsafe { LLVMGetModuleDataLayout(module.as_mut_ptr()) };
        let ptr_bytes = unsafe { LLVMPointerSize(td) } as usize;
        Loader {
            module,
            td,
            types: TypeTable {
                types: Vec::new(),
                ptr_bytes,
            },
            type_ids: HashMap::new(),
            func_ids: HashMap::new(),
            global_ids: HashMap::new(),
        }
    }

    fn load(mut self) -> Result<Module> {
        // Pass 1: ids for globals and functions, so constants can refer to them.
        let mut global_refs = Vec::new();
        for g in self.module.get_globals() {
            self.global_ids.insert(g.as_value_ref(), global_refs.len());
            global_refs.push(g.as_value_ref());
        }
        let mut func_refs = Vec::new();
        for f in self.module.get_functions() {
            self.func_ids.insert(f.as_value_ref(), func_refs.len());
            func_refs.push(f.as_value_ref());
        }
        // Pass 2: globals.
        let mut globals = Vec::new();
        for &g in &global_refs {
            let name = unsafe { value_name(g) };
            let ty = unsafe { self.intern(LLVMGlobalGetValueType(g)) }?;
            let init_ref = unsafe { LLVMGetInitializer(g) };
            let init = if init_ref.is_null() {
                None
            } else {
                Some(
                    unsafe { self.constant(init_ref) }
                        .with_context(|| format!("initializer of @{}", name))?,
                )
            };
            let align = unsafe { LLVMGetAlignment(g) } as usize;
            globals.push(Global {
                name,
                ty,
                align,
                init,
            });
        }
        // Pass 3: functions.
        let mut functions = Vec::new();
        for &f in &func_refs {
            let name = unsafe { value_name(f) };
            let func =
                unsafe { self.function(f) }.with_context(|| format!("in function @{}", name))?;
            functions.push(func);
        }
        let target_triple = unsafe { cstr(LLVMGetTarget(self.module.as_mut_ptr())) };
        Ok(Module {
            types: self.types,
            functions,
            globals,
            target_triple,
        })
    }

    // -----------------------------------------------------------------------
    // Types
    // -----------------------------------------------------------------------

    unsafe fn intern(&mut self, t: LLVMTypeRef) -> Result<TypeId> {
        if let Some(&id) = self.type_ids.get(&t) {
            return Ok(id);
        }
        let name = print_type(t);
        let kind = match LLVMGetTypeKind(t) {
            LLVMTypeKind::LLVMVoidTypeKind => TypeKind::Void,
            LLVMTypeKind::LLVMIntegerTypeKind => TypeKind::Int {
                bits: LLVMGetIntTypeWidth(t),
            },
            LLVMTypeKind::LLVMPointerTypeKind => TypeKind::Ptr,
            LLVMTypeKind::LLVMArrayTypeKind => {
                let elem = self.intern(LLVMGetElementType(t))?;
                TypeKind::Array {
                    elem,
                    len: LLVMGetArrayLength2(t) as usize,
                }
            }
            LLVMTypeKind::LLVMStructTypeKind => {
                if LLVMIsOpaqueStruct(t) != 0 {
                    TypeKind::Unsupported
                } else {
                    let n = LLVMCountStructElementTypes(t);
                    let mut fields = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        let ty = self.intern(LLVMStructGetTypeAtIndex(t, i))?;
                        let offset = LLVMOffsetOfElement(self.td, t, i) as usize;
                        fields.push(Field { ty, offset });
                    }
                    TypeKind::Struct {
                        fields,
                        packed: LLVMIsPackedStruct(t) != 0,
                    }
                }
            }
            LLVMTypeKind::LLVMFunctionTypeKind => {
                let ret = self.intern(LLVMGetReturnType(t))?;
                let n = LLVMCountParamTypes(t) as usize;
                let mut raw: Vec<LLVMTypeRef> = vec![std::ptr::null_mut(); n];
                LLVMGetParamTypes(t, raw.as_mut_ptr());
                let mut params = Vec::with_capacity(n);
                for p in raw {
                    params.push(self.intern(p)?);
                }
                TypeKind::Func {
                    ret,
                    params,
                    variadic: LLVMIsFunctionVarArg(t) != 0,
                }
            }
            _ => TypeKind::Unsupported,
        };
        let (size, align) = match kind {
            TypeKind::Void | TypeKind::Func { .. } | TypeKind::Unsupported => (0, 1),
            _ => (
                LLVMABISizeOfType(self.td, t) as usize,
                LLVMABIAlignmentOfType(self.td, t) as usize,
            ),
        };
        let id = self.types.types.len();
        self.types.types.push(TypeDef {
            kind,
            size,
            align,
            name,
        });
        self.type_ids.insert(t, id);
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    unsafe fn constant(&mut self, v: LLVMValueRef) -> Result<Constant> {
        if !LLVMIsAConstantInt(v).is_null() {
            return Ok(Constant::Int(LLVMConstIntGetZExtValue(v)));
        }
        if !LLVMIsAConstantPointerNull(v).is_null()
            || !LLVMIsAUndefValue(v).is_null()
            || !LLVMIsAPoisonValue(v).is_null()
            || !LLVMIsAConstantAggregateZero(v).is_null()
        {
            return Ok(Constant::Zero);
        }
        if !LLVMIsAFunction(v).is_null() {
            return self
                .func_ids
                .get(&v)
                .map(|&id| Constant::Func(id))
                .ok_or_else(|| anyhow!("unknown function"));
        }
        if !LLVMIsAGlobalVariable(v).is_null() {
            return self
                .global_ids
                .get(&v)
                .map(|&id| Constant::Global(id))
                .ok_or_else(|| anyhow!("unknown global"));
        }
        if !LLVMIsAConstantDataSequential(v).is_null() {
            let ty = LLVMTypeOf(v);
            if LLVMGetTypeKind(ty) != LLVMTypeKind::LLVMArrayTypeKind {
                bail!("unsupported constant {}", print_value(v));
            }
            let elem_size = LLVMABISizeOfType(self.td, LLVMGetElementType(ty)) as usize;
            let len = LLVMGetArrayLength2(ty) as usize;
            let mut bytes = Vec::with_capacity(len * elem_size);
            for i in 0..len {
                let e = LLVMGetAggregateElement(v, i as u32);
                if LLVMIsAConstantInt(e).is_null() {
                    bail!("unsupported constant data element in {}", print_value(v));
                }
                let x = LLVMConstIntGetZExtValue(e);
                for k in 0..elem_size {
                    bytes.push(((x >> (8 * k.min(7))) & 0xff) as u8);
                }
            }
            return Ok(Constant::Bytes(bytes));
        }
        if !LLVMIsAConstantArray(v).is_null() || !LLVMIsAConstantStruct(v).is_null() {
            let ty = self.intern(LLVMTypeOf(v))?;
            let n = LLVMGetNumOperands(v) as usize;
            let mut leaves = Vec::with_capacity(n);
            for i in 0..n {
                let (ety, offset) = self.types.element(ty, i)?;
                let value = self.constant(LLVMGetOperand(v, i as u32))?;
                leaves.push(AggLeaf {
                    offset,
                    ty: ety,
                    value,
                });
            }
            return Ok(Constant::Agg(leaves));
        }
        if !LLVMIsAConstantExpr(v).is_null() {
            return match LLVMGetConstOpcode(v) {
                LLVMOpcode::LLVMGetElementPtr => {
                    let base = self.constant(LLVMGetOperand(v, 0))?;
                    let bytes = self.gep_offset(v)?;
                    Ok(Constant::Offset {
                        base: Box::new(base),
                        bytes,
                    })
                }
                LLVMOpcode::LLVMPtrToInt
                | LLVMOpcode::LLVMIntToPtr
                | LLVMOpcode::LLVMBitCast
                | LLVMOpcode::LLVMAddrSpaceCast
                | LLVMOpcode::LLVMTrunc => self.constant(LLVMGetOperand(v, 0)),
                _ => bail!("unsupported constant expression {}", print_value(v)),
            };
        }
        bail!("unsupported constant {}", print_value(v))
    }

    /// Byte offset of a constant GEP whose indices are all constant integers.
    unsafe fn gep_offset(&mut self, gep: LLVMValueRef) -> Result<i64> {
        let n = LLVMGetNumOperands(gep) as u32;
        let mut indices = Vec::new();
        for i in 1..n {
            let op = LLVMGetOperand(gep, i);
            if LLVMIsAConstantInt(op).is_null() {
                bail!("non-constant index in constant GEP {}", print_value(gep));
            }
            indices.push(LLVMConstIntGetSExtValue(op));
        }
        if indices.is_empty() {
            return Ok(0);
        }
        let mut ty = LLVMGetGEPSourceElementType(gep);
        let mut off: i64 = indices[0] * LLVMABISizeOfType(self.td, ty) as i64;
        for &idx in &indices[1..] {
            match LLVMGetTypeKind(ty) {
                LLVMTypeKind::LLVMStructTypeKind => {
                    off += LLVMOffsetOfElement(self.td, ty, idx as u32) as i64;
                    ty = LLVMStructGetTypeAtIndex(ty, idx as u32);
                }
                LLVMTypeKind::LLVMArrayTypeKind => {
                    ty = LLVMGetElementType(ty);
                    off += idx * LLVMABISizeOfType(self.td, ty) as i64;
                }
                _ => bail!(
                    "constant GEP indexes into a non-aggregate: {}",
                    print_value(gep)
                ),
            }
        }
        Ok(off)
    }

    // -----------------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------------

    unsafe fn function(&mut self, f: LLVMValueRef) -> Result<Function> {
        let name = value_name(f);
        let fty_ref = LLVMGlobalGetValueType(f);
        let ty = self.intern(fty_ref)?;
        let (ret, variadic) = match self.types.kind(ty) {
            TypeKind::Func { ret, variadic, .. } => (*ret, *variadic),
            _ => bail!("function without function type"),
        };
        let mut fx = FnCtx {
            values: HashMap::new(),
            blocks: HashMap::new(),
        };
        let mut values: Vec<ValueInfo> = Vec::new();
        let mut params = Vec::new();
        for i in 0..LLVMCountParams(f) {
            let p = LLVMGetParam(f, i);
            let v = Value(values.len() as u32);
            values.push(ValueInfo {
                ty: self.intern(LLVMTypeOf(p))?,
                name: value_name(p),
            });
            fx.values.insert(p, v);
            params.push(v);
        }
        // Number blocks and instruction results before converting anything, since
        // operands may refer to values defined later in the function.
        let mut bb_refs = Vec::new();
        let mut bb = LLVMGetFirstBasicBlock(f);
        while !bb.is_null() {
            fx.blocks.insert(bb, bb_refs.len());
            bb_refs.push(bb);
            let mut inst = LLVMGetFirstInstruction(bb);
            while !inst.is_null() {
                let ity = LLVMTypeOf(inst);
                if LLVMGetTypeKind(ity) != LLVMTypeKind::LLVMVoidTypeKind {
                    let v = Value(values.len() as u32);
                    values.push(ValueInfo {
                        ty: self.intern(ity)?,
                        name: value_name(inst),
                    });
                    fx.values.insert(inst, v);
                }
                inst = LLVMGetNextInstruction(inst);
            }
            bb = LLVMGetNextBasicBlock(bb);
        }
        let mut blocks = Vec::with_capacity(bb_refs.len());
        for &bb in &bb_refs {
            blocks.push(self.block(bb, &fx)?);
        }
        Ok(Function {
            name,
            ty,
            params,
            ret,
            variadic,
            values,
            blocks,
        })
    }

    unsafe fn block(&mut self, bb: LLVMBasicBlockRef, fx: &FnCtx) -> Result<Block> {
        let name = value_name(LLVMBasicBlockAsValue(bb));
        let mut phis = Vec::new();
        let mut instrs = Vec::new();
        let mut term = None;
        let mut inst = LLVMGetFirstInstruction(bb);
        while !inst.is_null() {
            let text = print_value(inst);
            let iv = inkwell::values::InstructionValue::new(inst);
            let opcode = iv.get_opcode();
            let dest = fx.values.get(&inst).copied();
            let ty = self.intern(LLVMTypeOf(inst))?;
            match opcode {
                Opc::Phi => {
                    let dest = dest.ok_or_else(|| anyhow!("phi without result"))?;
                    let n = LLVMCountIncoming(inst);
                    let mut incoming = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        let val = self
                            .operand(LLVMGetIncomingValue(inst, i), fx)
                            .with_context(|| text.clone())?;
                        let blk = self.block_id(LLVMGetIncomingBlock(inst, i), fx)?;
                        incoming.push((val, blk));
                    }
                    phis.push(Phi { dest, ty, incoming });
                }
                Opc::Return | Opc::UncondBr | Opc::CondBr | Opc::Switch | Opc::Unreachable => {
                    term = Some((
                        self.terminator(inst, opcode, fx)
                            .with_context(|| text.clone())?,
                        text,
                    ));
                }
                _ => {
                    let op = self
                        .instruction(inst, opcode, fx)
                        .with_context(|| text.clone())?;
                    instrs.push(Instr { dest, ty, op, text });
                }
            }
            inst = LLVMGetNextInstruction(inst);
        }
        let (term, term_text) = term.ok_or_else(|| anyhow!("block %{} has no terminator", name))?;
        Ok(Block {
            name,
            phis,
            instrs,
            term,
            term_text,
        })
    }

    unsafe fn block_id(&self, bb: LLVMBasicBlockRef, fx: &FnCtx) -> Result<BlockId> {
        fx.blocks
            .get(&bb)
            .copied()
            .ok_or_else(|| anyhow!("reference to a block of another function"))
    }

    unsafe fn operand(&mut self, v: LLVMValueRef, fx: &FnCtx) -> Result<Operand> {
        let ty = self.intern(LLVMTypeOf(v))?;
        if let Some(&val) = fx.values.get(&v) {
            return Ok(Operand::value(ty, val));
        }
        if !LLVMIsAConstant(v).is_null() {
            return Ok(Operand::constant(ty, self.constant(v)?));
        }
        bail!("unsupported operand {}", print_value(v))
    }

    unsafe fn op(&mut self, inst: LLVMValueRef, i: u32, fx: &FnCtx) -> Result<Operand> {
        self.operand(LLVMGetOperand(inst, i), fx)
    }

    unsafe fn terminator(
        &mut self,
        inst: LLVMValueRef,
        opcode: Opc,
        fx: &FnCtx,
    ) -> Result<Terminator> {
        Ok(match opcode {
            Opc::Return => {
                if LLVMGetNumOperands(inst) == 0 {
                    Terminator::Ret(None)
                } else {
                    Terminator::Ret(Some(self.op(inst, 0, fx)?))
                }
            }
            Opc::UncondBr | Opc::CondBr => {
                if !LLVMIsACondBrInst(inst).is_null() {
                    let cond = self.operand(LLVMGetCondition(inst), fx)?;
                    let if_true = self.block_id(LLVMGetSuccessor(inst, 0), fx)?;
                    let if_false = self.block_id(LLVMGetSuccessor(inst, 1), fx)?;
                    Terminator::CondBr {
                        cond,
                        if_true,
                        if_false,
                    }
                } else {
                    Terminator::Br(self.block_id(LLVMGetSuccessor(inst, 0), fx)?)
                }
            }
            Opc::Switch => {
                // Operands are [condition, default, dest_1, ..., dest_n]; the case
                // values live out of line and are read by successor index (1-based,
                // successor 0 being the default).
                let value = self.op(inst, 0, fx)?;
                let default = self.block_id(LLVMGetSwitchDefaultDest(inst), fx)?;
                let num_cases = LLVMGetNumOperands(inst) as u32 - 2;
                let mut cases = Vec::with_capacity(num_cases as usize);
                for i in 0..num_cases {
                    let c = LLVMGetSwitchCaseValue(inst, i + 1);
                    if LLVMIsAConstantInt(c).is_null() {
                        bail!("non-constant switch case");
                    }
                    let blk = self.block_id(LLVMGetSuccessor(inst, i + 1), fx)?;
                    cases.push((LLVMConstIntGetZExtValue(c), blk));
                }
                Terminator::Switch {
                    value,
                    cases,
                    default,
                }
            }
            Opc::Unreachable => Terminator::Unreachable,
            _ => bail!("unsupported terminator"),
        })
    }

    unsafe fn instruction(&mut self, inst: LLVMValueRef, opcode: Opc, fx: &FnCtx) -> Result<Op> {
        let binary = |kind: BinOp, this: &mut Self| -> Result<Op> {
            Ok(Op::Binary {
                kind,
                a: this.op(inst, 0, fx)?,
                b: this.op(inst, 1, fx)?,
            })
        };
        let cast = |kind: CastKind, this: &mut Self| -> Result<Op> {
            Ok(Op::Cast {
                kind,
                x: this.op(inst, 0, fx)?,
            })
        };
        Ok(match opcode {
            Opc::Add => binary(BinOp::Add, self)?,
            Opc::Sub => binary(BinOp::Sub, self)?,
            Opc::Mul => binary(BinOp::Mul, self)?,
            Opc::UDiv => binary(BinOp::UDiv, self)?,
            Opc::SDiv => binary(BinOp::SDiv, self)?,
            Opc::URem => binary(BinOp::URem, self)?,
            Opc::SRem => binary(BinOp::SRem, self)?,
            Opc::And => binary(BinOp::And, self)?,
            Opc::Or => binary(BinOp::Or, self)?,
            Opc::Xor => binary(BinOp::Xor, self)?,
            Opc::Shl => binary(BinOp::Shl, self)?,
            Opc::LShr => binary(BinOp::LShr, self)?,
            Opc::AShr => binary(BinOp::AShr, self)?,
            Opc::ICmp => {
                let pred = match LLVMGetICmpPredicate(inst) {
                    LLVMIntPredicate::LLVMIntEQ => Pred::Eq,
                    LLVMIntPredicate::LLVMIntNE => Pred::Ne,
                    LLVMIntPredicate::LLVMIntULT => Pred::Ult,
                    LLVMIntPredicate::LLVMIntULE => Pred::Ule,
                    LLVMIntPredicate::LLVMIntUGT => Pred::Ugt,
                    LLVMIntPredicate::LLVMIntUGE => Pred::Uge,
                    LLVMIntPredicate::LLVMIntSLT => Pred::Slt,
                    LLVMIntPredicate::LLVMIntSLE => Pred::Sle,
                    LLVMIntPredicate::LLVMIntSGT => Pred::Sgt,
                    LLVMIntPredicate::LLVMIntSGE => Pred::Sge,
                };
                Op::ICmp {
                    pred,
                    a: self.op(inst, 0, fx)?,
                    b: self.op(inst, 1, fx)?,
                }
            }
            Opc::ZExt => cast(CastKind::ZExt, self)?,
            Opc::SExt => cast(CastKind::SExt, self)?,
            Opc::Trunc => cast(CastKind::Trunc, self)?,
            Opc::PtrToInt | Opc::PtrToAddr | Opc::IntToPtr | Opc::BitCast | Opc::AddrSpaceCast => {
                cast(CastKind::Reinterpret, self)?
            }
            Opc::Freeze => Op::Freeze {
                x: self.op(inst, 0, fx)?,
            },
            Opc::Load => Op::Load {
                addr: self.op(inst, 0, fx)?,
                align: LLVMGetAlignment(inst) as usize,
            },
            Opc::Store => Op::Store {
                value: self.op(inst, 0, fx)?,
                addr: self.op(inst, 1, fx)?,
                align: LLVMGetAlignment(inst) as usize,
            },
            Opc::GetElementPtr => {
                let base = self.op(inst, 0, fx)?;
                let source_ty = self.intern(LLVMGetGEPSourceElementType(inst))?;
                let mut indices = Vec::new();
                for i in 1..LLVMGetNumOperands(inst) as u32 {
                    indices.push(self.op(inst, i, fx)?);
                }
                Op::Gep {
                    base,
                    source_ty,
                    indices,
                }
            }
            Opc::Alloca => {
                let ty = self.intern(LLVMGetAllocatedType(inst))?;
                let count_ref = LLVMGetOperand(inst, 0);
                if LLVMIsAConstantInt(count_ref).is_null() {
                    bail!("alloca with a variable element count is not supported");
                }
                Op::Alloca {
                    ty,
                    count: LLVMConstIntGetZExtValue(count_ref),
                    align: LLVMGetAlignment(inst) as usize,
                }
            }
            Opc::Select => Op::Select {
                cond: self.op(inst, 0, fx)?,
                if_true: self.op(inst, 1, fx)?,
                if_false: self.op(inst, 2, fx)?,
            },
            Opc::ExtractValue => Op::ExtractValue {
                agg: self.op(inst, 0, fx)?,
                indices: self.indices(inst),
            },
            Opc::InsertValue => Op::InsertValue {
                agg: self.op(inst, 0, fx)?,
                elem: self.op(inst, 1, fx)?,
                indices: self.indices(inst),
            },
            Opc::VAArg => Op::VaArg {
                list: self.op(inst, 0, fx)?,
            },
            Opc::Call => self.call(inst, fx)?,
            _ => bail!("unsupported instruction"),
        })
    }

    unsafe fn indices(&self, inst: LLVMValueRef) -> Vec<u32> {
        let n = LLVMGetNumIndices(inst) as usize;
        std::slice::from_raw_parts(LLVMGetIndices(inst), n).to_vec()
    }

    unsafe fn call(&mut self, inst: LLVMValueRef, fx: &FnCtx) -> Result<Op> {
        let callee_ref = LLVMGetCalledValue(inst);
        let fn_ty = self.intern(LLVMGetCalledFunctionType(inst))?;
        if !LLVMIsAInlineAsm(callee_ref).is_null() {
            bail!("inline assembly is not supported");
        }
        let callee = if !LLVMIsAFunction(callee_ref).is_null() {
            let name = value_name(callee_ref);
            if name.starts_with("llvm.") || name.starts_with("__llvbf_") {
                Callee::Intrinsic(name)
            } else {
                Callee::Direct(self.func_ids[&callee_ref])
            }
        } else {
            Callee::Indirect(self.operand(callee_ref, fx)?)
        };
        let intrinsic = matches!(callee, Callee::Intrinsic(_));
        let mut args = Vec::new();
        for i in 0..LLVMGetNumArgOperands(inst) {
            let a = LLVMGetOperand(inst, i);
            // Metadata arguments only occur on intrinsics the backend ignores.
            if intrinsic && LLVMGetValueKind(a) == LLVMValueKind::LLVMMetadataAsValueValueKind {
                continue;
            }
            args.push(self.operand(a, fx)?);
        }
        Ok(Op::Call {
            callee,
            fn_ty,
            args,
        })
    }
}
