//! Module analysis: assigns dispatch block ids, splits basic blocks into
//! segments at calls and memory accesses, assigns frame slots to SSA values and
//! lays out global variables in memory.

use crate::assist::GepTable;
use crate::types::TypeInfo;
use anyhow::{anyhow, bail, Context, Result};
use either::Either;
use llvm_ir::constant::Constant;
use llvm_ir::instruction::Instruction;
use llvm_ir::{Function, Module, Name, Operand, Type, TypeRef};
use std::collections::HashMap;

/// What a memory access is for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AccessRole {
    /// A plain `load` or `store` instruction.
    Plain,
    /// `va_arg`: load the current argument pointer from the va_list.
    VaPtrLoad,
    /// `va_arg`: load the argument value through that pointer.
    VaValue,
    /// `va_arg`: store the advanced argument pointer back into the va_list.
    VaPtrStore,
    /// `llvm.va_start`: store the hidden variadic-area pointer into the va_list.
    VaStart,
    /// Variadic call: spill variadic argument `k` into the memory stack.
    VaSpill(usize),
    /// `llvm.va_copy`: load the source va_list.
    VaCopyLoad,
    /// `llvm.va_copy`: store into the destination va_list.
    VaCopyStore,
}

/// A memory access of `width` bytes at byte offset `off` of the value of instruction `instr`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Access {
    pub instr: usize,
    pub is_store: bool,
    pub off: usize,
    pub width: usize,
    pub role: AccessRole,
}

/// An inline step of a segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// Execute an instruction inline.
    Instr(usize),
    /// Reserve the variadic spill area for the call instruction (MSP -= total).
    VaSetup(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SegEnd {
    /// The segment ends with the block terminator.
    Term,
    /// The segment ends with a (non-inline) call instruction.
    Call(usize),
    /// The segment ends with a memory access request.
    Access(Access),
}

#[derive(Clone, Debug)]
pub struct Segment {
    pub id: u16,
    /// Steps executed inline in this segment (in order).
    pub steps: Vec<Step>,
    pub end: SegEnd,
    /// What the previous segment of the same block ended with (to finish it).
    pub prev_end: Option<SegEnd>,
}

#[derive(Clone, Debug)]
pub struct BlockPlan {
    pub segments: Vec<Segment>,
}

#[derive(Clone, Copy, Debug)]
pub enum SlotKind {
    Param,
    Local,
}

#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub kind: SlotKind,
    pub off: usize,
    pub size: usize,
}

pub struct FuncPlan {
    pub name: String,
    pub fidx: usize,
    pub entry_id: u16,
    pub blocks: Vec<BlockPlan>,
    pub block_ids: HashMap<Name, u16>,
    pub slots: HashMap<Name, Slot>,
    /// Hidden slots for instructions that need temporary storage across segments,
    /// keyed by (block index, instruction index).
    pub extra_slots: HashMap<(usize, usize), Slot>,
    /// Slot of the hidden variadic-area pointer parameter (variadic functions only).
    pub va_slot: Option<Slot>,
    pub locals_size: usize,
    pub params_size: usize,
    pub ret_size: usize,
}

pub struct Plan {
    pub funcs: Vec<FuncPlan>,
    pub func_by_name: HashMap<String, usize>,
    pub globals: HashMap<String, u64>,
    pub global_bytes: Vec<(u64, u8)>,
    pub heap_start: u64,
    pub ret_max: usize,
    pub params_max: usize,
    pub locals_max: usize,
    pub num_ids: usize,
    pub start_id: u16,
    pub exit_id: u16,
    /// Distinct (is_store, width) access kinds used by the program.
    pub access_kinds: Vec<(bool, usize)>,
    pub mem_size: usize,
}

/// Names of compiler builtins that are lowered inline (not real calls).
pub fn is_builtin(name: &str) -> bool {
    matches!(name, "__llvbf_putc" | "__llvbf_getc" | "__llvbf_exit")
}

