//! Code generation: lowers a planned module to Brainfuck.
//!
//! See README.md for the tape layout, the dispatch loop and the memory system.

use crate::arith::*;
use crate::assist::GepTable;
use crate::emit::{Cell, Emitter};
use crate::plan::*;
use crate::types::TypeInfo;
use anyhow::{anyhow, bail, Context, Result};
use either::Either;
use llvm_ir::instruction::{self, Instruction};
use llvm_ir::predicates::IntPredicate;
use llvm_ir::terminator::Terminator;
use llvm_ir::{Module, Name, Operand, Type, TypeRef};
use std::collections::HashMap;

pub struct Options {
    /// Size of addressable memory in bytes (multiple of 256, at most 65536).
    pub mem_size: usize,
    /// Emit comments describing the generated code.
    pub comments: bool,
    /// Name of the entry function.
    pub entry: String,
}

impl Default for Options {
    fn default() -> Self {
        Options { mem_size: 16384, comments: false, entry: "main".to_string() }
    }
}

/// Cell offsets within a stack frame (all frames share this layout) and the
/// geometry of the memory blocks.
#[derive(Clone, Debug)]
pub struct Layout {
    pub active: Cell,
    pub home: Cell,
    pub nothome: Cell,
    pub pc_lo: Cell,
    pub pc_hi: Cell,
    pub ch_lo: Cell,
    pub ch_hi: Cell,
    pub tmp: Cell,
    pub req: Cell,
    pub addr_hi: Cell,
    pub hops: Cell,
    /// Start of the walking control block: addr_lo, cnt, t, data[0..8].
    pub addr_lo: Cell,
    pub cnt: Cell,
    pub t: Cell,
    pub data: Cell,
    pub msp: Cell,
    pub scratch: Cell,
    pub scratch_size: usize,
    pub ret: Cell,
    pub params: Cell,
    pub locals: Cell,
    pub frame: Cell,
    pub ctl: Cell,
    pub block: Cell,
    pub num_blocks: usize,
    pub ptr_bytes: usize,
}

impl Layout {
    fn new(plan: &Plan, ptr_bytes: usize, scratch_size: usize) -> Layout {
        let active = 0;
        let home = 1; // pads at 2, 3
        let nothome = 4;
        let pc_lo = 5;
        let pc_hi = 6;
        let ch_lo = 7; // pads 8, 9
        let ch_hi = 10; // pads 11, 12
        let tmp = 13;
        let req = 14;
        let addr_hi = req + plan.access_kinds.len().max(1) as Cell;
        let hops = addr_hi + 1;
        let addr_lo = hops + 1;
        let cnt = addr_lo + 1;
        let t = cnt + 1;
        let data = t + 1;
        let ctl = data + 8;
        let msp = ctl;
        let scratch = msp + ptr_bytes as Cell;
        let ret = scratch + scratch_size as Cell;
        let params = ret + plan.ret_max as Cell;
        let locals = params + plan.params_max as Cell;
        let frame = locals + plan.locals_max as Cell;
        Layout {
            active,
            home,
            nothome,
            pc_lo,
            pc_hi,
            ch_lo,
            ch_hi,
            tmp,
            req,
            addr_hi,
            hops,
            addr_lo,
            cnt,
            t,
            data,
            msp,
            scratch,
            scratch_size,
            ret,
            params,
            locals,
            frame,
            ctl,
            block: ctl + 256,
            num_blocks: plan.mem_size / 256,
            ptr_bytes,
        }
    }

    /// Absolute tape index of memory byte `addr`.
    fn abs_cell(&self, addr: u64) -> Cell {
        let h = (addr >> 8) as Cell;
        let l = (addr & 255) as Cell;
        (self.num_blocks as Cell - 1 - h) * self.block + self.ctl + l
    }

    fn guard_base(&self) -> Cell {
        self.num_blocks as Cell * self.block
    }

    fn frame0_base(&self) -> Cell {
        self.guard_base() + self.frame
    }
}

struct SegRef {
    id: u16,
    func: usize,
    block: usize,
    seg: usize,
}

struct Gen<'a> {
    module: &'a Module,
    ti: &'a TypeInfo<'a>,
    plan: &'a Plan,
    geps: &'a GepTable,
    lay: Layout,
    opts: &'a Options,
    e: Emitter,
    sc: Scratch,
    func_ids: HashMap<String, u16>,
    cur: usize,
}

/// Compile a module to Brainfuck.
pub fn compile(module: &Module, geps: &GepTable, opts: &Options) -> Result<Vec<u8>> {
    if opts.mem_size % 256 != 0 || opts.mem_size == 0 || opts.mem_size > 65536 {
        bail!("memory size must be a positive multiple of 256 and at most 65536");
    }
    let ti = TypeInfo::new(module);
    let plan = plan_module(module, &ti, geps, opts.mem_size, &opts.entry)?;
    // Pass 1 measures the scratch requirement, pass 2 emits the final code.
    let (hw, _) = run_gen(module, &ti, &plan, geps, opts, 4096)?;
    let (_, code) = run_gen(module, &ti, &plan, geps, opts, hw + 4)?;
    Ok(code)
}

fn run_gen(
    module: &Module,
    ti: &TypeInfo,
    plan: &Plan,
    geps: &GepTable,
    opts: &Options,
    scratch_size: usize,
) -> Result<(usize, Vec<u8>)> {
    let lay = Layout::new(plan, ti.ptr_bytes, scratch_size);
    let mut e = Emitter::new();
    e.comments = opts.comments;
    let sc = Scratch::new(lay.scratch);
    let func_ids = plan.funcs.iter().map(|f| (f.name.clone(), f.entry_id)).collect();
    let mut g = Gen { module, ti, plan, geps, lay, opts, e, sc, func_ids, cur: 0 };
    g.emit_program()?;
    let used = g.sc.used();
    if used > scratch_size {
        bail!("internal error: scratch overflow");
    }
    Ok((used, g.e.into_code()))
}

