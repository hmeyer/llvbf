//! Module analysis: assigns dispatch block ids, splits basic blocks into
//! segments at calls and memory accesses, assigns frame slots to SSA values and
//! lays out global variables in memory.

use crate::arith::limbs;
use crate::ir::*;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};

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

/// The shared arithmetic routines: multiplication, unsigned and signed division
/// (each division routine yields both quotient and remainder).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArithKind {
    Mul,
    UDiv,
    SDiv,
}

impl ArithKind {
    pub fn of(op: BinOp) -> Option<ArithKind> {
        match op {
            BinOp::Mul => Some(ArithKind::Mul),
            BinOp::UDiv | BinOp::URem => Some(ArithKind::UDiv),
            BinOp::SDiv | BinOp::SRem => Some(ArithKind::SDiv),
            _ => None,
        }
    }
}

/// A multiplication or division of `n`-limb operands serviced by a shared routine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArithOp {
    pub instr: usize,
    pub op: BinOp,
    pub n: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SegEnd {
    /// The segment ends with the block terminator.
    Term,
    /// The segment ends with a (non-inline) call instruction.
    Call(usize),
    /// The segment ends with a memory access request.
    Access(Access),
    /// The segment ends with a request to a shared arithmetic routine.
    Arith(ArithOp),
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SlotKind {
    /// Parameter area (layout determined by the signature).
    Param,
    /// Shared pool for values that are defined and last used within one block;
    /// slots are reused once a value is dead.
    Pool,
    /// Dedicated slot for values that live across blocks (and for phis).
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
    pub fid: FuncId,
    pub entry_id: u16,
    pub blocks: Vec<BlockPlan>,
    /// Dispatch id of the first segment of each block.
    pub block_ids: Vec<u16>,
    pub slots: HashMap<Value, Slot>,
    /// Hidden slots for instructions that need temporary storage across segments,
    /// keyed by (block index, instruction index).
    pub extra_slots: HashMap<(usize, usize), Slot>,
    /// Slot of the hidden variadic-area pointer parameter (variadic functions only).
    pub va_slot: Option<Slot>,
    /// Pooled values: the number of times each appears as an operand of a
    /// non-terminator instruction of its block, and of the block's terminator
    /// (including the phi copies of all outgoing edges). Used to move a value on
    /// its last use instead of copying it.
    pub instr_uses: HashMap<Value, usize>,
    pub term_uses: HashMap<Value, usize>,
    pub locals_size: usize,
    pub pool_size: usize,
    pub params_size: usize,
    pub ret_size: usize,
}

pub struct Plan {
    pub funcs: Vec<FuncPlan>,
    /// Plan index of each defined function, by `FuncId`.
    pub func_plan: HashMap<FuncId, usize>,
    /// Memory address of each global, by `GlobalId`.
    pub global_addr: Vec<u64>,
    pub global_bytes: Vec<(u64, u8)>,
    pub heap_start: u64,
    pub ret_max: usize,
    pub params_max: usize,
    pub locals_max: usize,
    pub pool_max: usize,
    pub num_ids: usize,
    pub start_id: u16,
    pub exit_id: u16,
    /// Distinct (is_store, width) access kinds used by the program.
    pub access_kinds: Vec<(bool, usize)>,
    /// Distinct (kind, limbs) arithmetic routines used by the program.
    pub arith_kinds: Vec<(ArithKind, usize)>,
    /// Size of the operand/result area of arithmetic requests (two operands).
    pub arg_size: usize,
    pub mem_size: usize,
    /// Functions (by `FuncId`) that use the memory stack pointer themselves or
    /// through a callee: allocas, variadic calls, `va_arg`, indirect calls.
    /// Calls to other functions skip copying MSP into the callee frame.
    pub needs_msp: Vec<bool>,
}

impl Plan {
    pub fn func_by_name(&self, name: &str) -> Option<usize> {
        self.funcs.iter().position(|f| f.name == name)
    }
    /// Entry segment id of a function (its "address"), if it is defined.
    pub fn entry_of(&self, fid: FuncId) -> Option<u16> {
        self.func_plan.get(&fid).map(|&i| self.funcs[i].entry_id)
    }
}

/// Names of compiler builtins that are lowered inline (not real calls).
pub fn is_builtin(name: &str) -> bool {
    matches!(name, "__llvbf_putc" | "__llvbf_getc" | "__llvbf_exit")
}

/// Intrinsics that are lowered inline without ending the segment.
pub fn is_inline_intrinsic(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "llvm.lifetime.",
        "llvm.dbg.",
        "llvm.assume",
        "llvm.experimental.noalias",
        "llvm.expect.",
        "llvm.abs.",
        "llvm.smax.",
        "llvm.smin.",
        "llvm.umax.",
        "llvm.umin.",
        "llvm.bswap.",
        "llvm.uadd.with.overflow.",
        "llvm.usub.with.overflow.",
        "llvm.sadd.with.overflow.",
        "llvm.ssub.with.overflow.",
        "llvm.umul.with.overflow.",
        "llvm.va_end",
        "llvm.fshl.",
        "llvm.fshr.",
        "llvm.ctpop.",
        "llvm.ctlz.",
        "llvm.cttz.",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
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

/// Name of an intrinsic or builtin callee.
pub fn intrinsic_name(callee: &Callee) -> Option<&str> {
    match callee {
        Callee::Intrinsic(n) => Some(n),
        _ => None,
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

/// Does a call end its segment (a real call) or is it lowered inline?
fn call_ends_segment(callee: &Callee) -> Result<bool> {
    match callee {
        Callee::Intrinsic(n) if is_builtin(n) || is_inline_intrinsic(n) => Ok(false),
        Callee::Intrinsic(n) if intrinsic_runtime_call(n).is_some() => Ok(true),
        Callee::Intrinsic(n) => bail!("unsupported intrinsic {}", n),
        _ => Ok(true),
    }
}

fn round_up(x: usize, a: usize) -> usize {
    x.div_ceil(a) * a
}

/// Size and alignment of a variadic argument's slot in the spill area: slots are
/// pointer-sized, and arguments with a larger alignment are aligned to at most two
/// pointers. This is what clang's inline `va_arg` expansion assumes on 32-bit
/// targets (4-byte slots, 8-byte alignment for 64-bit values); on the 16-bit target
/// llvbf lowers `va_arg` itself with the same rule.
pub fn va_slot(types: &TypeTable, ty: TypeId) -> (usize, usize) {
    let ptr = types.ptr_bytes;
    let size = round_up(types.size(ty), ptr);
    let align = types.align(ty).max(ptr).min(2 * ptr);
    (size, align)
}

pub fn call_is_variadic(types: &TypeTable, fn_ty: TypeId) -> bool {
    matches!(types.kind(fn_ty), TypeKind::Func { variadic: true, .. })
}

/// Byte offsets and sizes of the variadic arguments of a call, and the total area
/// size (a multiple of two pointers, so the memory stack pointer stays aligned).
pub fn va_spill_layout(
    types: &TypeTable,
    fn_ty: TypeId,
    args: &[Operand],
) -> Result<(Vec<(usize, usize)>, usize)> {
    let (params, _) = types.func_sig(fn_ty)?;
    let mut v = Vec::new();
    let mut off = 0;
    for a in args.iter().skip(params.len()) {
        let (slot_size, slot_align) = va_slot(types, a.ty);
        off = round_up(off, slot_align);
        v.push((off, types.size(a.ty)));
        off += slot_size;
    }
    Ok((v, round_up(off, 2 * types.ptr_bytes)))
}

fn access_align(types: &TypeTable, ty: TypeId, align: usize) -> usize {
    if align == 0 {
        types.align(ty)
    } else {
        align
    }
}

fn plan_function(
    m: &Module,
    f: &Function,
    fid: FuncId,
    next_id: &mut u16,
    access_kinds: &mut Vec<(bool, usize)>,
    arith_kinds: &mut Vec<(ArithKind, usize)>,
) -> Result<FuncPlan> {
    let types = &m.types;
    let mut slots: HashMap<Value, Slot> = HashMap::new();
    // Parameters.
    let mut poff = 0usize;
    for &p in &f.params {
        let size = types.size(f.value_ty(p));
        slots.insert(
            p,
            Slot {
                kind: SlotKind::Param,
                off: poff,
                size,
            },
        );
        poff += size;
    }
    let va_slot = if f.variadic {
        let s = Slot {
            kind: SlotKind::Param,
            off: poff,
            size: types.ptr_bytes,
        };
        poff += types.ptr_bytes;
        Some(s)
    } else {
        None
    };
    let params_size = poff;
    let ret_size = types.size(f.ret);
    // Where is each value used? A phi's incoming value counts as a use at the end of
    // the predecessor block. Values whose uses all lie in their defining block are
    // pooled; phis and cross-block values get dedicated slots.
    let term_pos = |b: &Block| b.instrs.len();
    let mut uses: HashMap<Value, Vec<(BlockId, usize)>> = HashMap::new();
    let mut instr_uses: HashMap<Value, usize> = HashMap::new();
    let mut term_uses: HashMap<Value, usize> = HashMap::new();
    let mut def_block: HashMap<Value, BlockId> = HashMap::new();
    for (bi, bb) in f.blocks.iter().enumerate() {
        for phi in &bb.phis {
            def_block.insert(phi.dest, usize::MAX); // never pooled
            for (val, pred) in &phi.incoming {
                if let Some(v) = val.as_value() {
                    uses.entry(v)
                        .or_default()
                        .push((*pred, term_pos(&f.blocks[*pred])));
                    *term_uses.entry(v).or_default() += 1;
                }
            }
        }
        for (ii, inst) in bb.instrs.iter().enumerate() {
            if let Some(d) = inst.dest {
                def_block.insert(d, bi);
            }
            for v in op_values(&inst.op) {
                uses.entry(v).or_default().push((bi, ii));
                *instr_uses.entry(v).or_default() += 1;
            }
        }
        for v in term_values(&bb.term) {
            uses.entry(v).or_default().push((bi, term_pos(bb)));
            *term_uses.entry(v).or_default() += 1;
        }
    }
    let is_local = |v: Value| -> bool {
        match def_block.get(&v) {
            Some(&bi) if bi != usize::MAX => uses
                .get(&v)
                .map(|u| u.iter().all(|(b, _)| *b == bi))
                .unwrap_or(true),
            _ => false,
        }
    };
    // Slots. Block-local values are interval-allocated within a per-function pool
    // (close to scratch). Phis and cross-block values are interval-allocated in the
    // dedicated area using a liveness analysis over the CFG, so values that are never
    // live at the same time share a slot.
    let mut pool_size = 0usize;
    let mut extra_slots = HashMap::new();
    for (bi, bb) in f.blocks.iter().enumerate() {
        let mut pool = IntervalAllocator::default();
        for (ii, inst) in bb.instrs.iter().enumerate() {
            pool.release_dead(ii);
            if let Some(dest) = inst.dest {
                if is_local(dest) {
                    let size = types.size(inst.ty);
                    let end = uses
                        .get(&dest)
                        .and_then(|u| u.iter().map(|(_, p)| *p).max())
                        .unwrap_or(ii);
                    let off = pool.alloc(size, end);
                    slots.insert(
                        dest,
                        Slot {
                            kind: SlotKind::Pool,
                            off,
                            size,
                        },
                    );
                }
            }
            let needs_extra = match &inst.op {
                Op::VaArg { .. } => true,
                Op::Call { callee, .. } => {
                    matches!(intrinsic_name(callee), Some(n) if n.starts_with("llvm.va_copy"))
                }
                _ => false,
            };
            if needs_extra {
                let off = pool.alloc(types.ptr_bytes, ii);
                extra_slots.insert(
                    (bi, ii),
                    Slot {
                        kind: SlotKind::Pool,
                        off,
                        size: types.ptr_bytes,
                    },
                );
            }
        }
        pool_size = pool_size.max(pool.top);
    }
    let loff = allocate_cross_block(f, types, &uses, &is_local, &mut slots);
    let instr_uses: HashMap<Value, usize> = instr_uses
        .into_iter()
        .filter(|(v, _)| is_local(*v))
        .collect();
    let term_uses: HashMap<Value, usize> = term_uses
        .into_iter()
        .filter(|(v, _)| is_local(*v))
        .collect();
    // Blocks and segments.
    let mut block_ids = Vec::new();
    let mut blocks = Vec::new();
    let entry_id = *next_id;
    for bb in &f.blocks {
        block_ids.push(*next_id);
        let mut sb = SegBuilder {
            segments: Vec::new(),
            cur: Vec::new(),
            prev_end: None,
            next_id,
            access_kinds,
            arith_kinds,
        };
        for (i, inst) in bb.instrs.iter().enumerate() {
            match &inst.op {
                Op::Load { align, .. } => {
                    let a = access_align(types, inst.ty, *align);
                    sb.access(i, false, types.size(inst.ty), a, 0, AccessRole::Plain);
                }
                Op::Store { value, align, .. } => {
                    let a = access_align(types, value.ty, *align);
                    sb.access(i, true, types.size(value.ty), a, 0, AccessRole::Plain);
                }
                Op::Binary { kind, a, b } if routed_arith(types, *kind, a, b) => {
                    sb.arith(i, *kind, limbs(types.bits(a.ty)?));
                }
                Op::VaArg { .. } => {
                    sb.access(i, false, types.ptr_bytes, 2, 0, AccessRole::VaPtrLoad);
                    sb.access(i, false, types.size(inst.ty), 2, 0, AccessRole::VaValue);
                    sb.access(i, true, types.ptr_bytes, 2, 0, AccessRole::VaPtrStore);
                }
                Op::Call {
                    callee,
                    fn_ty,
                    args,
                } => match intrinsic_name(callee) {
                    Some(n) if n.starts_with("llvm.va_start") => {
                        if !f.variadic {
                            bail!("va_start in a non-variadic function");
                        }
                        sb.access(i, true, types.ptr_bytes, 2, 0, AccessRole::VaStart);
                    }
                    Some(n) if n.starts_with("llvm.va_copy") => {
                        sb.access(i, false, types.ptr_bytes, 2, 0, AccessRole::VaCopyLoad);
                        sb.access(i, true, types.ptr_bytes, 2, 0, AccessRole::VaCopyStore);
                    }
                    _ if call_ends_segment(callee)? => {
                        if call_is_variadic(types, *fn_ty) {
                            let (layout, _) = va_spill_layout(types, *fn_ty, args)?;
                            sb.cur.push(Step::VaSetup(i));
                            for (k, (aoff, size)) in layout.iter().enumerate() {
                                sb.access(i, true, *size, 2, *aoff, AccessRole::VaSpill(k));
                            }
                        }
                        sb.finish(SegEnd::Call(i));
                    }
                    _ => sb.cur.push(Step::Instr(i)),
                },
                _ => sb.cur.push(Step::Instr(i)),
            }
        }
        sb.finish(SegEnd::Term);
        blocks.push(BlockPlan {
            segments: sb.segments,
        });
    }
    Ok(FuncPlan {
        name: f.name.clone(),
        fid,
        entry_id,
        blocks,
        block_ids,
        slots,
        extra_slots,
        va_slot,
        instr_uses,
        term_uses,
        locals_size: loff,
        pool_size,
        params_size,
        ret_size,
    })
}

/// First-fit allocator of slots for values with known live intervals.
#[derive(Default)]
struct IntervalAllocator {
    /// (end position, offset, size) of live intervals.
    live: Vec<(usize, usize, usize)>,
    /// (offset, size) of free ranges.
    free: Vec<(usize, usize)>,
    top: usize,
}

impl IntervalAllocator {
    /// Free the slots of values whose last use is before instruction `pos`.
    fn release_dead(&mut self, pos: usize) {
        let (dead, live): (Vec<_>, Vec<_>) =
            self.live.drain(..).partition(|(end, _, _)| *end < pos);
        self.live = live;
        for (_, off, size) in dead {
            self.free.push((off, size));
        }
    }

    fn alloc(&mut self, size: usize, end: usize) -> usize {
        let off = if let Some(i) = self.free.iter().position(|(_, s)| *s == size) {
            self.free.remove(i).0
        } else if let Some(i) = self.free.iter().position(|(_, s)| *s > size) {
            let (off, s) = self.free.remove(i);
            self.free.push((off + size, s - size));
            off
        } else {
            self.top += size;
            self.top - size
        };
        self.live.push((end, off, size));
        off
    }
}

/// Allocate dedicated slots for phis and cross-block values; returns the area size.
///
/// Blocks are laid out in order to give every instruction a position (a block of
/// `k` instructions occupies `k + 1` positions, the last one for its terminator).
/// A liveness analysis over the CFG yields the values live on entry to and exit from
/// each block. A value's interval spans from its definition to its last use, widened
/// to cover every block it is live in and, for phis, the terminators of all
/// predecessors (where phis are written). Two values live at the same program point
/// then have overlapping intervals, so the first-fit allocator keeps them apart;
/// everything else may share a slot.
fn allocate_cross_block(
    f: &Function,
    types: &TypeTable,
    uses: &HashMap<Value, Vec<(BlockId, usize)>>,
    is_local: &dyn Fn(Value) -> bool,
    slots: &mut HashMap<Value, Slot>,
) -> usize {
    let nb = f.blocks.len();
    let mut base = vec![0usize; nb];
    let mut pos = 0;
    for (b, bb) in f.blocks.iter().enumerate() {
        base[b] = pos;
        pos += bb.instrs.len() + 1;
    }
    let term_pos = |b: BlockId| base[b] + f.blocks[b].instrs.len();
    let successors = |b: BlockId| -> Vec<BlockId> {
        match &f.blocks[b].term {
            Terminator::Br(d) => vec![*d],
            Terminator::CondBr {
                if_true, if_false, ..
            } => vec![*if_true, *if_false],
            Terminator::Switch { cases, default, .. } => {
                let mut v: Vec<BlockId> = cases.iter().map(|(_, d)| *d).collect();
                v.push(*default);
                v
            }
            Terminator::Ret(_) | Terminator::Unreachable => vec![],
        }
    };
    // Candidates (phis and non-local instruction results) and where they are defined.
    let mut def_pos: HashMap<Value, usize> = HashMap::new();
    for (b, bb) in f.blocks.iter().enumerate() {
        for phi in &bb.phis {
            def_pos.insert(phi.dest, base[b]);
        }
        for (i, inst) in bb.instrs.iter().enumerate() {
            if let Some(d) = inst.dest {
                if !is_local(d) {
                    def_pos.insert(d, base[b] + i);
                }
            }
        }
    }
    // Per block: candidates read before being defined (`upward`), candidates defined,
    // and candidates read by the phis of successors (live at the end of this block).
    let mut upward: Vec<HashSet<Value>> = vec![HashSet::new(); nb];
    let mut defined: Vec<HashSet<Value>> = vec![HashSet::new(); nb];
    let mut phi_out: Vec<HashSet<Value>> = vec![HashSet::new(); nb];
    for (b, bb) in f.blocks.iter().enumerate() {
        for phi in &bb.phis {
            defined[b].insert(phi.dest);
            for (val, pred) in &phi.incoming {
                if let Some(v) = val.as_value() {
                    if def_pos.contains_key(&v) {
                        phi_out[*pred].insert(v);
                    }
                }
            }
        }
        let reads = bb
            .instrs
            .iter()
            .flat_map(|inst| op_values(&inst.op).into_iter().chain(inst.dest))
            .chain(term_values(&bb.term));
        // `dest` is chained after the operands so a value read by its own definition's
        // operands is still seen as upward-exposed; SSA forbids that anyway.
        for v in reads {
            if def_pos.contains_key(&v) && !defined[b].contains(&v) {
                upward[b].insert(v);
            }
        }
        for inst in &bb.instrs {
            if let Some(d) = inst.dest {
                defined[b].insert(d);
            }
        }
    }
    let mut live_in: Vec<HashSet<Value>> = vec![HashSet::new(); nb];
    let mut live_out: Vec<HashSet<Value>> = vec![HashSet::new(); nb];
    loop {
        let mut changed = false;
        for b in (0..nb).rev() {
            let mut out = phi_out[b].clone();
            for s in successors(b) {
                out.extend(live_in[s].iter().copied());
            }
            let mut inn = upward[b].clone();
            inn.extend(out.iter().filter(|v| !defined[b].contains(v)));
            if out != live_out[b] || inn != live_in[b] {
                live_out[b] = out;
                live_in[b] = inn;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut intervals: Vec<(usize, usize, Value)> = Vec::new();
    for (&v, &def) in &def_pos {
        let mut start = def;
        let mut end = def;
        for (b, p) in uses.get(&v).map(|u| u.as_slice()).unwrap_or(&[]) {
            end = end.max(base[*b] + p);
        }
        for b in 0..nb {
            if live_in[b].contains(&v) {
                start = start.min(base[b]);
                end = end.max(base[b]);
            }
            if live_out[b].contains(&v) {
                end = end.max(term_pos(b));
            }
        }
        intervals.push((start, end, v));
    }
    for bb in &f.blocks {
        for phi in &bb.phis {
            let e = intervals
                .iter_mut()
                .find(|(_, _, v)| *v == phi.dest)
                .unwrap();
            for (_, pred) in &phi.incoming {
                e.0 = e.0.min(term_pos(*pred));
                e.1 = e.1.max(term_pos(*pred));
            }
        }
    }
    intervals.sort_unstable();
    let mut alloc = IntervalAllocator::default();
    for (start, end, v) in intervals {
        alloc.release_dead(start);
        let size = types.size(f.value_ty(v));
        let off = alloc.alloc(size, end);
        slots.insert(
            v,
            Slot {
                kind: SlotKind::Local,
                off,
                size,
            },
        );
    }
    alloc.top
}

/// The SSA values an instruction reads.
pub fn op_values(op: &Op) -> Vec<Value> {
    let mut v = Vec::new();
    let mut add = |o: &Operand| {
        if let Some(x) = o.as_value() {
            v.push(x);
        }
    };
    match op {
        Op::Binary { a, b, .. } | Op::ICmp { a, b, .. } => {
            add(a);
            add(b);
        }
        Op::Cast { x, .. } | Op::Freeze { x } => add(x),
        Op::Load { addr, .. } => add(addr),
        Op::Store { addr, value, .. } => {
            add(addr);
            add(value);
        }
        Op::Gep { base, indices, .. } => {
            add(base);
            indices.iter().for_each(&mut add);
        }
        Op::Call { callee, args, .. } => {
            if let Callee::Indirect(c) = callee {
                add(c);
            }
            args.iter().for_each(&mut add);
        }
        Op::Alloca { .. } => {}
        Op::Select {
            cond,
            if_true,
            if_false,
        } => {
            add(cond);
            add(if_true);
            add(if_false);
        }
        Op::ExtractValue { agg, .. } => add(agg),
        Op::InsertValue { agg, elem, .. } => {
            add(agg);
            add(elem);
        }
        Op::VaArg { list } => add(list),
    }
    v
}

/// The SSA values a terminator reads (phi incomings are counted separately).
pub fn term_values(t: &Terminator) -> Vec<Value> {
    match t {
        Terminator::Ret(Some(op))
        | Terminator::CondBr { cond: op, .. }
        | Terminator::Switch { value: op, .. } => op.as_value().into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Multiplications by a variable and all divisions go through the shared routines;
/// they are far too big to inline. Odd bit widths are kept inline (the routines
/// work on whole limbs).
fn routed_arith(types: &TypeTable, op: BinOp, a: &Operand, b: &Operand) -> bool {
    let Some(kind) = ArithKind::of(op) else {
        return false;
    };
    let Ok(bits) = types.bits(a.ty) else {
        return false;
    };
    bits % 8 == 0 && bits <= 64 && (kind != ArithKind::Mul || b.as_const().is_none())
}

/// Accumulates the segments of one basic block.
struct SegBuilder<'a> {
    segments: Vec<Segment>,
    cur: Vec<Step>,
    prev_end: Option<SegEnd>,
    next_id: &'a mut u16,
    access_kinds: &'a mut Vec<(bool, usize)>,
    arith_kinds: &'a mut Vec<(ArithKind, usize)>,
}

impl<'a> SegBuilder<'a> {
    /// Close the current segment with `end`.
    fn finish(&mut self, end: SegEnd) {
        self.segments.push(Segment {
            id: *self.next_id,
            steps: std::mem::take(&mut self.cur),
            end: end.clone(),
            prev_end: self.prev_end.take(),
        });
        *self.next_id = self.next_id.checked_add(1).expect("too many blocks");
        self.prev_end = Some(end);
    }

    /// Close the segment with an arithmetic routine request.
    fn arith(&mut self, instr: usize, op: BinOp, n: usize) {
        let kind = ArithKind::of(op).expect("arithmetic routine kind");
        if !self.arith_kinds.contains(&(kind, n)) {
            self.arith_kinds.push((kind, n));
        }
        self.finish(SegEnd::Arith(ArithOp { instr, op, n }));
    }

    /// Close one segment per aligned chunk of a memory access.
    fn access(
        &mut self,
        instr: usize,
        is_store: bool,
        size: usize,
        align: usize,
        base_off: usize,
        role: AccessRole,
    ) {
        for (off, width) in chunks(size, align) {
            if !self.access_kinds.contains(&(is_store, width)) {
                self.access_kinds.push((is_store, width));
            }
            self.finish(SegEnd::Access(Access {
                instr,
                is_store,
                off: base_off + off,
                width,
                role,
            }));
        }
    }
}

pub fn mask_bits(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        v & ((1u64 << bits) - 1)
    }
}

/// Evaluates constants to numbers and bytes once global addresses and function ids are known.
pub struct ConstEval<'a> {
    pub types: &'a TypeTable,
    pub global_addr: &'a [u64],
    /// Entry segment id of each function (`None` for declarations).
    pub func_entry: &'a [Option<u16>],
}

impl<'a> ConstEval<'a> {
    /// A constant as a scalar (zero-extended; callers mask to the operand width).
    pub fn scalar(&self, c: &Constant) -> Result<u64> {
        Ok(match c {
            Constant::Int(v) => *v,
            Constant::Zero => 0,
            Constant::Global(g) => self.global_addr[*g],
            Constant::Func(f) => self.func_entry[*f]
                .ok_or_else(|| anyhow::anyhow!("address of undefined function"))?
                as u64,
            Constant::Offset { base, bytes } => {
                let b = self.scalar(base)? as i64;
                mask_bits((b + bytes) as u64, 8 * self.types.ptr_bytes as u32)
            }
            Constant::Bytes(_) | Constant::Agg(_) => bail!("aggregate constant used as a scalar"),
        })
    }

    /// A constant operand as a scalar, masked to the operand's width.
    pub fn operand_scalar(&self, op: &Operand) -> Result<u64> {
        let c = op
            .as_const()
            .ok_or_else(|| anyhow::anyhow!("not a constant"))?;
        Ok(mask_bits(self.scalar(c)?, self.types.bits(op.ty)?))
    }

    /// The in-memory bytes of a constant of type `ty`.
    pub fn bytes(&self, c: &Constant, ty: TypeId) -> Result<Vec<u8>> {
        let size = self.types.size(ty);
        let mut out = vec![0u8; size];
        match c {
            Constant::Zero => {}
            Constant::Bytes(b) => {
                let n = b.len().min(size);
                out[..n].copy_from_slice(&b[..n]);
            }
            Constant::Agg(leaves) => {
                for leaf in leaves {
                    let b = self.bytes(&leaf.value, leaf.ty)?;
                    out[leaf.offset..leaf.offset + b.len()].copy_from_slice(&b);
                }
            }
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

pub fn plan_module(m: &Module, mem_size: usize, entry: &str) -> Result<Plan> {
    let types = &m.types;
    let mut next_id: u16 = 1; // 0 is reserved (null function pointer)
    let start_id = next_id;
    let exit_id = next_id + 1;
    next_id += 2;
    let mut funcs = Vec::new();
    let mut func_plan = HashMap::new();
    let mut access_kinds = Vec::new();
    let mut arith_kinds = Vec::new();
    for (fid, f) in m.functions.iter().enumerate() {
        if f.is_declaration() {
            continue;
        }
        let fp = plan_function(m, f, fid, &mut next_id, &mut access_kinds, &mut arith_kinds)
            .with_context(|| format!("in function {}", f.name))?;
        func_plan.insert(fid, funcs.len());
        funcs.push(fp);
    }
    if !funcs.iter().any(|f| f.name == entry) {
        bail!("entry function '{}' not found (or has no body)", entry);
    }
    // Globals: laid out in declaration order from address 16; the heap follows.
    let mut global_addr = vec![0u64; m.globals.len()];
    let mut addr: u64 = 16;
    for (gi, g) in m.globals.iter().enumerate() {
        if g.name == "__llvbf_heap_start" {
            continue;
        }
        if g.init.is_none() {
            bail!("external global @{} has no definition", g.name);
        }
        let align = (g.align as u64).max(types.align(g.ty) as u64).max(1);
        addr = addr.div_ceil(align) * align;
        global_addr[gi] = addr;
        addr += types.size(g.ty) as u64;
    }
    let heap_start = addr.div_ceil(8) * 8;
    if let Some(gi) = m
        .globals
        .iter()
        .position(|g| g.name == "__llvbf_heap_start")
    {
        global_addr[gi] = heap_start;
    }
    if heap_start as usize > mem_size {
        bail!(
            "global data ({} bytes) does not fit into memory of {} bytes",
            heap_start,
            mem_size
        );
    }
    let func_entry: Vec<Option<u16>> = (0..m.functions.len())
        .map(|fid| func_plan.get(&fid).map(|&i| funcs[i].entry_id))
        .collect();
    let ce = ConstEval {
        types,
        global_addr: &global_addr,
        func_entry: &func_entry,
    };
    let mut global_bytes = Vec::new();
    for (gi, g) in m.globals.iter().enumerate() {
        if let Some(init) = &g.init {
            let bytes = ce
                .bytes(init, g.ty)
                .with_context(|| format!("initializer of @{}", g.name))?;
            for (i, b) in bytes.iter().enumerate() {
                if *b != 0 {
                    global_bytes.push((global_addr[gi] + i as u64, *b));
                }
            }
        }
    }
    let needs_msp = compute_needs_msp(m);
    let ret_max = funcs.iter().map(|f| f.ret_size).max().unwrap_or(0).max(2);
    let params_max = funcs.iter().map(|f| f.params_size).max().unwrap_or(0);
    let locals_max = funcs.iter().map(|f| f.locals_size).max().unwrap_or(0);
    let pool_max = funcs.iter().map(|f| f.pool_size).max().unwrap_or(0);
    Ok(Plan {
        funcs,
        func_plan,
        global_addr,
        global_bytes,
        heap_start,
        ret_max,
        params_max,
        locals_max,
        pool_max,
        num_ids: next_id as usize,
        start_id,
        exit_id,
        access_kinds,
        arg_size: 2 * arith_kinds.iter().map(|k| k.1).max().unwrap_or(0),
        arith_kinds,
        mem_size,
        needs_msp,
    })
}

/// Which functions need a valid memory stack pointer in their frame (see `Plan::needs_msp`).
fn compute_needs_msp(m: &Module) -> Vec<bool> {
    let n = m.functions.len();
    let mut needs = vec![false; n];
    let mut callees: Vec<Vec<FuncId>> = vec![Vec::new(); n];
    for (fid, f) in m.functions.iter().enumerate() {
        for bb in &f.blocks {
            for inst in &bb.instrs {
                match &inst.op {
                    Op::Alloca { .. } | Op::VaArg { .. } => needs[fid] = true,
                    Op::Call { callee, fn_ty, .. } => {
                        if call_is_variadic(&m.types, *fn_ty) {
                            needs[fid] = true;
                        }
                        match callee {
                            Callee::Direct(c) => callees[fid].push(*c),
                            Callee::Indirect(_) => needs[fid] = true,
                            Callee::Intrinsic(name) => {
                                if name.starts_with("llvm.va_") {
                                    needs[fid] = true;
                                }
                                if let Some(rt) = intrinsic_runtime_call(name) {
                                    if let Some(c) = m.func_by_name(rt) {
                                        callees[fid].push(c);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Propagate to callers until stable.
    loop {
        let mut changed = false;
        for fid in 0..n {
            if !needs[fid] && callees[fid].iter().any(|&c| needs[c]) {
                needs[fid] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    needs
}