/// Intrinsics that are lowered inline without ending the segment.
pub fn is_inline_intrinsic(name: &str) -> bool {
    name.starts_with("llvm.lifetime.")
        || name.starts_with("llvm.dbg.")
        || name.starts_with("llvm.assume")
        || name.starts_with("llvm.experimental.noalias")
        || name.starts_with("llvm.expect.")
        || name.starts_with("llvm.abs.")
        || name.starts_with("llvm.smax.")
        || name.starts_with("llvm.smin.")
        || name.starts_with("llvm.umax.")
        || name.starts_with("llvm.umin.")
        || name.starts_with("llvm.bswap.")
        || name.starts_with("llvm.uadd.with.overflow.")
        || name.starts_with("llvm.usub.with.overflow.")
        || name.starts_with("llvm.sadd.with.overflow.")
        || name.starts_with("llvm.ssub.with.overflow.")
        || name.starts_with("llvm.umul.with.overflow.")
        || name.starts_with("llvm.va_end")
        || name.starts_with("llvm.fshl.")
        || name.starts_with("llvm.fshr.")
        || name.starts_with("llvm.ctpop.")
        || name.starts_with("llvm.ctlz.")
        || name.starts_with("llvm.cttz.")
        || name.starts_with("llvm.va_start")
        || name.starts_with("llvm.va_copy")
}

/// Intrinsics that are rewritten into calls of runtime functions.
pub fn intrinsic_runtime_call(name: &str) -> Option<&'static str> {
    if name.starts_with("llvm.memcpy.") {
        Some("memcpy")
    } else if name.starts_with("llvm.memmove.") {
        Some("memmove")
    } else if name.starts_with("llvm.memset.") {
        Some("memset")
    } else {
        None
    }
}

/// Name of the callee of a call instruction, if it is a direct call.
pub fn callee_name(call: &llvm_ir::instruction::Call) -> Option<String> {
    match &call.function {
        Either::Right(Operand::ConstantOperand(c)) => match c.as_ref() {
            Constant::GlobalReference { name, .. } => Some(name_str(name)),
            _ => None,
        },
        _ => None,
    }
}

pub fn name_str(n: &Name) -> String {
    match n {
        Name::Name(s) => s.to_string(),
        Name::Number(i) => format!("{}", i),
    }
}

/// Split the byte range of a memory access into aligned chunks of 1, 2, 4 or 8 bytes.
pub fn chunks(size: usize, align: usize) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut pos = 0;
    let align = align.max(1);
    while pos < size {
        let mut w = 8;
        while w > 1 && (w > size - pos || w > align || pos % w != 0) {
            w /= 2;
        }
        v.push((pos, w));
        pos += w;
    }
    v
}

/// Classify a call: Some(true) if it ends the segment (real call), Some(false) if inline.
fn call_ends_segment(call: &llvm_ir::instruction::Call) -> Result<bool> {
    if let Either::Left(_) = &call.function {
        bail!("inline assembly is not supported");
    }
    match callee_name(call) {
        Some(n) if is_builtin(&n) || is_inline_intrinsic(&n) => Ok(false),
        Some(n) if intrinsic_runtime_call(&n).is_some() => Ok(true),
        Some(n) if n.starts_with("llvm.") => bail!("unsupported intrinsic {}", n),
        _ => Ok(true),
    }
}

fn access_align(ti: &TypeInfo, ty: &TypeRef, alignment: u32) -> Result<usize> {
    if alignment == 0 {
        ti.align_of(ty)
    } else {
        Ok(alignment as usize)
    }
}

/// Size of a variadic argument slot in the spill area (rounded up to 2 bytes).
pub fn va_slot_size(size: usize) -> usize {
    (size + 1) / 2 * 2
}

/// Byte offsets and sizes of the variadic arguments of a call, and the total area size.
pub fn va_spill_layout(
    module: &Module,
    ti: &TypeInfo,
    call: &llvm_ir::instruction::Call,
) -> Result<(Vec<(usize, usize)>, usize)> {
    let fixed = match call.function_ty.as_ref() {
        Type::FuncType { param_types, .. } => param_types.len(),
        _ => bail!("call through non-function type"),
    };
    let mut v = Vec::new();
    let mut off = 0;
    for (a, _) in call.arguments.iter().skip(fixed) {
        let size = ti.size_of(&module.type_of(a))?;
        v.push((off, size));
        off += va_slot_size(size);
    }
    Ok((v, off))
}

pub fn call_is_variadic(call: &llvm_ir::instruction::Call) -> bool {
    matches!(
        call.function_ty.as_ref(),
        Type::FuncType {
            is_var_arg: true,
            ..
        }
    )
}