impl<'a> Gen<'a> {
    fn ce(&self) -> ConstEval<'_> {
        ConstEval { module: self.module, ti: self.ti, geps: self.geps, globals: &self.plan.globals, func_ids: &self.func_ids }
    }

    fn fp(&self) -> &'a FuncPlan {
        &self.plan.funcs[self.cur]
    }

    fn func(&self) -> &'a llvm_ir::Function {
        &self.module.functions[self.fp().fidx]
    }

    fn comment(&mut self, s: &str) {
        self.e.comment(s);
    }

    /// Cell and size of the slot of an SSA value in the current frame.
    fn slot(&self, name: &Name) -> Result<(Cell, usize)> {
        let s = self.fp().slots.get(name).ok_or_else(|| anyhow!("unknown value %{}", name))?;
        Ok((self.slot_cell(s), s.size))
    }

    fn slot_cell(&self, s: &Slot) -> Cell {
        match s.kind {
            SlotKind::Param => self.lay.params + s.off as Cell,
            SlotKind::Local => self.lay.locals + s.off as Cell,
        }
    }

    fn op_type(&self, op: &Operand) -> TypeRef {
        self.module.type_of(op)
    }

    fn op_bits(&self, op: &Operand) -> Result<u32> {
        self.ti.scalar_bits(&self.op_type(op))
    }

    fn ptr_limbs(&self) -> usize {
        self.lay.ptr_bytes
    }

    /// Load a scalar operand into a zeroed work area (zero-extending if the work is wider).
    fn load_operand(&mut self, op: &Operand, w: Work) -> Result<()> {
        match op {
            Operand::LocalOperand { name, .. } => {
                let (cell, size) = self.slot(name)?;
                let n = w.n.min(size);
                load_packed(&mut self.e, &mut self.sc, cell, w.truncated(n));
            }
            Operand::ConstantOperand(c) => {
                let v = self.ce().scalar(c)?;
                load_const(&mut self.e, w, v);
            }
            Operand::MetadataOperand => bail!("metadata operand in value position"),
        }
        Ok(())
    }

    /// Bytes `off..off+n` of an operand's in-memory representation into a work area.
    fn load_operand_bytes(&mut self, op: &Operand, off: usize, w: Work) -> Result<()> {
        match op {
            Operand::LocalOperand { name, .. } => {
                let (cell, size) = self.slot(name)?;
                if off + w.n > size {
                    bail!("byte range out of slot bounds for %{}", name);
                }
                load_packed(&mut self.e, &mut self.sc, cell + off as Cell, w);
            }
            Operand::ConstantOperand(c) => {
                let ty = self.op_type(op);
                let bytes = self.ce().bytes(c, &ty)?;
                for k in 0..w.n {
                    let b = bytes.get(off + k).copied().unwrap_or(0);
                    if b != 0 {
                        self.e.set_using(w.limb(k), b, w.limb(k) + 1);
                    }
                }
            }
            Operand::MetadataOperand => bail!("metadata operand in value position"),
        }
        Ok(())
    }

    /// Store a work area into the slot of `name`, clearing the whole slot first.
    fn store_result(&mut self, name: &Name, w: Work) -> Result<()> {
        let (cell, size) = self.slot(name)?;
        self.store_to_cells(cell, size, w);
        Ok(())
    }

    fn store_to_cells(&mut self, cell: Cell, size: usize, w: Work) {
        for i in 0..size {
            self.e.clear(cell + i as Cell);
        }
        for i in 0..w.n.min(size) {
            self.e.move_cell(w.limb(i), cell + i as Cell);
        }
        for i in size..w.n {
            self.e.clear(w.limb(i));
        }
    }

    fn set_pc(&mut self, base: Cell, id: u16) {
        self.e.set(base + self.lay.pc_lo, (id & 0xff) as u8);
        self.e.set(base + self.lay.pc_hi, (id >> 8) as u8);
    }

    /// Prepare the dispatch-chain idiom cells of the frame at `base` so that the
    /// pending `if_zero` epilogues of the dispatch chain find the state they expect
    /// after a frame shift.
    fn prepare_dispatch_cells(&mut self, base: Cell) {
        for x in [self.lay.ch_lo, self.lay.ch_hi] {
            self.e.clear(base + x);
            self.e.clear(base + x + 1);
            self.e.clear(base + x + 2);
            self.e.dec(base + x + 2);
        }
    }

    // -----------------------------------------------------------------------
    // Program structure
    // -----------------------------------------------------------------------

    fn emit_program(&mut self) -> Result<()> {
        let lay = self.lay.clone();
        self.comment(&format!(
            "llvbf output: {} memory blocks of {} cells, frame size {}, {} dispatch blocks",
            lay.num_blocks, lay.block, lay.frame, self.plan.num_ids
        ));
        // Initialize global data (absolute addressing from tape cell 0).
        self.comment("initialize global data");
        let mut bytes = self.plan.global_bytes.clone();
        bytes.sort();
        for (addr, b) in bytes {
            let cell = lay.abs_cell(addr);
            self.e.set_using(cell, b, cell + 1);
        }
        // Enter frame 0 (the _start frame).
        self.e.goto(lay.frame0_base());
        self.e.set_pos(0);
        self.comment("start frame");
        self.e.inc(lay.active);
        self.set_pc(0, self.plan.start_id);
        let stack_top = (self.plan.mem_size - 8) as u64;
        {
            let pl = self.ptr_limbs();
            let w = Work::alloc(&mut self.sc, pl);
            load_const(&mut self.e, w, stack_top);
            self.store_to_cells(lay.msp, self.ptr_limbs(), w);
            self.sc.release(w.base);
        }
        // Main loop.
        self.e.begin_while(lay.active);
        self.emit_dispatch()?;
        self.emit_memory_routines();
        self.e.end_while(lay.active);
        self.comment("end of program");
        Ok(())
    }

    fn collect_segments(&self) -> Vec<SegRef> {
        let mut v = Vec::new();
        for (fi, f) in self.plan.funcs.iter().enumerate() {
            for (bi, b) in f.blocks.iter().enumerate() {
                for (si, s) in b.segments.iter().enumerate() {
                    v.push(SegRef { id: s.id, func: fi, block: bi, seg: si });
                }
            }
        }
        v.sort_by_key(|s| s.id);
        v
    }

    fn emit_dispatch(&mut self) -> Result<()> {
        let lay = self.lay.clone();
        self.comment("dispatch: copy pc");
        self.e.copy_cell(lay.pc_lo, lay.ch_lo, lay.tmp);
        self.e.copy_cell(lay.pc_hi, lay.ch_hi, lay.tmp);
        let segs = self.collect_segments();
        // Synthetic start/exit segments have ids start_id, exit_id (below all others).
        let mut all: Vec<(u16, Option<usize>)> = vec![(self.plan.start_id, None), (self.plan.exit_id, None)];
        for (i, s) in segs.iter().enumerate() {
            all.push((s.id, Some(i)));
        }
        all.sort_by_key(|x| x.0);
        let mut prev_hi: i32 = 0;
        let mut i = 0;
        while i < all.len() {
            let hi = (all[i].0 >> 8) as i32;
            self.e.add(lay.ch_hi, -(hi - prev_hi));
            prev_hi = hi;
            self.e.begin_if_zero(lay.ch_hi);
            let mut prev_lo: i32 = 0;
            while i < all.len() && (all[i].0 >> 8) as i32 == hi {
                let (id, sref) = (all[i].0, all[i].1);
                let lo = (id & 0xff) as i32;
                self.e.add(lay.ch_lo, -(lo - prev_lo));
                prev_lo = lo;
                self.e.begin_if_zero(lay.ch_lo);
                match sref {
                    None => self.emit_synthetic(id)?,
                    Some(si) => {
                        let s = &segs[si];
                        self.emit_segment(s.func, s.block, s.seg)?;
                    }
                }
                self.e.end_if_zero(lay.ch_lo);
                i += 1;
            }
            self.e.clear(lay.ch_lo);
            self.e.end_if_zero(lay.ch_hi);
        }
        self.e.clear(lay.ch_hi);
        Ok(())
    }

    fn emit_synthetic(&mut self, id: u16) -> Result<()> {
        let lay = self.lay.clone();
        if id == self.plan.start_id {
            self.comment("_start: call entry function");
            let entry_idx = self.plan.func_by_name[&self.opts.entry];
            let entry_id = self.plan.funcs[entry_idx].entry_id;
            let f = lay.frame;
            // Parameters of main (argc, argv) are left zero.
            for i in 0..self.ptr_limbs() {
                self.e.clear(f + lay.msp + i as Cell);
                self.e.copy_cell(lay.msp + i as Cell, f + lay.msp + i as Cell, lay.tmp);
            }
            self.e.set(f + lay.active, 1);
            self.e.clear(f + lay.home);
            self.set_pc(f, entry_id);
            self.prepare_dispatch_cells(f);
            self.set_pc(0, self.plan.exit_id);
            let p = self.e.pos();
            self.e.set_pos(p - f);
        } else {
            self.comment("_exit");
            self.e.clear(lay.active);
        }
        Ok(())
    }

    fn emit_segment(&mut self, func: usize, block: usize, seg: usize) -> Result<()> {
        self.cur = func;
        let fp = self.fp();
        let f = self.func();
        let bb = &f.basic_blocks[block];
        let s = &fp.blocks[block].segments[seg];
        self.comment(&format!("segment {} = {} / %{} / {}", s.id, fp.name, bb.name, seg));
        let m = self.sc.mark();
        // Finish what the previous segment started.
        match &s.prev_end {
            Some(SegEnd::Access(a)) if !a.is_store => self.finish_load(block, bb, *a)?,
            Some(SegEnd::Call(i)) => self.finish_call(bb, *i)?,
            _ => {}
        }
        for step in &s.steps {
            let m2 = self.sc.mark();
            match *step {
                Step::Instr(i) => {
                    if self.opts.comments {
                        self.comment(&format!("{}", bb.instrs[i]));
                    }
                    self.emit_instr(bb, i).with_context(|| format!("in {}: {}", fp.name, bb.instrs[i]))?;
                }
                Step::VaSetup(i) => {
                    self.comment("variadic call: reserve spill area");
                    let call = match &bb.instrs[i] {
                        Instruction::Call(c) => c,
                        _ => unreachable!(),
                    };
                    let (_, total) = va_spill_layout(self.module, self.ti, call)?;
                    self.adjust_msp((total as u64).wrapping_neg());
                }
            }
            self.sc.release(m2);
        }
        match &s.end {
            SegEnd::Term => {
                if self.opts.comments {
                    self.comment(&format!("{}", bb.term));
                }
                self.emit_terminator(bb, s).with_context(|| format!("in {}: {}", fp.name, bb.term))?
            }
            SegEnd::Call(i) => self.emit_call(bb, *i, s).with_context(|| format!("in {}: {}", fp.name, bb.instrs[*i]))?,
            SegEnd::Access(a) => self.emit_access(block, bb, *a, s).with_context(|| format!("in {}: {}", fp.name, bb.instrs[a.instr]))?,
        }
        self.sc.release(m);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Calls and returns
    // -----------------------------------------------------------------------

    fn finish_call(&mut self, bb: &llvm_ir::BasicBlock, i: usize) -> Result<()> {
        let call = match &bb.instrs[i] {
            Instruction::Call(c) => c,
            _ => unreachable!(),
        };
        if let Some(dest) = &call.dest {
            let (cell, size) = self.slot(dest)?;
            let f = self.lay.frame;
            for k in 0..size {
                self.e.clear(cell + k as Cell);
                self.e.move_cell(f + self.lay.ret + k as Cell, cell + k as Cell);
            }
        }
        if call_is_variadic(call) {
            let (_, total) = va_spill_layout(self.module, self.ti, call)?;
            self.adjust_msp(total as u64);
        }
        Ok(())
    }

    /// MSP += delta (mod 2^ptrbits).
    fn adjust_msp(&mut self, delta: u64) {
        let n = self.ptr_limbs();
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        load_packed(&mut self.e, &mut self.sc, self.lay.msp, w);
        add_const(&mut self.e, &mut self.sc, w, mask_bits(delta, 8 * n as u32));
        self.store_to_cells(self.lay.msp, n, w);
        self.sc.release(m);
    }

    fn extra_slot(&self, block: usize, instr: usize) -> Result<(Cell, usize)> {
        let s = self
            .fp()
            .extra_slots
            .get(&(block, instr))
            .ok_or_else(|| anyhow!("internal error: missing hidden slot"))?;
        Ok((self.slot_cell(s), s.size))
    }

    fn param_offsets(&self, tys: &[TypeRef]) -> Result<Vec<(usize, usize)>> {
        let mut v = Vec::new();
        let mut off = 0;
        for t in tys {
            let s = self.ti.size_of(t)?;
            v.push((off, s));
            off += s;
        }
        Ok(v)
    }

    fn emit_call(&mut self, bb: &llvm_ir::BasicBlock, i: usize, seg: &Segment) -> Result<()> {
        let call = match &bb.instrs[i] {
            Instruction::Call(c) => c,
            _ => unreachable!(),
        };
        let lay = self.lay.clone();
        let f = lay.frame;
        let callee_op = match &call.function {
            Either::Right(op) => op,
            Either::Left(_) => bail!("inline assembly is not supported"),
        };
        // Resolve the callee: direct function, runtime function for an intrinsic, or indirect.
        let mut direct: Option<usize> = None;
        let mut rewritten = false;
        let mut args: Vec<Operand> = call.arguments.iter().map(|(a, _)| a.clone()).collect();
        if let Some(name) = callee_name(call) {
            let target = match intrinsic_runtime_call(&name) {
                Some(rt) => {
                    args.truncate(3);
                    rewritten = true;
                    rt.to_string()
                }
                None => name,
            };
            let idx = self
                .plan
                .func_by_name
                .get(&target)
                .ok_or_else(|| anyhow!("call to undefined function @{} (link the llvbf runtime?)", target))?;
            direct = Some(*idx);
        }
        // Parameter layout from the callee's declared types (direct) or the call's function type.
        let variadic = !rewritten && call_is_variadic(call);
        let param_tys: Vec<TypeRef> = if rewritten {
            // Intrinsic rewritten to a runtime call: use the runtime function's own signature
            // (operands are zero-extended / truncated to the parameter widths).
            let callee = &self.module.functions[self.plan.funcs[direct.unwrap()].fidx];
            callee.parameters.iter().map(|p| p.ty.clone()).collect()
        } else {
            match call.function_ty.as_ref() {
                Type::FuncType { param_types, .. } => param_types.clone(),
                t => bail!("call through non-function type {}", t),
            }
        };
        if let Some(idx) = direct {
            let callee = &self.module.functions[self.plan.funcs[idx].fidx];
            if callee.is_var_arg != variadic || callee.parameters.len() != param_tys.len() {
                bail!("call signature does not match the definition of @{}", callee.name);
            }
        }
        if args.len() < param_tys.len() || (!variadic && args.len() != param_tys.len()) {
            bail!("argument count mismatch in call ({} vs {})", args.len(), param_tys.len());
        }
        let offs = self.param_offsets(&param_tys)?;
        let fixed_size: usize = offs.iter().map(|(_, s)| *s).sum();
        args.truncate(param_tys.len());
        self.comment("call: pass arguments");
        for (k, arg) in args.iter().enumerate() {
            let (off, size) = offs[k];
            let m = self.sc.mark();
            let w = Work::alloc(&mut self.sc, size);
            if self.ti.is_scalar(&param_tys[k]) {
                self.load_operand(arg, w)?;
            } else {
                self.load_operand_bytes(arg, 0, w)?;
            }
            self.store_to_cells(f + lay.params + off as Cell, size, w);
            self.sc.release(m);
        }
        if variadic {
            // Hidden parameter: pointer to the spilled variadic arguments (current MSP).
            let n = self.ptr_limbs();
            let m = self.sc.mark();
            let w = Work::alloc(&mut self.sc, n);
            load_packed(&mut self.e, &mut self.sc, lay.msp, w);
            self.store_to_cells(f + lay.params + fixed_size as Cell, n, w);
            self.sc.release(m);
        }
        self.comment("call: set up callee frame");
        for k in 0..self.ptr_limbs() {
            self.e.clear(f + lay.msp + k as Cell);
            self.e.copy_cell(lay.msp + k as Cell, f + lay.msp + k as Cell, lay.tmp);
        }
        self.e.set(f + lay.active, 1);
        self.e.clear(f + lay.home);
        match direct {
            Some(idx) => {
                let id = self.plan.funcs[idx].entry_id;
                self.set_pc(f, id);
            }
            None => {
                let m = self.sc.mark();
                let pl = self.ptr_limbs();
            let w = Work::alloc(&mut self.sc, pl);
                self.load_operand(callee_op, w)?;
                self.e.clear(f + lay.pc_lo);
                self.e.clear(f + lay.pc_hi);
                self.e.move_cell(w.limb(0), f + lay.pc_lo);
                self.e.move_cell(w.limb(1), f + lay.pc_hi);
                clear_work(&mut self.e, w);
                self.sc.release(m);
            }
        }
        self.prepare_dispatch_cells(f);
        self.set_pc(0, seg.id + 1);
        let p = self.e.pos();
        self.e.set_pos(p - f);
        Ok(())
    }

    fn emit_ret(&mut self, ret: &llvm_ir::terminator::Ret) -> Result<()> {
        let lay = self.lay.clone();
        if let Some(op) = &ret.return_operand {
            let ty = self.op_type(op);
            let size = self.ti.size_of(&ty)?;
            let m = self.sc.mark();
            let w = Work::alloc(&mut self.sc, size);
            if self.ti.is_scalar(&ty) {
                self.load_operand(op, w)?;
            } else {
                self.load_operand_bytes(op, 0, w)?;
            }
            self.store_to_cells(lay.ret, size, w);
            self.sc.release(m);
        }
        self.e.clear(lay.active);
        self.prepare_dispatch_cells(-lay.frame);
        let p = self.e.pos();
        self.e.set_pos(p + lay.frame);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Memory accesses
    // -----------------------------------------------------------------------

    fn access_kind_index(&self, is_store: bool, width: usize) -> Cell {
        self.plan.access_kinds.iter().position(|k| *k == (is_store, width)).expect("access kind") as Cell
    }

    fn finish_load(&mut self, block: usize, bb: &llvm_ir::BasicBlock, a: Access) -> Result<()> {
        let (cell, size) = match a.role {
            AccessRole::Plain => match &bb.instrs[a.instr] {
                Instruction::Load(l) => self.slot(&l.dest)?,
                _ => unreachable!(),
            },
            AccessRole::VaValue => match &bb.instrs[a.instr] {
                Instruction::VAArg(v) => self.slot(&v.dest)?,
                _ => unreachable!(),
            },
            AccessRole::VaPtrLoad | AccessRole::VaCopyLoad => self.extra_slot(block, a.instr)?,
            _ => bail!("internal error: finish_load on a store"),
        };
        if a.off + a.width > size {
            bail!("load chunk out of bounds");
        }
        for k in 0..a.width {
            let dst = cell + (a.off + k) as Cell;
            self.e.clear(dst);
            self.e.move_cell(self.lay.data + k as Cell, dst);
        }
        Ok(())
    }

    fn emit_access(&mut self, block: usize, bb: &llvm_ir::BasicBlock, a: Access, seg: &Segment) -> Result<()> {
        let lay = self.lay.clone();
        let n = self.ptr_limbs();
        self.comment(&format!("memory {} of {} bytes at offset {} ({:?})", if a.is_store { "store" } else { "load" }, a.width, a.off, a.role));
        let m = self.sc.mark();
        // Address.
        let w = Work::alloc(&mut self.sc, n);
        let mut addr_off = a.off as u64;
        match a.role {
            AccessRole::Plain => {
                let addr_op = match &bb.instrs[a.instr] {
                    Instruction::Load(l) => &l.address,
                    Instruction::Store(s) => &s.address,
                    _ => unreachable!(),
                };
                self.load_operand(addr_op, w)?;
            }
            AccessRole::VaPtrLoad | AccessRole::VaPtrStore => {
                let v = match &bb.instrs[a.instr] {
                    Instruction::VAArg(v) => v,
                    _ => unreachable!(),
                };
                self.load_operand(&v.arg_list, w)?;
            }
            AccessRole::VaValue => {
                let (cell, _) = self.extra_slot(block, a.instr)?;
                load_packed(&mut self.e, &mut self.sc, cell, w);
            }
            AccessRole::VaStart | AccessRole::VaCopyStore => {
                let c = match &bb.instrs[a.instr] {
                    Instruction::Call(c) => c,
                    _ => unreachable!(),
                };
                self.load_operand(&c.arguments[0].0, w)?;
            }
            AccessRole::VaCopyLoad => {
                let c = match &bb.instrs[a.instr] {
                    Instruction::Call(c) => c,
                    _ => unreachable!(),
                };
                self.load_operand(&c.arguments[1].0, w)?;
            }
            AccessRole::VaSpill(_) => {
                load_packed(&mut self.e, &mut self.sc, lay.msp, w);
            }
        }
        if addr_off != 0 {
            add_const(&mut self.e, &mut self.sc, w, addr_off);
            addr_off = 0;
        }
        let _ = addr_off;
        self.e.clear(lay.addr_lo);
        self.e.clear(lay.addr_hi);
        self.e.move_cell(w.limb(0), lay.addr_lo);
        if w.n > 1 {
            self.e.move_cell(w.limb(1), lay.addr_hi);
        }
        clear_work(&mut self.e, w);
        self.sc.release(m);
        // Data for stores.
        if a.is_store {
            let m = self.sc.mark();
            let dw = Work::alloc(&mut self.sc, a.width);
            match a.role {
                AccessRole::Plain => {
                    let v = match &bb.instrs[a.instr] {
                        Instruction::Store(s) => &s.value,
                        _ => unreachable!(),
                    };
                    self.load_operand_bytes(v, a.off, dw)?;
                }
                AccessRole::VaPtrStore => {
                    let v = match &bb.instrs[a.instr] {
                        Instruction::VAArg(v) => v,
                        _ => unreachable!(),
                    };
                    let (cell, _) = self.extra_slot(block, a.instr)?;
                    let full = Work::alloc(&mut self.sc, n);
                    load_packed(&mut self.e, &mut self.sc, cell, full);
                    let adv = va_slot_size(self.ti.size_of(&v.cur_type)?);
                    add_const(&mut self.e, &mut self.sc, full, adv as u64);
                    for k in 0..a.width {
                        self.e.move_cell(full.limb(a.off + k), dw.limb(k));
                    }
                    clear_work(&mut self.e, full);
                }
                AccessRole::VaStart => {
                    let s = self.fp().va_slot.ok_or_else(|| anyhow!("va_start in non-variadic function"))?;
                    let cell = self.slot_cell(&s);
                    load_packed(&mut self.e, &mut self.sc, cell + a.off as Cell, dw);
                }
                AccessRole::VaCopyStore => {
                    let (cell, _) = self.extra_slot(block, a.instr)?;
                    load_packed(&mut self.e, &mut self.sc, cell + a.off as Cell, dw);
                }
                AccessRole::VaSpill(k) => {
                    let c = match &bb.instrs[a.instr] {
                        Instruction::Call(c) => c,
                        _ => unreachable!(),
                    };
                    let (layout, _) = va_spill_layout(self.module, self.ti, c)?;
                    let fixed = c.arguments.len() - layout.len();
                    let (aoff, _) = layout[k];
                    let arg = &c.arguments[fixed + k].0;
                    let ty = self.op_type(arg);
                    if self.ti.is_scalar(&ty) && a.off - aoff == 0 && a.width >= self.ti.size_of(&ty)? {
                        self.load_operand(arg, dw)?;
                    } else {
                        self.load_operand_bytes(arg, a.off - aoff, dw)?;
                    }
                }
                _ => unreachable!(),
            }
            for k in 0..a.width {
                self.e.clear(lay.data + k as Cell);
                self.e.move_cell(dw.limb(k), lay.data + k as Cell);
            }
            self.sc.release(m);
        }
        let req = lay.req + self.access_kind_index(a.is_store, a.width);
        self.e.set(req, 1);
        self.set_pc(0, seg.id + 1);
        Ok(())
    }

    /// Move the transit cells of the memory request from the current region to the
    /// region at `stride` cells away, then re-anchor the origin there.
    fn hop(&mut self, stride: Cell, cells: &[Cell]) {
        for &c in cells {
            self.e.move_cell(c, c + stride);
        }
    }

    fn emit_memory_routines(&mut self) {
        let kinds = self.plan.access_kinds.clone();
        for (k, (is_store, width)) in kinds.iter().enumerate() {
            self.emit_memory_routine(k as Cell, *is_store, *width);
        }
    }

    fn emit_memory_routine(&mut self, k: Cell, is_store: bool, width: usize) {
        let lay = self.lay.clone();
        let f = lay.frame;
        let b = lay.block;
        let req = lay.req + k;
        let data: Vec<Cell> = (0..width as Cell).map(|i| lay.data + i).collect();
        self.comment(&format!("memory routine: {} {} bytes", if is_store { "store" } else { "load" }, width));
        self.e.begin_while(req);
        self.e.dec(req);
        self.e.inc(lay.home);
        // Hop down through the active frames to the guard frame.
        let mut transit = vec![lay.addr_lo, lay.addr_hi];
        if is_store {
            transit.extend(&data);
        }
        self.e.begin_while(lay.active);
        self.hop(-f, &transit);
        self.e.goto(lay.active - f);
        self.e.set_pos(lay.active);
        self.e.end_while(lay.active);
        // Guard frame -> block 0 control area.
        self.hop(-b, &transit);
        let p = self.e.pos();
        self.e.set_pos(p + b);
        // Hop blocks by the high address byte, counting hops.
        let mut transit_blk = vec![lay.addr_lo, lay.addr_hi, lay.hops];
        if is_store {
            transit_blk.extend(&data);
        }
        self.e.begin_while(lay.addr_hi);
        self.e.dec(lay.addr_hi);
        self.e.inc(lay.hops);
        self.hop(-b, &transit_blk);
        self.e.goto(lay.addr_hi - b);
        self.e.set_pos(lay.addr_hi);
        self.e.end_while(lay.addr_hi);
        // Walk the control block {addr_lo, cnt, t, data[0..width]} right through the
        // block's data cells, one cell per unit of the low address byte. Each step
        // rotates the displaced data cell to the left of the block, so walking back
        // the same number of steps restores the memory contents.
        let k = 3 + width as Cell; // size of the walking block
        let wb_start = lay.addr_lo;
        debug_assert_eq!(lay.cnt, wb_start + 1);
        debug_assert_eq!(lay.t, wb_start + 2);
        debug_assert_eq!(lay.data, wb_start + 3);
        debug_assert_eq!(lay.ctl, lay.data + 8);
        let block_end = wb_start + k; // first cell after the walking block (a memory data cell once aligned)
        // Move the block so that it ends right before data cell 0: shift by (ctl - block_end).
        let pre = lay.ctl - block_end;
        let cells: Vec<Cell> = (0..k).map(|i| wb_start + i).collect();
        if pre > 0 {
            for &c in cells.iter().rev() {
                self.e.move_cell(c, c + pre);
            }
            let p = self.e.pos();
            self.e.set_pos(p - pre);
        }
        // Now the walking block occupies [wb_start, block_end) and data cell 0 is at block_end.
        self.e.begin_while(wb_start);
        self.e.dec(wb_start);
        self.e.inc(lay.cnt);
        self.e.move_cell(block_end, lay.t); // displaced data cell -> t
        for &c in cells.iter().rev() {
            self.e.move_cell(c, c + 1);
        }
        self.e.move_cell(lay.t + 1, wb_start); // displaced value into the vacated cell
        self.e.goto(wb_start + 1);
        self.e.set_pos(wb_start);
        self.e.end_while(wb_start);
        // The addressed cells are block_end .. block_end + width.
        for i in 0..width as Cell {
            let mem = block_end + i;
            if is_store {
                self.e.clear(mem);
                self.e.move_cell(lay.data + i, mem);
            } else {
                self.e.copy_cell(mem, lay.data + i, lay.t);
            }
        }
        // Walk back.
        self.e.begin_while(lay.cnt);
        self.e.dec(lay.cnt);
        self.e.move_cell(wb_start - 1, lay.t); // displaced value -> t
        for &c in cells.iter() {
            self.e.move_cell(c, c - 1);
        }
        self.e.move_cell(lay.t - 1, block_end - 1); // back to its cell (now right after the shifted block)
        self.e.goto(lay.cnt - 1);
        self.e.set_pos(lay.cnt);
        self.e.end_while(lay.cnt);
        if pre > 0 {
            for &c in cells.iter() {
                self.e.move_cell(c, c - pre);
            }
            let p = self.e.pos();
            self.e.set_pos(p + pre);
        }
        // Hop back up the blocks.
        let mut back = vec![lay.hops];
        if !is_store {
            back.extend(&data);
        }
        self.e.begin_while(lay.hops);
        self.e.dec(lay.hops);
        self.hop(b, &back);
        self.e.goto(lay.hops + b);
        self.e.set_pos(lay.hops);
        self.e.end_while(lay.hops);
        // Block 0 -> guard frame.
        let back_g: Vec<Cell> = if is_store { vec![] } else { data.clone() };
        self.hop(b, &back_g);
        let p = self.e.pos();
        self.e.set_pos(p - b);
        // Hop up through the frames until the home frame.
        self.e.inc(lay.nothome);
        self.e.begin_while(lay.nothome);
        self.e.dec(lay.nothome);
        self.hop(f, &back_g);
        self.e.goto(lay.nothome + f);
        self.e.set_pos(lay.nothome);
        self.e.begin_if_zero(lay.home);
        self.e.inc(lay.nothome);
        self.e.end_if_zero(lay.home);
        self.e.end_while(lay.nothome);
        self.e.dec(lay.home);
        self.e.end_while(req);
    }

    // -----------------------------------------------------------------------
    // Terminators and phis
    // -----------------------------------------------------------------------

    fn block_id(&self, name: &Name) -> Result<u16> {
        self.fp().block_ids.get(name).copied().ok_or_else(|| anyhow!("unknown block %{}", name))
    }

    /// Emit the phi moves for the edge `from -> to`, then set the pc to `to`.
    fn branch_to(&mut self, from: &Name, to: &Name) -> Result<()> {
        let f = self.func();
        let dest = f.basic_blocks.iter().find(|b| &b.name == to).ok_or_else(|| anyhow!("unknown block %{}", to))?;
        let m = self.sc.mark();
        let mut pending: Vec<(Name, Work)> = Vec::new();
        for inst in &dest.instrs {
            if let Instruction::Phi(phi) = inst {
                let (_, size) = self.slot(&phi.dest)?;
                let w = Work::alloc(&mut self.sc, size);
                let (val, _) = phi
                    .incoming_values
                    .iter()
                    .find(|(_, b)| b == from)
                    .ok_or_else(|| anyhow!("phi %{} has no incoming value from %{}", phi.dest, from))?;
                if self.ti.is_scalar(&phi.to_type) {
                    self.load_operand(val, w)?;
                } else {
                    self.load_operand_bytes(val, 0, w)?;
                }
                pending.push((phi.dest.clone(), w));
            } else {
                break;
            }
        }
        for (name, w) in pending {
            self.store_result(&name, w)?;
        }
        self.sc.release(m);
        let id = self.block_id(to)?;
        self.set_pc(0, id);
        Ok(())
    }

    fn emit_terminator(&mut self, bb: &llvm_ir::BasicBlock, _seg: &Segment) -> Result<()> {
        match &bb.term {
            Terminator::Ret(r) => self.emit_ret(r),
            Terminator::Br(br) => self.branch_to(&bb.name, &br.dest),
            Terminator::CondBr(cb) => {
                let m = self.sc.mark();
                let c = Work::alloc(&mut self.sc, 1);
                self.load_operand(&cb.condition, c)?;
                let flag = self.sc.alloc(1);
                self.e.begin_if_else(c.limb(0), flag);
                self.branch_to(&bb.name, &cb.false_dest)?;
                self.e.else_if_else(c.limb(0), flag);
                self.branch_to(&bb.name, &cb.true_dest)?;
                self.e.end_if_else(flag);
                self.e.clear(c.limb(0));
                self.sc.release(m);
                Ok(())
            }
            Terminator::Switch(sw) => {
                let bits = self.op_bits(&sw.operand)?;
                let n = limbs(bits);
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, n);
                self.load_operand(&sw.operand, w)?;
                let done = Work::alloc(&mut self.sc, 1);
                for (c, dest) in &sw.dests {
                    let m2 = self.sc.mark();
                    let cw = Work::alloc(&mut self.sc, n);
                    let v = self.ce().scalar(c)?;
                    load_const(&mut self.e, cw, v);
                    let flag = Work::alloc(&mut self.sc, 1);
                    eq(&mut self.e, &mut self.sc, w, cw, flag.limb(0));
                    clear_work(&mut self.e, cw);
                    let f2 = self.sc.alloc(1);
                    self.e.begin_if_nonzero(flag.limb(0), f2);
                    self.e.inc(done.limb(0));
                    self.branch_to(&bb.name, dest)?;
                    self.e.end_if_nonzero(f2);
                    self.e.clear(flag.limb(0));
                    self.sc.release(m2);
                }
                self.e.begin_if_zero(done.limb(0));
                self.branch_to(&bb.name, &sw.default_dest)?;
                self.e.end_if_zero(done.limb(0));
                self.e.clear(done.limb(0));
                clear_work(&mut self.e, w);
                self.sc.release(m);
                Ok(())
            }
            Terminator::Unreachable(_) => {
                self.e.clear(self.lay.active);
                Ok(())
            }
            t => bail!("unsupported terminator {}", t),
        }
    }

    // -----------------------------------------------------------------------
    // Instructions
    // -----------------------------------------------------------------------

    fn const_of(&self, op: &Operand) -> Option<u64> {
        match op {
            Operand::ConstantOperand(c) => self.ce().scalar(c).ok(),
            _ => None,
        }
    }

    fn emit_instr(&mut self, bb: &llvm_ir::BasicBlock, i: usize) -> Result<()> {
        let inst = &bb.instrs[i];
        match inst {
            Instruction::Add(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Add),
            Instruction::Sub(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Sub),
            Instruction::Mul(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Mul),
            Instruction::UDiv(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::UDiv),
            Instruction::SDiv(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::SDiv),
            Instruction::URem(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::URem),
            Instruction::SRem(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::SRem),
            Instruction::And(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::And),
            Instruction::Or(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Or),
            Instruction::Xor(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Xor),
            Instruction::Shl(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::Shl),
            Instruction::LShr(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::LShr),
            Instruction::AShr(x) => self.binop(&x.operand0, &x.operand1, &x.dest, BinOp::AShr),
            Instruction::ICmp(x) => self.icmp(x),
            Instruction::ZExt(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::SExt(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::SExt),
            Instruction::Trunc(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::Trunc),
            Instruction::PtrToInt(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::IntToPtr(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::BitCast(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::AddrSpaceCast(x) => self.cast(&x.operand, &x.to_type, &x.dest, Cast::ZExt),
            Instruction::Freeze(x) => {
                let ty = self.op_type(&x.operand);
                self.cast(&x.operand, &ty, &x.dest, Cast::ZExt)
            }
            Instruction::Select(x) => self.select(x),
            Instruction::Phi(_) => Ok(()),
            Instruction::GetElementPtr(x) => self.gep(x),
            Instruction::Alloca(x) => self.alloca(bb, x),
            Instruction::ExtractValue(x) => self.extract_value(x),
            Instruction::InsertValue(x) => self.insert_value(x),
            Instruction::Call(c) => self.inline_call(c),
            Instruction::Load(_) | Instruction::Store(_) | Instruction::VAArg(_) => Ok(()), // handled as segment ends
            other => bail!("unsupported instruction {}", other),
        }
    }

    fn binop(&mut self, a: &Operand, b: &Operand, dest: &Name, op: BinOp) -> Result<()> {
        let bits = self.op_bits(a)?;
        let n = limbs(bits);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        self.load_operand(a, wa)?;
        let cb = self.const_of(b);
        let mut result = wa;
        match op {
            BinOp::Add | BinOp::Sub => {
                if let Some(c) = cb {
                    let v = if op == BinOp::Sub { c.wrapping_neg() } else { c };
                    add_const(&mut self.e, &mut self.sc, wa, v);
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    if op == BinOp::Add {
                        add_work(&mut self.e, wa, wb);
                    } else {
                        sub_work(&mut self.e, wa, wb);
                    }
                }
            }
            BinOp::Mul => {
                let res = Work::alloc(&mut self.sc, n);
                if let Some(c) = cb {
                    mul_const(&mut self.e, &mut self.sc, wa, c, res);
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    mul_work(&mut self.e, &mut self.sc, wa, wb, res);
                }
                result = res;
            }
            BinOp::UDiv | BinOp::URem | BinOp::SDiv | BinOp::SRem => {
                let wb = Work::alloc(&mut self.sc, n);
                self.load_operand(b, wb)?;
                let q = Work::alloc(&mut self.sc, n);
                let r = Work::alloc(&mut self.sc, n);
                if matches!(op, BinOp::UDiv | BinOp::URem) {
                    udivrem(&mut self.e, &mut self.sc, wa, wb, q, r);
                } else {
                    sdivrem(&mut self.e, &mut self.sc, wa, wb, q, r);
                }
                if matches!(op, BinOp::UDiv | BinOp::SDiv) {
                    clear_work(&mut self.e, r);
                    result = q;
                } else {
                    clear_work(&mut self.e, q);
                    result = r;
                }
            }
            BinOp::And | BinOp::Or | BinOp::Xor => {
                let kind = match op {
                    BinOp::And => 0,
                    BinOp::Or => 1,
                    _ => 2,
                };
                if let Some(c) = cb {
                    bitwise_const(&mut self.e, &mut self.sc, wa, c, kind);
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    bitwise(&mut self.e, &mut self.sc, wa, wb, kind);
                }
            }
            BinOp::Shl | BinOp::LShr | BinOp::AShr => {
                if let Some(c) = cb {
                    let k = c as u32;
                    match op {
                        BinOp::Shl => shl_const(&mut self.e, &mut self.sc, wa, k),
                        BinOp::LShr => lshr_const(&mut self.e, &mut self.sc, wa, k),
                        _ => ashr_const(&mut self.e, &mut self.sc, wa, k),
                    }
                } else {
                    let wb = Work::alloc(&mut self.sc, n);
                    self.load_operand(b, wb)?;
                    let kind = match op {
                        BinOp::Shl => 0,
                        BinOp::LShr => 1,
                        _ => 2,
                    };
                    shift_var(&mut self.e, &mut self.sc, wa, wb, kind);
                }
            }
        }
        if bits % 8 != 0 {
            trunc_bits(&mut self.e, &mut self.sc, result, bits);
        }
        self.store_result(dest, result)?;
        self.sc.release(m);
        Ok(())
    }

    fn icmp(&mut self, x: &instruction::ICmp) -> Result<()> {
        let bits = self.op_bits(&x.operand0)?;
        let n = limbs(bits);
        let m = self.sc.mark();
        let wa = Work::alloc(&mut self.sc, n);
        let wb = Work::alloc(&mut self.sc, n);
        self.load_operand(&x.operand0, wa)?;
        self.load_operand(&x.operand1, wb)?;
        let flag = Work::alloc(&mut self.sc, 1);
        let fc = flag.limb(0);
        use IntPredicate::*;
        let signed = matches!(x.predicate, SGT | SGE | SLT | SLE);
        if signed {
            if bits % 8 != 0 {
                // Sign bit of a sub-byte value (i1): move it to the top of the limb.
                let k = 8 - (bits % 8);
                shl_const(&mut self.e, &mut self.sc, wa, k);
                shl_const(&mut self.e, &mut self.sc, wb, k);
            }
            flip_sign(&mut self.e, wa);
            flip_sign(&mut self.e, wb);
        }
        match x.predicate {
            EQ => eq(&mut self.e, &mut self.sc, wa, wb, fc),
            NE => {
                let t = self.sc.alloc(1);
                eq(&mut self.e, &mut self.sc, wa, wb, t);
                self.e.inc(fc);
                self.e.move_cell_neg(t, fc);
            }
            ULT | SLT => ult(&mut self.e, &mut self.sc, wa, wb, fc),
            UGT | SGT => ult(&mut self.e, &mut self.sc, wb, wa, fc),
            ULE | SLE => {
                let t = self.sc.alloc(1);
                ult(&mut self.e, &mut self.sc, wb, wa, t);
                self.e.inc(fc);
                self.e.move_cell_neg(t, fc);
            }
            UGE | SGE => {
                let t = self.sc.alloc(1);
                ult(&mut self.e, &mut self.sc, wa, wb, t);
                self.e.inc(fc);
                self.e.move_cell_neg(t, fc);
            }
        }
        clear_work(&mut self.e, wa);
        clear_work(&mut self.e, wb);
        self.store_result(&x.dest, flag)?;
        self.sc.release(m);
        Ok(())
    }

    fn cast(&mut self, op: &Operand, to: &TypeRef, dest: &Name, kind: Cast) -> Result<()> {
        let from_bits = self.op_bits(op)?;
        let to_bits = self.ti.scalar_bits(to)?;
        let nf = limbs(from_bits);
        let nt = limbs(to_bits);
        let m = self.sc.mark();
        let w = match kind {
            Cast::ZExt => {
                let w = Work::alloc(&mut self.sc, nt.max(nf));
                self.load_operand(op, w.truncated(nf))?;
                if to_bits < from_bits {
                    trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                }
                w.truncated(nt)
            }
            Cast::SExt => {
                let src = Work::alloc(&mut self.sc, nf);
                self.load_operand(op, src)?;
                let w = Work::alloc(&mut self.sc, nt);
                sext_bits(&mut self.e, &mut self.sc, src, from_bits, w);
                if to_bits % 8 != 0 {
                    trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                }
                w
            }
            Cast::Trunc => {
                let w = Work::alloc(&mut self.sc, nf);
                self.load_operand(op, w)?;
                trunc_bits(&mut self.e, &mut self.sc, w, to_bits);
                w.truncated(nt)
            }
        };
        self.store_result(dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn select(&mut self, x: &instruction::Select) -> Result<()> {
        let ty = self.op_type(&x.true_value);
        let size = self.ti.size_of(&ty)?;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        let scalar = self.ti.is_scalar(&ty);
        if scalar {
            self.load_operand(&x.false_value, w)?;
        } else {
            self.load_operand_bytes(&x.false_value, 0, w)?;
        }
        let c = Work::alloc(&mut self.sc, 1);
        self.load_operand(&x.condition, c)?;
        let flag = self.sc.alloc(1);
        self.e.begin_if_nonzero(c.limb(0), flag);
        clear_work(&mut self.e, w);
        if scalar {
            self.load_operand(&x.true_value, w)?;
        } else {
            self.load_operand_bytes(&x.true_value, 0, w)?;
        }
        self.e.end_if_nonzero(flag);
        self.e.clear(c.limb(0));
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn gep(&mut self, x: &instruction::GetElementPtr) -> Result<()> {
        let n = self.ptr_limbs();
        let pbits = 8 * n as u32;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        self.load_operand(&x.address, w)?;
        let mut ty = x.source_element_type.clone();
        for (k, idx) in x.indices.iter().enumerate() {
            let scale: usize;
            if k == 0 {
                scale = self.ti.size_of(&ty)?;
            } else {
                let rty = self.ti.resolve(&ty)?.clone();
                match rty {
                    Type::StructType { .. } => {
                        let c = self.const_of(idx).ok_or_else(|| anyhow!("non-constant struct index in GEP"))?;
                        let (ety, off) = self.ti.element(&ty, c as usize)?;
                        add_const(&mut self.e, &mut self.sc, w, off as u64);
                        ty = ety;
                        continue;
                    }
                    Type::ArrayType { .. } => {
                        let ety = self.ti.array_element(&ty)?;
                        scale = self.ti.size_of(&ety)?;
                        ty = ety;
                    }
                    t => bail!("GEP into unsupported type {}", t),
                }
            }
            if let Some(c) = self.const_of(idx) {
                let ibits = self.op_bits(idx)?;
                let sc_val = (sext(c, ibits) as i64).wrapping_mul(scale as i64);
                add_const(&mut self.e, &mut self.sc, w, mask_bits(sc_val as u64, pbits));
            } else {
                let ibits = self.op_bits(idx)?;
                let ni = limbs(ibits);
                let m2 = self.sc.mark();
                let iw = Work::alloc(&mut self.sc, ni);
                self.load_operand(idx, iw)?;
                let ext = Work::alloc(&mut self.sc, n);
                if ni >= n {
                    move_work(&mut self.e, iw.truncated(n), ext);
                    clear_work(&mut self.e, iw);
                } else {
                    sext_bits(&mut self.e, &mut self.sc, iw, ibits, ext);
                }
                if scale == 1 {
                    add_work(&mut self.e, w, ext);
                } else {
                    let prod = Work::alloc(&mut self.sc, n);
                    mul_const(&mut self.e, &mut self.sc, ext, scale as u64, prod);
                    add_work(&mut self.e, w, prod);
                }
                self.sc.release(m2);
            }
        }
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn alloca(&mut self, bb: &llvm_ir::BasicBlock, x: &instruction::Alloca) -> Result<()> {
        if bb.name != self.func().basic_blocks[0].name {
            bail!("alloca outside the entry block (dynamic allocation) is not supported");
        }
        let count = match &x.num_elements {
            Operand::ConstantOperand(c) => self.ce().scalar(c)?,
            _ => bail!("alloca with a variable element count is not supported"),
        };
        let size = self.ti.size_of(&x.allocated_type)? as u64 * count;
        let align = (x.alignment as u64).max(2);
        let size = (size + align - 1) / align * align;
        let n = self.ptr_limbs();
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        load_packed(&mut self.e, &mut self.sc, self.lay.msp, w);
        add_const(&mut self.e, &mut self.sc, w, mask_bits(size.wrapping_neg(), 8 * n as u32));
        let w2 = Work::alloc(&mut self.sc, n);
        copy_work(&mut self.e, &mut self.sc, w, w2);
        self.store_to_cells(self.lay.msp, n, w);
        self.store_result(&x.dest, w2)?;
        self.sc.release(m);
        Ok(())
    }

    fn agg_field(&self, ty: &TypeRef, indices: &[u32]) -> Result<(TypeRef, usize)> {
        let mut t = ty.clone();
        let mut off = 0;
        for &i in indices {
            let (et, o) = self.ti.element(&t, i as usize)?;
            off += o;
            t = et;
        }
        Ok((t, off))
    }

    fn extract_value(&mut self, x: &instruction::ExtractValue) -> Result<()> {
        let ty = self.op_type(&x.aggregate);
        let (fty, off) = self.agg_field(&ty, &x.indices)?;
        let size = self.ti.size_of(&fty)?;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_operand_bytes(&x.aggregate, off, w)?;
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn insert_value(&mut self, x: &instruction::InsertValue) -> Result<()> {
        let ty = self.op_type(&x.aggregate);
        let size = self.ti.size_of(&ty)?;
        let (fty, off) = self.agg_field(&ty, &x.indices)?;
        let fsize = self.ti.size_of(&fty)?;
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, size);
        self.load_operand_bytes(&x.aggregate, 0, w)?;
        for k in 0..fsize {
            self.e.clear(w.limb(off + k));
        }
        let fw = Work::alloc(&mut self.sc, fsize);
        if self.ti.is_scalar(&fty) {
            self.load_operand(&x.element, fw)?;
        } else {
            self.load_operand_bytes(&x.element, 0, fw)?;
        }
        for k in 0..fsize {
            self.e.move_cell(fw.limb(k), w.limb(off + k));
        }
        self.store_result(&x.dest, w)?;
        self.sc.release(m);
        Ok(())
    }

    fn inline_call(&mut self, c: &instruction::Call) -> Result<()> {
        let name = callee_name(c).ok_or_else(|| anyhow!("indirect call treated as inline"))?;
        let args: Vec<&Operand> = c.arguments.iter().map(|(a, _)| a).collect();
        match name.as_str() {
            "__llvbf_putc" => {
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, 1);
                self.load_operand(args[0], w)?;
                self.e.out(w.limb(0));
                self.e.clear(w.limb(0));
                self.sc.release(m);
                Ok(())
            }
            "__llvbf_getc" => {
                let dest = c.dest.as_ref().ok_or_else(|| anyhow!("__llvbf_getc without result"))?;
                let (cell, size) = self.slot(dest)?;
                for k in 0..size {
                    self.e.clear(cell + k as Cell);
                }
                self.e.input(cell);
                Ok(())
            }
            "__llvbf_exit" => {
                self.e.clear(self.lay.active);
                Ok(())
            }
            n if n.starts_with("llvm.expect.") => {
                let dest = c.dest.as_ref().unwrap();
                let ty = self.op_type(args[0]);
                self.cast(args[0], &ty, dest, Cast::ZExt)
            }
            n if n.starts_with("llvm.abs.") => {
                let dest = c.dest.as_ref().unwrap();
                let bits = self.op_bits(args[0])?;
                let n = limbs(bits);
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, n);
                self.load_operand(args[0], w)?;
                let s = self.sc.alloc(1);
                sign_bit(&mut self.e, &mut self.sc, w, s);
                self.e.begin_while(s);
                self.e.dec(s);
                negate_work(&mut self.e, &mut self.sc, w);
                self.e.end_while(s);
                self.store_result(dest, w)?;
                self.sc.release(m);
                Ok(())
            }
            n if n.starts_with("llvm.smax.") || n.starts_with("llvm.smin.") || n.starts_with("llvm.umax.") || n.starts_with("llvm.umin.") => {
                let dest = c.dest.as_ref().unwrap();
                let bits = self.op_bits(args[0])?;
                let nl = limbs(bits);
                let m = self.sc.mark();
                let wa = Work::alloc(&mut self.sc, nl);
                let wb = Work::alloc(&mut self.sc, nl);
                self.load_operand(args[0], wa)?;
                self.load_operand(args[1], wb)?;
                let signed = n.starts_with("llvm.s");
                let want_max = n.contains("max");
                if signed {
                    flip_sign(&mut self.e, wa);
                    flip_sign(&mut self.e, wb);
                }
                // flag = (a < b)
                let flag = Work::alloc(&mut self.sc, 1);
                ult(&mut self.e, &mut self.sc, wa, wb, flag.limb(0));
                if signed {
                    flip_sign(&mut self.e, wa);
                    flip_sign(&mut self.e, wb);
                }
                // result = want_max ? (a<b ? b : a) : (a<b ? a : b)
                let res = Work::alloc(&mut self.sc, nl);
                let f2 = self.sc.alloc(1);
                self.e.begin_if_else(flag.limb(0), f2);
                // a >= b
                if want_max {
                    move_work(&mut self.e, wa, res);
                    clear_work(&mut self.e, wb);
                } else {
                    move_work(&mut self.e, wb, res);
                    clear_work(&mut self.e, wa);
                }
                self.e.else_if_else(flag.limb(0), f2);
                if want_max {
                    move_work(&mut self.e, wb, res);
                    clear_work(&mut self.e, wa);
                } else {
                    move_work(&mut self.e, wa, res);
                    clear_work(&mut self.e, wb);
                }
                self.e.end_if_else(f2);
                self.e.clear(flag.limb(0));
                self.store_result(dest, res)?;
                self.sc.release(m);
                Ok(())
            }
            n if n.starts_with("llvm.bswap.") => {
                let dest = c.dest.as_ref().unwrap();
                let bits = self.op_bits(args[0])?;
                let nl = limbs(bits);
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, nl);
                self.load_operand(args[0], w)?;
                let r = Work::alloc(&mut self.sc, nl);
                for k in 0..nl {
                    self.e.move_cell(w.limb(k), r.limb(nl - 1 - k));
                }
                self.store_result(dest, r)?;
                self.sc.release(m);
                Ok(())
            }
            n if n.contains(".with.overflow.") => self.with_overflow(c, n),
            n if n.starts_with("llvm.fshl.") || n.starts_with("llvm.fshr.") => {
                let dest = c.dest.as_ref().unwrap();
                let bits = self.op_bits(args[0])?;
                let nl = limbs(bits);
                if bits % 8 != 0 {
                    bail!("funnel shift on non-byte width i{}", bits);
                }
                let left = n.starts_with("llvm.fshl.");
                let m = self.sc.mark();
                let wa = Work::alloc(&mut self.sc, nl);
                let wb = Work::alloc(&mut self.sc, nl);
                self.load_operand(args[0], wa)?;
                self.load_operand(args[1], wb)?;
                if let Some(cst) = self.const_of(args[2]) {
                    let k = (cst % bits as u64) as u32;
                    if k != 0 {
                        if left {
                            shl_const(&mut self.e, &mut self.sc, wa, k);
                            lshr_const(&mut self.e, &mut self.sc, wb, bits - k);
                        } else {
                            lshr_const(&mut self.e, &mut self.sc, wb, k);
                            shl_const(&mut self.e, &mut self.sc, wa, bits - k);
                        }
                        add_work(&mut self.e, wa, wb);
                    } else if left {
                        clear_work(&mut self.e, wb);
                    } else {
                        clear_work(&mut self.e, wa);
                        move_work(&mut self.e, wb, wa);
                    }
                } else {
                    // k = c mod bits; the other shift amount is bits - k (full shift clears when k == 0).
                    let kb = limbs(self.op_bits(args[2])?);
                    let kw = Work::alloc(&mut self.sc, kb);
                    self.load_operand(args[2], kw)?;
                    bitwise_const(&mut self.e, &mut self.sc, kw, (bits - 1) as u64, 0);
                    let k2 = Work::alloc(&mut self.sc, kb);
                    load_const(&mut self.e, k2, bits as u64);
                    let kc = Work::alloc(&mut self.sc, kb);
                    copy_work(&mut self.e, &mut self.sc, kw, kc);
                    sub_work(&mut self.e, k2, kc);
                    // if k == 0 then k2 = bits: shifting by bits must clear; handle via the loop (bits iterations clear everything).
                    if left {
                        shift_var(&mut self.e, &mut self.sc, wa, kw, 0);
                        shift_var(&mut self.e, &mut self.sc, wb, k2, 1);
                    } else {
                        shift_var(&mut self.e, &mut self.sc, wb, kw, 1);
                        shift_var(&mut self.e, &mut self.sc, wa, k2, 0);
                    }
                    add_work(&mut self.e, wa, wb);
                }
                self.store_result(dest, wa)?;
                self.sc.release(m);
                Ok(())
            }
            n if n.starts_with("llvm.ctpop.") || n.starts_with("llvm.ctlz.") || n.starts_with("llvm.cttz.") => {
                let dest = c.dest.as_ref().unwrap();
                let bits = self.op_bits(args[0])?;
                let nl = limbs(bits);
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, nl);
                self.load_operand(args[0], w)?;
                let res = Work::alloc(&mut self.sc, nl);
                let bitcells = self.sc.alloc(8 * nl);
                for i in 0..nl {
                    to_bits(&mut self.e, &mut self.sc, w.limb(i), bitcells + 8 * i as Cell);
                }
                if n.starts_with("llvm.ctpop.") {
                    for k in 0..8 * nl {
                        let b = bitcells + k as Cell;
                        self.e.begin_while(b);
                        self.e.dec(b);
                        inc_with_carry(&mut self.e, res, 0);
                        self.e.end_while(b);
                    }
                } else {
                    // count zeros from one end until the first one bit
                    let counting = self.sc.alloc(1);
                    self.e.inc(counting);
                    let order: Vec<usize> = if n.starts_with("llvm.cttz.") { (0..8 * nl).collect() } else { (0..8 * nl).rev().collect() };
                    for k in order {
                        let b = bitcells + k as Cell;
                        // if bit: counting = 0; else if counting: res++
                        let t = self.sc.alloc(1);
                        self.e.begin_while(b);
                        self.e.dec(b);
                        self.e.clear(counting);
                        self.e.inc(t);
                        self.e.end_while(b);
                        // t == 1 means the bit was set (already handled); if t == 0 and counting: res++
                        self.e.inc(t);
                        self.e.begin_while(t);
                        self.e.dec(t);
                        // runs 1 or 2 times: on the first pass do nothing but on a single pass (bit clear) count
                        self.e.begin_while(t);
                        self.e.dec(t);
                        self.e.inc(t); // keep t = 1 to exit outer loop after this iteration with marker
                        self.e.dec(t);
                        self.e.end_while(t);
                        self.e.end_while(t);
                        // The above cleared t; emulate "if bit was clear" via a fresh flag instead:
                        self.sc.release(t);
                    }
                    // Simpler and correct implementation: recompute with explicit flags.
                    clear_work(&mut self.e, res);
                    self.e.clear(counting);
                    self.e.inc(counting);
                    for i in 0..nl {
                        let _ = i;
                    }
                    // Re-extract bits (they were consumed above).
                    let w2 = Work::alloc(&mut self.sc, nl);
                    self.load_operand(args[0], w2)?;
                    for i in 0..nl {
                        to_bits(&mut self.e, &mut self.sc, w2.limb(i), bitcells + 8 * i as Cell);
                    }
                    let order: Vec<usize> = if n.starts_with("llvm.cttz.") { (0..8 * nl).collect() } else { (0..8 * nl).rev().collect() };
                    for k in order {
                        let b = bitcells + k as Cell;
                        let f = self.sc.alloc(1);
                        // f = counting (copy)
                        let tmp = self.sc.alloc(1);
                        self.e.copy_cell(counting, f, tmp);
                        // if bit set: counting = 0, f = 0
                        self.e.begin_while(b);
                        self.e.dec(b);
                        self.e.clear(counting);
                        self.e.clear(f);
                        self.e.end_while(b);
                        // if f: res++
                        self.e.begin_while(f);
                        self.e.dec(f);
                        inc_with_carry(&mut self.e, res, 0);
                        self.e.end_while(f);
                    }
                    self.e.clear(counting);
                }
                self.store_result(dest, res)?;
                self.sc.release(m);
                Ok(())
            }
            n if is_inline_intrinsic(n) => Ok(()),
            n => bail!("unsupported call to {}", n),
        }
    }

    fn with_overflow(&mut self, c: &instruction::Call, name: &str) -> Result<()> {
        let dest = c.dest.as_ref().unwrap();
        let args: Vec<&Operand> = c.arguments.iter().map(|(a, _)| a).collect();
        let bits = self.op_bits(args[0])?;
        let n = limbs(bits);
        let (cell, size) = self.slot(dest)?;
        let rty = self.module.type_of(c);
        let (_, flag_off) = self.ti.element(&rty, 1)?;
        for k in 0..size {
            self.e.clear(cell + k as Cell);
        }
        let m = self.sc.mark();
        let flag = Work::alloc(&mut self.sc, 1);
        let fc = flag.limb(0);
        if name.starts_with("llvm.uadd.") {
            let wa = Work::alloc(&mut self.sc, n + 1);
            let wb = Work::alloc(&mut self.sc, n);
            self.load_operand(args[0], wa.truncated(n))?;
            self.load_operand(args[1], wb)?;
            add_work(&mut self.e, wa, wb);
            self.e.move_cell(wa.limb(n), fc);
            self.store_to_cells(cell, n, wa.truncated(n));
        } else if name.starts_with("llvm.usub.") {
            let wa = Work::alloc(&mut self.sc, n);
            let wb = Work::alloc(&mut self.sc, n);
            self.load_operand(args[0], wa)?;
            self.load_operand(args[1], wb)?;
            ult(&mut self.e, &mut self.sc, wa, wb, fc);
            sub_work(&mut self.e, wa, wb);
            self.store_to_cells(cell, n, wa);
        } else if name.starts_with("llvm.sadd.") || name.starts_with("llvm.ssub.") {
            let wa = Work::alloc(&mut self.sc, n);
            let wb = Work::alloc(&mut self.sc, n);
            self.load_operand(args[0], wa)?;
            self.load_operand(args[1], wb)?;
            let sa = self.sc.alloc(1);
            let sb = self.sc.alloc(1);
            let sr = self.sc.alloc(1);
            sign_bit(&mut self.e, &mut self.sc, wa, sa);
            sign_bit(&mut self.e, &mut self.sc, wb, sb);
            if name.starts_with("llvm.sadd.") {
                add_work(&mut self.e, wa, wb);
            } else {
                sub_work(&mut self.e, wa, wb);
                // for subtraction, overflow iff sign(a) != sign(b) and sign(res) != sign(a): flip sb
                self.e.inc(sb);
                let t = self.sc.alloc(1);
                self.e.move_cell(sb, t);
                // t in {1,2}: sb' = (t == 1)
                self.e.begin_while(t);
                self.e.dec(t);
                self.e.inc(sb);
                self.e.begin_while(t);
                self.e.dec(t);
                self.e.dec(sb);
                self.e.end_while(t);
                self.e.end_while(t);
            }
            sign_bit(&mut self.e, &mut self.sc, wa, sr);
            // same = (sa == sb); diff = (sr != sa); ovf = same && diff
            let t = self.sc.alloc(1);
            let u = self.sc.alloc(1);
            self.e.copy_cell(sa, t, u);
            self.e.move_cell(sb, t); // t in {0,1,2}, same iff t != 1
            self.e.move_cell(sa, u);
            self.e.move_cell(sr, u); // u in {0,1,2}, diff iff u == 1
            // ovf = (t != 1) && (u == 1)
            let v = self.sc.alloc(1);
            self.e.begin_while(u);
            self.e.dec(u);
            self.e.inc(v);
            self.e.begin_while(u);
            self.e.dec(u);
            self.e.dec(v);
            self.e.end_while(u);
            self.e.end_while(u);
            // v = diff. now t: compute same into fc: fc = v if t != 1
            let s = self.sc.alloc(1);
            self.e.inc(s);
            self.e.begin_while(t);
            self.e.dec(t);
            self.e.dec(s);
            self.e.begin_while(t);
            self.e.dec(t);
            self.e.inc(s);
            self.e.end_while(t);
            self.e.end_while(t);
            // s = same
            self.e.begin_while(s);
            self.e.dec(s);
            self.e.move_cell(v, fc);
            self.e.end_while(s);
            self.e.clear(v);
            self.store_to_cells(cell, n, wa);
        } else if name.starts_with("llvm.umul.") {
            let wa = Work::alloc(&mut self.sc, 2 * n);
            let wb = Work::alloc(&mut self.sc, 2 * n);
            self.load_operand(args[0], wa.truncated(n))?;
            self.load_operand(args[1], wb.truncated(n))?;
            let res = Work::alloc(&mut self.sc, 2 * n);
            mul_work(&mut self.e, &mut self.sc, wa, wb, res);
            let hi = Work { base: res.limb(n), n };
            is_nonzero_destructive(&mut self.e, hi, fc);
            self.store_to_cells(cell, n, res.truncated(n));
        } else {
            bail!("unsupported intrinsic {}", name);
        }
        self.e.move_cell(fc, cell + flag_off as Cell);
        self.sc.release(m);
        Ok(())
    }
}

fn sext(v: u64, bits: u32) -> u64 {
    if bits >= 64 {
        v
    } else {
        (((v << (64 - bits)) as i64) >> (64 - bits)) as u64
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    UDiv,
    SDiv,
    URem,
    SRem,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
}

#[derive(Clone, Copy, PartialEq)]
enum Cast {
    ZExt,
    SExt,
    Trunc,
}
