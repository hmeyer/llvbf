//! Builds the snapshot IR (`ir.rs`) from an LLVM module through `inkwell`'s safe
//! API. The few things `inkwell` does not expose live in `raw.rs`, which is the
//! only `unsafe` code in the crate.

mod raw;

use crate::ir::*;
use anyhow::{anyhow, bail, Context as _, Result};
use inkwell::basic_block::BasicBlock;
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::module::Module as LlvmModule;
use inkwell::targets::TargetData;
use inkwell::types::{
    AnyType, AnyTypeEnum, AsTypeRef, BasicMetadataTypeEnum, BasicType, BasicTypeEnum,
};
use inkwell::values::{
    AnyValue, AsValueRef, BasicValue, BasicValueEnum, CallSiteValue, FunctionValue,
    InstructionOpcode as Opc, InstructionValue, Operand as LlvmOperand, PhiValue,
};
use inkwell::IntPredicate;
use std::collections::HashMap;
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
    Loader::new(&module).load(&module)
}

/// Identity of an LLVM value or type, used as a map key only (never dereferenced).
type Key = usize;

fn vkey<V: AsValueRef>(v: &V) -> Key {
    v.as_value_ref() as Key
}

fn tkey<T: AsTypeRef>(t: &T) -> Key {
    t.as_type_ref() as Key
}

fn bkey(b: &BasicBlock<'_>) -> Key {
    b.as_mut_ptr() as Key
}

struct Loader<'ctx> {
    td: TargetData,
    void_ty: AnyTypeEnum<'ctx>,
    types: TypeTable,
    type_ids: HashMap<Key, TypeId>,
    func_ids: HashMap<Key, FuncId>,
    global_ids: HashMap<Key, GlobalId>,
}

/// Per-function conversion state.
struct FnCtx {
    values: HashMap<Key, Value>,
    blocks: HashMap<Key, BlockId>,
}

impl<'ctx> Loader<'ctx> {
    fn new(module: &LlvmModule<'ctx>) -> Self {
        let td = TargetData::create(module.get_data_layout().as_str().to_str().unwrap_or(""));
        let ptr_bytes = td.get_pointer_byte_size(None) as usize;
        Loader {
            td,
            void_ty: module.get_context().void_type().as_any_type_enum(),
            types: TypeTable {
                types: Vec::new(),
                ptr_bytes,
            },
            type_ids: HashMap::new(),
            func_ids: HashMap::new(),
            global_ids: HashMap::new(),
        }
    }