fn plan_function(
    module: &Module,
    ti: &TypeInfo,
    f: &Function,
    fidx: usize,
    next_id: &mut u16,
    access_kinds: &mut Vec<(bool, usize)>,
) -> Result<FuncPlan> {
    let mut slots: HashMap<Name, Slot> = HashMap::new();
    // Parameters.
    let mut poff = 0usize;
    for p in &f.parameters {
        let size = ti
            .size_of(&p.ty)
            .with_context(|| format!("parameter {} of {}", p.name, f.name))?;
        slots.insert(
            p.name.clone(),
            Slot {
                kind: SlotKind::Param,
                off: poff,
                size,
            },
        );
        poff += size;
    }
    let va_slot = if f.is_var_arg {
        let s = Slot {
            kind: SlotKind::Param,
            off: poff,
            size: ti.ptr_bytes,
        };
        poff += ti.ptr_bytes;
        Some(s)
    } else {
        None
    };
    let params_size = poff;
    let ret_size = match f.return_type.as_ref() {
        Type::VoidType => 0,
        _ => ti
            .size_of(&f.return_type)
            .with_context(|| format!("return type of {}", f.name))?,
    };
    // Locals.
    let mut loff = 0usize;
    let mut extra_slots = HashMap::new();
    for (bi, bb) in f.basic_blocks.iter().enumerate() {
        for (ii, inst) in bb.instrs.iter().enumerate() {
            if let Some(dest) = inst_dest(inst) {
                let ty = module.type_of(inst);
                if matches!(ty.as_ref(), Type::VoidType) {
                    continue;
                }
                let size = ti
                    .size_of(&ty)
                    .with_context(|| format!("value %{} in {}", dest, f.name))?;
                slots.insert(
                    dest.clone(),
                    Slot {
                        kind: SlotKind::Local,
                        off: loff,
                        size,
                    },
                );
                loff += size;
            }
            let needs_extra = match inst {
                Instruction::VAArg(_) => true,
                Instruction::Call(c) => {
                    matches!(callee_name(c).as_deref(), Some(n) if n.starts_with("llvm.va_copy"))
                }
                _ => false,
            };
            if needs_extra {
                extra_slots.insert(
                    (bi, ii),
                    Slot {
                        kind: SlotKind::Local,
                        off: loff,
                        size: ti.ptr_bytes,
                    },
                );
                loff += ti.ptr_bytes;
            }
        }
    }
    // Blocks and segments.
    let mut block_ids = HashMap::new();
    let mut blocks = Vec::new();
    let entry_id = *next_id;
    let mut note_kind = |is_store: bool, width: usize| {
        if !access_kinds.contains(&(is_store, width)) {
            access_kinds.push((is_store, width));
        }
    };
    for bb in &f.basic_blocks {
        block_ids.insert(bb.name.clone(), *next_id);
        let mut segments: Vec<Segment> = Vec::new();
        let mut cur: Vec<Step> = Vec::new();
        let mut prev_end: Option<SegEnd> = None;
        let finish = |segments: &mut Vec<Segment>,
                      cur: &mut Vec<Step>,
                      end: SegEnd,
                      prev_end: &mut Option<SegEnd>,
                      next_id: &mut u16| {
            segments.push(Segment {
                id: *next_id,
                steps: std::mem::take(cur),
                end: end.clone(),
                prev_end: prev_end.take(),
            });
            *next_id = next_id.checked_add(1).expect("too many blocks");
            *prev_end = Some(end);
        };
        for (i, inst) in bb.instrs.iter().enumerate() {
            match inst {
                Instruction::Load(l) => {
                    let size = ti.size_of(&l.loaded_ty)?;
                    let align = access_align(ti, &l.loaded_ty, l.alignment)?;
                    for (off, width) in chunks(size, align) {
                        note_kind(false, width);
                        finish(
                            &mut segments,
                            &mut cur,
                            SegEnd::Access(Access {
                                instr: i,
                                is_store: false,
                                off,
                                width,
                                role: AccessRole::Plain,
                            }),
                            &mut prev_end,
                            next_id,
                        );
                    }
                }
                Instruction::Store(s) => {
                    let ty = module.type_of(&s.value);
                    let size = ti.size_of(&ty)?;
                    let align = access_align(ti, &ty, s.alignment)?;
                    for (off, width) in chunks(size, align) {
                        note_kind(true, width);
                        finish(
                            &mut segments,
                            &mut cur,
                            SegEnd::Access(Access {
                                instr: i,
                                is_store: true,
                                off,
                                width,
                                role: AccessRole::Plain,
                            }),
                            &mut prev_end,
                            next_id,
                        );
                    }
                }
                Instruction::VAArg(v) => {
                    for (off, width) in chunks(ti.ptr_bytes, 2) {
                        note_kind(false, width);
                        finish(
                            &mut segments,
                            &mut cur,
                            SegEnd::Access(Access {
                                instr: i,
                                is_store: false,
                                off,
                                width,
                                role: AccessRole::VaPtrLoad,
                            }),
                            &mut prev_end,
                            next_id,
                        );
                    }
                    let size = ti.size_of(&v.cur_type)?;
                    for (off, width) in chunks(size, 2) {
                        note_kind(false, width);
                        finish(
                            &mut segments,
                            &mut cur,
                            SegEnd::Access(Access {
                                instr: i,
                                is_store: false,
                                off,
                                width,
                                role: AccessRole::VaValue,
                            }),
                            &mut prev_end,
                            next_id,
                        );
                    }
                    for (off, width) in chunks(ti.ptr_bytes, 2) {
                        note_kind(true, width);
                        finish(
                            &mut segments,
                            &mut cur,
                            SegEnd::Access(Access {
                                instr: i,
                                is_store: true,
                                off,
                                width,
                                role: AccessRole::VaPtrStore,
                            }),
                            &mut prev_end,
                            next_id,
                        );
                    }
                }
                Instruction::Call(c) => {
                    let name = callee_name(c);
                    if matches!(name.as_deref(), Some(n) if n.starts_with("llvm.va_start")) {
                        if !f.is_var_arg {
                            bail!("va_start in a non-variadic function");
                        }
                        for (off, width) in chunks(ti.ptr_bytes, 2) {
                            note_kind(true, width);
                            finish(
                                &mut segments,
                                &mut cur,
                                SegEnd::Access(Access {
                                    instr: i,
                                    is_store: true,
                                    off,
                                    width,
                                    role: AccessRole::VaStart,
                                }),
                                &mut prev_end,
                                next_id,
                            );
                        }
                    } else if matches!(name.as_deref(), Some(n) if n.starts_with("llvm.va_copy")) {
                        for (off, width) in chunks(ti.ptr_bytes, 2) {
                            note_kind(false, width);
                            finish(
                                &mut segments,
                                &mut cur,
                                SegEnd::Access(Access {
                                    instr: i,
                                    is_store: false,
                                    off,
                                    width,
                                    role: AccessRole::VaCopyLoad,
                                }),
                                &mut prev_end,
                                next_id,
                            );
                        }
                        for (off, width) in chunks(ti.ptr_bytes, 2) {
                            note_kind(true, width);
                            finish(
                                &mut segments,
                                &mut cur,
                                SegEnd::Access(Access {
                                    instr: i,
                                    is_store: true,
                                    off,
                                    width,
                                    role: AccessRole::VaCopyStore,
                                }),
                                &mut prev_end,
                                next_id,
                            );
                        }
                    } else if call_ends_segment(c)? {
                        if call_is_variadic(c) {
                            let (layout, _) = va_spill_layout(module, ti, c)?;
                            cur.push(Step::VaSetup(i));
                            for (k, (aoff, size)) in layout.iter().enumerate() {
                                for (off, width) in chunks(*size, 2) {
                                    note_kind(true, width);
                                    finish(
                                        &mut segments,
                                        &mut cur,
                                        SegEnd::Access(Access {
                                            instr: i,
                                            is_store: true,
                                            off: aoff + off,
                                            width,
                                            role: AccessRole::VaSpill(k),
                                        }),
                                        &mut prev_end,
                                        next_id,
                                    );
                                }
                            }
                        }
                        finish(
                            &mut segments,
                            &mut cur,
                            SegEnd::Call(i),
                            &mut prev_end,
                            next_id,
                        );
                    } else {
                        cur.push(Step::Instr(i));
                    }
                }
                _ => cur.push(Step::Instr(i)),
            }
        }
        finish(
            &mut segments,
            &mut cur,
            SegEnd::Term,
            &mut prev_end,
            next_id,
        );
        blocks.push(BlockPlan { segments });
    }
    Ok(FuncPlan {
        name: f.name.clone(),
        fidx,
        entry_id,
        blocks,
        block_ids,
        slots,
        extra_slots,
        va_slot,
        locals_size: loff,
        params_size,
        ret_size,
    })
}

pub fn inst_dest(inst: &Instruction) -> Option<&Name> {
    match inst {
        Instruction::Store(_) | Instruction::Fence(_) => None,
        Instruction::Call(c) => c.dest.as_ref(),
        other => Some(other.try_get_result()?),
    }
}

/// Evaluate a constant to a scalar value (addresses for globals, block ids for functions).
pub struct ConstEval<'a> {
    pub module: &'a Module,
    pub ti: &'a TypeInfo<'a>,
    pub geps: &'a GepTable,
    pub globals: &'a HashMap<String, u64>,
    pub func_ids: &'a HashMap<String, u16>,
}

pub fn mask_bits(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        v & ((1u64 << bits) - 1)
    }
}

fn sext_bits(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        v as i64
    } else {
        ((v << (64 - bits)) as i64) >> (64 - bits)
    }
}

impl<'a> ConstEval<'a> {
    pub fn bits_of(&self, ty: &TypeRef) -> Result<u32> {
        self.ti.scalar_bits(ty)
    }

    pub fn scalar(&self, c: &Constant) -> Result<u64> {
        Ok(match c {
            Constant::Int { bits, value } => mask_bits(*value, *bits),
            Constant::Null(_)
            | Constant::Undef(_)
            | Constant::Poison(_)
            | Constant::AggregateZero(_) => 0,
            Constant::GlobalReference { name, .. } => {
                let n = name_str(name);
                if let Some(id) = self.func_ids.get(&n) {
                    *id as u64
                } else if let Some(a) = self.globals.get(&n) {
                    *a
                } else {
                    bail!("undefined global @{}", n)
                }
            }
            Constant::GetElementPtr(g) => {
                let base_name = match g.address.as_ref() {
                    Constant::GlobalReference { name, .. } => name_str(name),
                    other => bail!("unsupported constant GEP base {}", other),
                };
                let base = self.scalar(g.address.as_ref())?;
                let mut idx = Vec::new();
                for i in &g.indices {
                    match i.as_ref() {
                        Constant::Int { bits, value } => idx.push(sext_bits(*value, *bits)),
                        other => bail!("non-constant GEP index {}", other),
                    }
                }
                let off = self.geps.lookup(&base_name, &idx).ok_or_else(|| {
                    anyhow!("constant GEP offset not found for @{} {:?}", base_name, idx)
                })?;
                mask_bits((base as i64 + off) as u64, 8 * self.ti.ptr_bytes as u32)
            }
            Constant::PtrToInt(x) => mask_bits(self.scalar(&x.operand)?, self.bits_of(&x.to_type)?),
            Constant::IntToPtr(x) => mask_bits(self.scalar(&x.operand)?, self.bits_of(&x.to_type)?),
            Constant::BitCast(x) => self.scalar(&x.operand)?,
            Constant::AddrSpaceCast(x) => self.scalar(&x.operand)?,
            Constant::Trunc(x) => mask_bits(self.scalar(&x.operand)?, self.bits_of(&x.to_type)?),
            Constant::Add(x) => {
                let bits = self.bits_of(&self.module.type_of(c))?;
                mask_bits(
                    self.scalar(&x.operand0)?
                        .wrapping_add(self.scalar(&x.operand1)?),
                    bits,
                )
            }
            Constant::Sub(x) => {
                let bits = self.bits_of(&self.module.type_of(c))?;
                mask_bits(
                    self.scalar(&x.operand0)?
                        .wrapping_sub(self.scalar(&x.operand1)?),
                    bits,
                )
            }
            Constant::Mul(x) => {
                let bits = self.bits_of(&self.module.type_of(c))?;
                mask_bits(
                    self.scalar(&x.operand0)?
                        .wrapping_mul(self.scalar(&x.operand1)?),
                    bits,
                )
            }
            Constant::Xor(x) => self.scalar(&x.operand0)? ^ self.scalar(&x.operand1)?,
            Constant::Shl(x) => {
                let bits = self.bits_of(&self.module.type_of(c))?;
                mask_bits(self.scalar(&x.operand0)? << self.scalar(&x.operand1)?, bits)
            }
            other => bail!("unsupported constant expression {}", other),
        })
    }