    fn load(mut self, module: &LlvmModule<'ctx>) -> Result<Module> {
        // Pass 1: ids for globals and functions, so constants can refer to them.
        let llvm_globals: Vec<_> = module.get_globals().collect();
        for (i, g) in llvm_globals.iter().enumerate() {
            self.global_ids.insert(vkey(g), i);
        }
        let llvm_functions: Vec<FunctionValue<'ctx>> = module.get_functions().collect();
        for (i, f) in llvm_functions.iter().enumerate() {
            self.func_ids.insert(vkey(f), i);
        }
        // Pass 2: globals.
        let mut globals = Vec::new();
        for g in &llvm_globals {
            let name = g.get_name().to_string_lossy().into_owned();
            let ty = self.intern(g.get_value_type())?;
            let init = match g.get_initializer() {
                Some(v) => Some(
                    self.constant(v)
                        .with_context(|| format!("initializer of @{}", name))?,
                ),
                None => None,
            };
            globals.push(Global {
                name,
                ty,
                align: g.get_alignment() as usize,
                init,
            });
        }
        // Pass 3: functions.
        let mut functions = Vec::new();
        for f in &llvm_functions {
            let name = f.get_name().to_string_lossy().into_owned();
            let func = self
                .function(*f)
                .with_context(|| format!("in function @{}", name))?;
            functions.push(func);
        }
        let target_triple = module.get_triple().as_str().to_string_lossy().into_owned();
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

    fn intern(&mut self, t: AnyTypeEnum<'ctx>) -> Result<TypeId> {
        if let Some(&id) = self.type_ids.get(&tkey(&t)) {
            return Ok(id);
        }
        let name = t.print_to_string().to_string();
        let kind = match t {
            AnyTypeEnum::VoidType(_) => TypeKind::Void,
            AnyTypeEnum::IntType(i) => TypeKind::Int {
                bits: i.get_bit_width(),
            },
            AnyTypeEnum::PointerType(_) => TypeKind::Ptr,
            AnyTypeEnum::ArrayType(a) => {
                let elem = self.intern(a.get_element_type().as_any_type_enum())?;
                TypeKind::Array {
                    elem,
                    len: a.len() as usize,
                }
            }
            AnyTypeEnum::StructType(s) => {
                if s.is_opaque() {
                    TypeKind::Unsupported
                } else {
                    let mut fields = Vec::new();
                    for (i, ft) in s.get_field_types().into_iter().enumerate() {
                        let ty = self.intern(ft.as_any_type_enum())?;
                        let offset = self
                            .td
                            .offset_of_element(&s, i as u32)
                            .ok_or_else(|| anyhow!("no offset for field {} of {}", i, name))?
                            as usize;
                        fields.push(Field { ty, offset });
                    }
                    TypeKind::Struct {
                        fields,
                        packed: s.is_packed(),
                    }
                }
            }
            AnyTypeEnum::FunctionType(f) => {
                let ret = match f.get_return_type() {
                    Some(r) => self.intern(r.as_any_type_enum())?,
                    None => self.intern(self.void_ty)?,
                };
                let mut params = Vec::new();
                for p in f.get_param_types() {
                    params.push(match p {
                        // Only intrinsics the backend ignores take metadata.
                        BasicMetadataTypeEnum::MetadataType(m) => {
                            self.intern_unsupported(tkey(&m), "metadata")
                        }
                        other => {
                            let bt: BasicTypeEnum<'ctx> = other
                                .try_into()
                                .map_err(|_| anyhow!("unsupported parameter type in {}", name))?;
                            self.intern(bt.as_any_type_enum())?
                        }
                    });
                }
                TypeKind::Func {
                    ret,
                    params,
                    variadic: f.is_var_arg(),
                }
            }
            _ => TypeKind::Unsupported,
        };
        let (size, align) = match kind {
            TypeKind::Void | TypeKind::Func { .. } | TypeKind::Unsupported => (0, 1),
            _ => (
                self.td.get_abi_size(&t) as usize,
                self.td.get_abi_alignment(&t) as usize,
            ),
        };
        let id = self.types.types.len();
        self.types.types.push(TypeDef {
            kind,
            size,
            align,
            name,
        });
        self.type_ids.insert(tkey(&t), id);
        Ok(id)
    }

    /// Intern a type the backend cannot represent (kept only so messages can name it).
    fn intern_unsupported(&mut self, key: Key, name: &str) -> TypeId {
        if let Some(&id) = self.type_ids.get(&key) {
            return id;
        }
        let id = self.types.types.len();
        self.types.types.push(TypeDef {
            kind: TypeKind::Unsupported,
            size: 0,
            align: 1,
            name: name.to_string(),
        });
        self.type_ids.insert(key, id);
        id
    }

    fn intern_basic(&mut self, t: BasicTypeEnum<'ctx>) -> Result<TypeId> {
        self.intern(t.as_any_type_enum())
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    fn constant(&mut self, v: BasicValueEnum<'ctx>) -> Result<Constant> {
        match v {
            BasicValueEnum::IntValue(i) => {
                if i.is_undef() {
                    return Ok(Constant::Zero);
                }
                if let Some(x) = i.get_zero_extended_constant() {
                    return Ok(Constant::Int(x));
                }
                self.const_expr(v)
            }
            BasicValueEnum::PointerValue(p) => {
                if p.is_null() || p.is_undef() {
                    return Ok(Constant::Zero);
                }
                if let Some(&f) = self.func_ids.get(&vkey(&p)) {
                    return Ok(Constant::Func(f));
                }
                if let Some(&g) = self.global_ids.get(&vkey(&p)) {
                    return Ok(Constant::Global(g));
                }
                self.const_expr(v)
            }
            BasicValueEnum::ArrayValue(a) => {
                let elems = raw::array_elements(a)
                    .ok_or_else(|| anyhow!("non-constant array {}", v.print_to_string()))?;
                let elem_ty = a.get_type().get_element_type();
                if let BasicTypeEnum::IntType(it) = elem_ty {
                    // Arrays of integers (strings, tables) as bytes.
                    let elem_size = self.td.get_abi_size(&it) as usize;
                    let mut bytes = Vec::with_capacity(elems.len() * elem_size);
                    for e in elems {
                        let x = match self.constant(e)? {
                            Constant::Int(x) => x,
                            Constant::Zero => 0,
                            _ => bail!("unsupported array element in {}", v.print_to_string()),
                        };
                        for k in 0..elem_size {
                            bytes.push(((x >> (8 * k.min(7))) & 0xff) as u8);
                        }
                    }
                    return Ok(Constant::Bytes(bytes));
                }
                let ety = self.intern_basic(elem_ty)?;
                let esize = self.types.size(ety);
                let mut leaves = Vec::with_capacity(elems.len());
                for (i, e) in elems.into_iter().enumerate() {
                    leaves.push(AggLeaf {
                        offset: i * esize,
                        ty: ety,
                        value: self.constant(e)?,
                    });
                }
                Ok(Constant::Agg(leaves))
            }
            BasicValueEnum::StructValue(s) => {
                let ty = self.intern_basic(s.get_type().as_basic_type_enum())?;
                let mut leaves = Vec::new();
                for i in 0..s.count_fields() {
                    let field = s
                        .get_field_at_index(i)
                        .ok_or_else(|| anyhow!("missing field {} of {}", i, v.print_to_string()))?;
                    let (fty, offset) = self.types.element(ty, i as usize)?;
                    leaves.push(AggLeaf {
                        offset,
                        ty: fty,
                        value: self.constant(field)?,
                    });
                }
                Ok(Constant::Agg(leaves))
            }
            _ => bail!("unsupported constant {}", v.print_to_string()),
        }
    }

    fn const_expr(&mut self, v: BasicValueEnum<'ctx>) -> Result<Constant> {
        match raw::const_expr(v) {
            Some(raw::ConstExpr::Gep {
                base,
                source_ty,
                indices,
            }) => {
                let base = self.constant(base)?;
                let bytes = self.gep_offset(source_ty, &indices)?;
                Ok(Constant::Offset {
                    base: Box::new(base),
                    bytes,
                })
            }
            Some(raw::ConstExpr::Cast(inner)) => self.constant(inner),
            Some(raw::ConstExpr::Other(text)) => bail!("unsupported constant expression {}", text),
            None => bail!("unsupported constant {}", v.print_to_string()),
        }
    }

    /// Byte offset of a constant GEP: the first index scales the source type, the
    /// others step into structs and arrays.
    fn gep_offset(&self, source_ty: BasicTypeEnum<'ctx>, indices: &[i64]) -> Result<i64> {
        let Some((&first, rest)) = indices.split_first() else {
            return Ok(0);
        };
        let mut off = first * self.td.get_abi_size(&source_ty) as i64;
        let mut ty = source_ty;
        for &idx in rest {
            match ty {
                BasicTypeEnum::StructType(s) => {
                    off += self
                        .td
                        .offset_of_element(&s, idx as u32)
                        .ok_or_else(|| anyhow!("bad struct index {} in constant GEP", idx))?
                        as i64;
                    ty = s
                        .get_field_type_at_index(idx as u32)
                        .ok_or_else(|| anyhow!("bad struct index {} in constant GEP", idx))?;
                }
                BasicTypeEnum::ArrayType(a) => {
                    ty = a.get_element_type();
                    off += idx * self.td.get_abi_size(&ty) as i64;
                }
                _ => bail!("constant GEP indexes into a non-aggregate type"),
            }
        }
        Ok(off)
    }

    // -----------------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------------

    fn function(&mut self, f: FunctionValue<'ctx>) -> Result<Function> {
        let name = f.get_name().to_string_lossy().into_owned();
        let fty = f.get_type();
        let ty = self.intern(fty.as_any_type_enum())?;
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
        if f.count_basic_blocks() == 0 {
            // A declaration: no body, and its parameters may have types the backend
            // cannot represent (intrinsics taking metadata).
            return Ok(Function {
                name,
                ty,
                params,
                ret,
                variadic,
                values,
                blocks: Vec::new(),
            });
        }
        for p in f.get_params() {
            let v = Value(values.len() as u32);
            values.push(ValueInfo {
                ty: self.intern_basic(p.get_type())?,
                name: value_name(&p),
            });
            fx.values.insert(vkey(&p), v);
            params.push(v);
        }
        // Number blocks and instruction results before converting anything, since
        // operands may refer to values defined later in the function.
        let llvm_blocks = f.get_basic_blocks();
        for (bi, bb) in llvm_blocks.iter().enumerate() {
            fx.blocks.insert(bkey(bb), bi);
            for inst in bb.get_instructions() {
                let ity = inst.get_type();
                if !matches!(ity, AnyTypeEnum::VoidType(_)) {
                    let v = Value(values.len() as u32);
                    let iname = inst
                        .get_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    values.push(ValueInfo {
                        ty: self.intern(ity)?,
                        name: iname,
                    });
                    fx.values.insert(vkey(&inst), v);
                }
            }
        }
        let mut blocks = Vec::with_capacity(llvm_blocks.len());
        for bb in &llvm_blocks {
            blocks.push(self.block(*bb, &fx)?);
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

    fn block(&mut self, bb: BasicBlock<'ctx>, fx: &FnCtx) -> Result<Block> {
        let name = bb.get_name().to_string_lossy().into_owned();
        let mut phis = Vec::new();
        let mut instrs = Vec::new();
        let mut term = None;
        for inst in bb.get_instructions() {
            let text = inst.print_to_string().to_string().trim().to_string();
            let opcode = inst.get_opcode();
            let dest = fx.values.get(&vkey(&inst)).copied();
            let ty = self.intern(inst.get_type())?;
            match opcode {
                Opc::Phi => {
                    let dest = dest.ok_or_else(|| anyhow!("phi without result"))?;
                    let phi = PhiValue::try_from(inst).map_err(|_| anyhow!("not a phi"))?;
                    let mut incoming = Vec::new();
                    for (val, blk) in phi.get_incomings() {
                        let val = self.operand(val, fx).with_context(|| text.clone())?;
                        incoming.push((val, block_id(&blk, fx)?));
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

    fn operand(&mut self, v: BasicValueEnum<'ctx>, fx: &FnCtx) -> Result<Operand> {
        let ty = self.intern_basic(v.get_type())?;
        if let Some(&val) = fx.values.get(&vkey(&v)) {
            return Ok(Operand::value(ty, val));
        }
        if v.is_const() {
            return Ok(Operand::constant(ty, self.constant(v)?));
        }
        bail!("unsupported operand {}", v.print_to_string())
    }

    /// Operand `i` of an instruction as a value operand.
    fn op(&mut self, inst: InstructionValue<'ctx>, i: u32, fx: &FnCtx) -> Result<Operand> {
        match inst.get_operand(i) {
            Some(LlvmOperand::Value(v)) => self.operand(v, fx),
            Some(LlvmOperand::Block(_)) => bail!("operand {} is a block", i),
            None => bail!("missing operand {}", i),
        }
    }

    /// Operand `i` of an instruction as a block.
    fn block_op(&self, inst: InstructionValue<'ctx>, i: u32, fx: &FnCtx) -> Result<BlockId> {
        match inst.get_operand(i) {
            Some(LlvmOperand::Block(b)) => block_id(&b, fx),
            _ => bail!("operand {} is not a block", i),
        }
    }

    fn terminator(
        &mut self,
        inst: InstructionValue<'ctx>,
        opcode: Opc,
        fx: &FnCtx,
    ) -> Result<Terminator> {
        Ok(match opcode {
            Opc::Return => {
                if inst.get_num_operands() == 0 {
                    Terminator::Ret(None)
                } else {
                    Terminator::Ret(Some(self.op(inst, 0, fx)?))
                }
            }
            Opc::UncondBr | Opc::CondBr => {
                if inst.is_conditional().unwrap_or(false) {
                    // Operands of a conditional branch are [condition, true dest, false dest],
                    // the same order as in the textual IR.
                    let cond = self.op(inst, 0, fx)?;
                    let if_true = self.block_op(inst, 1, fx)?;
                    let if_false = self.block_op(inst, 2, fx)?;
                    Terminator::CondBr {
                        cond,
                        if_true,
                        if_false,
                    }
                } else {
                    Terminator::Br(self.block_op(inst, 0, fx)?)
                }
            }
            Opc::Switch => {
                // Operands are [condition, default, dest_1, ..., dest_n]; the case values
                // live out of line (see raw::switch_case_values).
                let value = self.op(inst, 0, fx)?;
                let default = self.block_op(inst, 1, fx)?;
                let case_values = raw::switch_case_values(inst);
                let mut cases = Vec::with_capacity(case_values.len());
                for (k, cv) in case_values.into_iter().enumerate() {
                    cases.push((cv, self.block_op(inst, 2 + k as u32, fx)?));
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

    fn instruction(&mut self, inst: InstructionValue<'ctx>, opcode: Opc, fx: &FnCtx) -> Result<Op> {
        let binary = |this: &mut Self, kind: BinOp| -> Result<Op> {
            Ok(Op::Binary {
                kind,
                a: this.op(inst, 0, fx)?,
                b: this.op(inst, 1, fx)?,
            })
        };
        let cast = |this: &mut Self, kind: CastKind| -> Result<Op> {
            Ok(Op::Cast {
                kind,
                x: this.op(inst, 0, fx)?,
            })
        };
        let align = |inst: InstructionValue<'ctx>| inst.get_alignment().unwrap_or(0) as usize;
        Ok(match opcode {
            Opc::Add => binary(self, BinOp::Add)?,
            Opc::Sub => binary(self, BinOp::Sub)?,
            Opc::Mul => binary(self, BinOp::Mul)?,
            Opc::UDiv => binary(self, BinOp::UDiv)?,
            Opc::SDiv => binary(self, BinOp::SDiv)?,
            Opc::URem => binary(self, BinOp::URem)?,
            Opc::SRem => binary(self, BinOp::SRem)?,
            Opc::And => binary(self, BinOp::And)?,
            Opc::Or => binary(self, BinOp::Or)?,
            Opc::Xor => binary(self, BinOp::Xor)?,
            Opc::Shl => binary(self, BinOp::Shl)?,
            Opc::LShr => binary(self, BinOp::LShr)?,
            Opc::AShr => binary(self, BinOp::AShr)?,
            Opc::ICmp => {
                let pred = match inst
                    .get_icmp_predicate()
                    .ok_or_else(|| anyhow!("icmp without predicate"))?
                {
                    IntPredicate::EQ => Pred::Eq,
                    IntPredicate::NE => Pred::Ne,
                    IntPredicate::ULT => Pred::Ult,
                    IntPredicate::ULE => Pred::Ule,
                    IntPredicate::UGT => Pred::Ugt,
                    IntPredicate::UGE => Pred::Uge,
                    IntPredicate::SLT => Pred::Slt,
                    IntPredicate::SLE => Pred::Sle,
                    IntPredicate::SGT => Pred::Sgt,
                    IntPredicate::SGE => Pred::Sge,
                };
                Op::ICmp {
                    pred,
                    a: self.op(inst, 0, fx)?,
                    b: self.op(inst, 1, fx)?,
                }
            }
            Opc::ZExt => cast(self, CastKind::ZExt)?,
            Opc::SExt => cast(self, CastKind::SExt)?,
            Opc::Trunc => cast(self, CastKind::Trunc)?,
            Opc::PtrToInt | Opc::PtrToAddr | Opc::IntToPtr | Opc::BitCast | Opc::AddrSpaceCast => {
                cast(self, CastKind::Reinterpret)?
            }
            Opc::Freeze => Op::Freeze {
                x: self.op(inst, 0, fx)?,
            },
            Opc::Load => Op::Load {
                addr: self.op(inst, 0, fx)?,
                align: align(inst),
            },
            Opc::Store => Op::Store {
                value: self.op(inst, 0, fx)?,
                addr: self.op(inst, 1, fx)?,
                align: align(inst),
            },
            Opc::GetElementPtr => {
                let base = self.op(inst, 0, fx)?;
                let source_ty = inst
                    .get_gep_source_element_type()
                    .map_err(|e| anyhow!("{:?}", e))?;
                let source_ty = self.intern_basic(source_ty)?;
                let mut indices = Vec::new();
                for i in 1..inst.get_num_operands() {
                    indices.push(self.op(inst, i, fx)?);
                }
                Op::Gep {
                    base,
                    source_ty,
                    indices,
                }
            }
            Opc::Alloca => {
                let ty = inst.get_allocated_type().map_err(|e| anyhow!("{:?}", e))?;
                let count = match inst.get_operand(0) {
                    Some(LlvmOperand::Value(BasicValueEnum::IntValue(c))) => {
                        c.get_zero_extended_constant()
                    }
                    _ => None,
                }
                .ok_or_else(|| anyhow!("alloca with a variable element count is not supported"))?;
                Op::Alloca {
                    ty: self.intern_basic(ty)?,
                    count,
                    align: align(inst),
                }
            }
            Opc::Select => Op::Select {
                cond: self.op(inst, 0, fx)?,
                if_true: self.op(inst, 1, fx)?,
                if_false: self.op(inst, 2, fx)?,
            },
            Opc::ExtractValue => Op::ExtractValue {
                agg: self.op(inst, 0, fx)?,
                indices: inst.get_indices(),
            },
            Opc::InsertValue => Op::InsertValue {
                agg: self.op(inst, 0, fx)?,
                elem: self.op(inst, 1, fx)?,
                indices: inst.get_indices(),
            },
            Opc::VAArg => Op::VaArg {
                list: self.op(inst, 0, fx)?,
            },
            Opc::Call => self.call(inst, fx)?,
            _ => bail!("unsupported instruction"),
        })
    }

    fn call(&mut self, inst: InstructionValue<'ctx>, fx: &FnCtx) -> Result<Op> {
        let cs = CallSiteValue::try_from(inst).map_err(|_| anyhow!("not a call"))?;
        let nargs = cs.count_arguments();
        let (callee, fn_ty) = match cs.get_called_fn_value() {
            Some(f) => {
                let name = f.get_name().to_string_lossy().into_owned();
                let callee = if name.starts_with("llvm.") || name.starts_with("__llvbf_") {
                    Callee::Intrinsic(name)
                } else {
                    Callee::Direct(
                        *self
                            .func_ids
                            .get(&vkey(&f))
                            .ok_or_else(|| anyhow!("unknown function @{}", name))?,
                    )
                };
                (callee, self.intern(f.get_type().as_any_type_enum())?)
            }
            None => {
                // The callee is the last operand of a call instruction.
                let callee = self.op(inst, nargs, fx)?;
                let fn_ty = self.intern(raw::called_function_type(cs).as_any_type_enum())?;
                (Callee::Indirect(callee), fn_ty)
            }
        };
        let intrinsic = matches!(callee, Callee::Intrinsic(_));
        let mut args = Vec::with_capacity(nargs as usize);
        for i in 0..nargs {
            // Metadata arguments only occur on intrinsics the backend ignores.
            if intrinsic && raw::operand_is_metadata(inst, i) {
                continue;
            }
            args.push(self.op(inst, i, fx)?);
        }
        Ok(Op::Call {
            callee,
            fn_ty,
            args,
        })
    }
}

fn block_id(b: &BasicBlock<'_>, fx: &FnCtx) -> Result<BlockId> {
    fx.blocks
        .get(&bkey(b))
        .copied()
        .ok_or_else(|| anyhow!("reference to a block of another function"))
}

fn value_name(v: &BasicValueEnum<'_>) -> String {
    match v {
        BasicValueEnum::IntValue(x) => x.get_name().to_string_lossy().into_owned(),
        BasicValueEnum::PointerValue(x) => x.get_name().to_string_lossy().into_owned(),
        BasicValueEnum::ArrayValue(x) => x.get_name().to_string_lossy().into_owned(),
        BasicValueEnum::StructValue(x) => x.get_name().to_string_lossy().into_owned(),
        _ => String::new(),
    }
}