    /// Serialize a constant of type `ty` to its in-memory bytes.
    pub fn bytes(&self, c: &Constant, ty: &TypeRef) -> Result<Vec<u8>> {
        let size = self.ti.size_of(ty)?;
        let rty = self.ti.resolve(ty)?;
        let mut out = vec![0u8; size];
        match c {
            Constant::Null(_)
            | Constant::Undef(_)
            | Constant::Poison(_)
            | Constant::AggregateZero(_) => {}
            Constant::Struct { values, .. } => {
                let (offsets, _) = self.ti.struct_layout(ty)?;
                let elem_types = match rty {
                    Type::StructType { element_types, .. } => element_types,
                    _ => bail!("struct constant with non-struct type {}", ty),
                };
                for (i, v) in values.iter().enumerate() {
                    let b = self.bytes(v, &elem_types[i])?;
                    out[offsets[i]..offsets[i] + b.len()].copy_from_slice(&b);
                }
            }
            Constant::Array {
                element_type,
                elements,
            } => {
                let es = self.ti.size_of(element_type)?;
                for (i, v) in elements.iter().enumerate() {
                    let b = self.bytes(v, element_type)?;
                    out[i * es..i * es + b.len()].copy_from_slice(&b);
                }
            }
            Constant::Float(_) | Constant::Vector(_) => bail!("unsupported constant {}", c),
            _ => {
                let v = self.scalar(c)?;
                for (i, byte) in out.iter_mut().enumerate().take(8) {
                    *byte = ((v >> (8 * i)) & 0xff) as u8;
                }
            }
        }
        Ok(out)
    }
}

pub fn plan_module(
    module: &Module,
    ti: &TypeInfo,
    geps: &GepTable,
    mem_size: usize,
    entry: &str,
) -> Result<Plan> {
    let mut next_id: u16 = 1; // 0 is reserved (null function pointer / halt)
    let start_id = next_id;
    let exit_id = next_id + 1;
    next_id += 2;
    let mut funcs = Vec::new();
    let mut func_by_name = HashMap::new();
    let mut access_kinds = Vec::new();
    for (fidx, f) in module.functions.iter().enumerate() {
        if f.basic_blocks.is_empty() {
            continue;
        }
        let fp = plan_function(module, ti, f, fidx, &mut next_id, &mut access_kinds)
            .with_context(|| format!("in function {}", f.name))?;
        func_by_name.insert(f.name.clone(), funcs.len());
        funcs.push(fp);
    }
    if !func_by_name.contains_key(entry) {
        bail!("entry function '{}' not found (or has no body)", entry);
    }
    // Globals.
    let mut globals: HashMap<String, u64> = HashMap::new();
    let mut addr: u64 = 16;
    let mut sizes: Vec<(String, u64, TypeRef)> = Vec::new();
    for g in &module.global_vars {
        let n = name_str(&g.name);
        if n == "__llvbf_heap_start" {
            continue;
        }
        let init = match &g.initializer {
            Some(i) => i,
            None => bail!("external global @{} has no definition", n),
        };
        let ty = module.type_of(init.as_ref());
        let size = ti.size_of(&ty).with_context(|| format!("global @{}", n))?;
        let align = (g.alignment as u64).max(ti.align_of(&ty)? as u64).max(1);
        addr = (addr + align - 1) / align * align;
        globals.insert(n.clone(), addr);
        sizes.push((n, addr, ty));
        addr += size as u64;
    }
    let heap_start = (addr + 7) / 8 * 8;
    globals.insert("__llvbf_heap_start".to_string(), heap_start);
    if heap_start as usize > mem_size {
        bail!(
            "global data ({} bytes) does not fit into memory of {} bytes",
            heap_start,
            mem_size
        );
    }
    let func_ids: HashMap<String, u16> =
        funcs.iter().map(|f| (f.name.clone(), f.entry_id)).collect();
    let ce = ConstEval {
        module,
        ti,
        geps,
        globals: &globals,
        func_ids: &func_ids,
    };
    let mut global_bytes = Vec::new();
    for g in &module.global_vars {
        let n = name_str(&g.name);
        if n == "__llvbf_heap_start" {
            continue;
        }
        let (_, base, ty) = sizes.iter().find(|(m, _, _)| *m == n).unwrap();
        let bytes = ce
            .bytes(g.initializer.as_ref().unwrap(), ty)
            .with_context(|| format!("initializer of @{}", n))?;
        for (i, b) in bytes.iter().enumerate() {
            if *b != 0 {
                global_bytes.push((base + i as u64, *b));
            }
        }
    }
    let ret_max = funcs.iter().map(|f| f.ret_size).max().unwrap_or(0).max(2);
    let params_max = funcs.iter().map(|f| f.params_size).max().unwrap_or(0);
    let locals_max = funcs.iter().map(|f| f.locals_size).max().unwrap_or(0);
    Ok(Plan {
        funcs,
        func_by_name,
        globals,
        global_bytes,
        heap_start,
        ret_max,
        params_max,
        locals_max,
        num_ids: next_id as usize,
        start_id,
        exit_id,
        access_kinds,
        mem_size,
    })
}
