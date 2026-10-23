//! Code generation: lowers a planned module to Brainfuck.
//!
//! See README.md for the tape layout, the dispatch loop and the memory system.
//! This file holds the frame layout, the program skeleton (global data
//! initialization, the dispatch loop) and operand access helpers. The lowering
//! of individual constructs lives in the submodules:
//!
//! * `calls`      - calls, returns and the frame shift,
//! * `memory`     - load/store requests and the shared memory routines,
//! * `control`    - terminators and phi copies,
//! * `instr`      - arithmetic, comparisons, casts, GEP, select, ...
//! * `intrinsics` - builtins and LLVM intrinsics lowered inline.

mod calls;
mod control;
mod instr;
mod intrinsics;
mod memory;

use crate::arith::*;
use crate::emit::{Cell, Emitter};
use crate::ir::*;
use crate::plan::*;
use anyhow::{anyhow, bail, Context as _, Result};
use instr::Cast;

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
        Options {
            mem_size: 16384,
            comments: false,
            entry: "main".to_string(),
        }
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
    m: &'a Module,
    plan: &'a Plan,
    lay: Layout,
    opts: &'a Options,
    e: Emitter,
    sc: Scratch,
    /// Entry segment id of each function, by `FuncId` (`None` for declarations).
    func_entry: Vec<Option<u16>>,
    /// Index into `plan.funcs` of the function being emitted.
    cur: usize,
}

/// Compile a module to Brainfuck.
pub fn compile(m: &Module, opts: &Options) -> Result<Vec<u8>> {
    if !opts.mem_size.is_multiple_of(256) || opts.mem_size == 0 || opts.mem_size > 65536 {
        bail!("memory size must be a positive multiple of 256 and at most 65536");
    }
    let plan = plan_module(m, opts.mem_size, &opts.entry)?;
    // Pass 1 measures the scratch requirement, pass 2 emits the final code.
    let (hw, _) = run_gen(m, &plan, opts, 4096)?;
    let (_, code) = run_gen(m, &plan, opts, hw + 4)?;
    Ok(code)
}

fn run_gen(
    m: &Module,
    plan: &Plan,
    opts: &Options,
    scratch_size: usize,
) -> Result<(usize, Vec<u8>)> {
    let lay = Layout::new(plan, m.types.ptr_bytes, scratch_size);
    let mut e = Emitter::new();
    e.comments = opts.comments;
    let sc = Scratch::new(lay.scratch);
    let func_entry = (0..m.functions.len())
        .map(|fid| plan.entry_of(fid))
        .collect();
    let mut g = Gen {
        m,
        plan,
        lay,
        opts,
        e,
        sc,
        func_entry,
        cur: 0,
    };
    g.emit_program()?;
    let used = g.sc.used();
    if used > scratch_size {
        bail!("internal error: scratch overflow");
    }
    Ok((used, g.e.into_code()))
}

impl<'a> Gen<'a> {
    fn types(&self) -> &'a TypeTable {
        &self.m.types
    }

    fn ce(&self) -> ConstEval<'_> {
        ConstEval {
            types: &self.m.types,
            global_addr: &self.plan.global_addr,
            func_entry: &self.func_entry,
        }
    }

    fn fp(&self) -> &'a FuncPlan {
        &self.plan.funcs[self.cur]
    }

    fn func(&self) -> &'a Function {
        &self.m.functions[self.fp().fid]
    }

    fn comment(&mut self, s: &str) {
        self.e.comment(s);
    }

    /// Cell and size of the slot of an SSA value in the current frame.
    fn slot(&self, v: Value) -> Result<(Cell, usize)> {
        let s = self
            .fp()
            .slots
            .get(&v)
            .ok_or_else(|| anyhow!("unknown value {}", v))?;
        Ok((self.slot_cell(s), s.size))
    }

    fn slot_cell(&self, s: &Slot) -> Cell {
        match s.kind {
            SlotKind::Param => self.lay.params + s.off as Cell,
            SlotKind::Local => self.lay.locals + s.off as Cell,
        }
    }

    fn op_bits(&self, op: &Operand) -> Result<u32> {
        self.types().bits(op.ty)
    }

    fn ptr_limbs(&self) -> usize {
        self.lay.ptr_bytes
    }

    /// Load a scalar operand into a zeroed work area (zero-extending if the work is wider).
    fn load_operand(&mut self, op: &Operand, w: Work) -> Result<()> {
        match &op.kind {
            OperandKind::Value(v) => {
                let (cell, size) = self.slot(*v)?;
                let n = w.n.min(size);
                load_packed(&mut self.e, &mut self.sc, cell, w.truncated(n));
            }
            OperandKind::Const(_) => {
                let v = self.ce().operand_scalar(op)?;
                load_const(&mut self.e, w, v);
            }
        }
        Ok(())
    }

    /// Bytes `off..off+n` of an operand's in-memory representation into a work area.
    fn load_operand_bytes(&mut self, op: &Operand, off: usize, w: Work) -> Result<()> {
        match &op.kind {
            OperandKind::Value(v) => {
                let (cell, size) = self.slot(*v)?;
                if off + w.n > size {
                    bail!("byte range out of slot bounds for {}", v);
                }
                load_packed(&mut self.e, &mut self.sc, cell + off as Cell, w);
            }
            OperandKind::Const(c) => {
                let bytes = self.ce().bytes(c, op.ty)?;
                for k in 0..w.n {
                    let b = bytes.get(off + k).copied().unwrap_or(0);
                    if b != 0 {
                        self.e.set_using(w.limb(k), b, w.limb(k) + 1);
                    }
                }
            }
        }
        Ok(())
    }

    /// Load an operand of any type into a work area sized for it.
    fn load_any(&mut self, op: &Operand, w: Work) -> Result<()> {
        if self.types().is_scalar(op.ty) {
            self.load_operand(op, w)
        } else {
            self.load_operand_bytes(op, 0, w)
        }
    }

    /// Store a work area into the slot of `v`, clearing the whole slot first.
    fn store_result(&mut self, v: Value, w: Work) -> Result<()> {
        let (cell, size) = self.slot(v)?;
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
                    v.push(SegRef {
                        id: s.id,
                        func: fi,
                        block: bi,
                        seg: si,
                    });
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
        let mut all: Vec<(u16, Option<usize>)> =
            vec![(self.plan.start_id, None), (self.plan.exit_id, None)];
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
            let entry_idx = self.plan.func_by_name(&self.opts.entry).unwrap();
            let entry_id = self.plan.funcs[entry_idx].entry_id;
            let f = lay.frame;
            // Parameters of main (argc, argv) are left zero.
            for i in 0..self.ptr_limbs() {
                self.e.clear(f + lay.msp + i as Cell);
                self.e
                    .copy_cell(lay.msp + i as Cell, f + lay.msp + i as Cell, lay.tmp);
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
        let bb = &f.blocks[block];
        let s = &fp.blocks[block].segments[seg];
        self.comment(&format!(
            "segment {} = {} / %{} / {}",
            s.id, fp.name, bb.name, seg
        ));
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
                        self.comment(&bb.instrs[i].text);
                    }
                    self.emit_instr(block, bb, i)
                        .with_context(|| format!("in {}: {}", fp.name, bb.instrs[i].text))?;
                }
                Step::VaSetup(i) => {
                    self.comment("variadic call: reserve spill area");
                    let (fn_ty, args) = match &bb.instrs[i].op {
                        Op::Call { fn_ty, args, .. } => (*fn_ty, args),
                        _ => unreachable!(),
                    };
                    let (_, total) = va_spill_layout(self.types(), fn_ty, args)?;
                    self.adjust_msp((total as u64).wrapping_neg());
                }
            }
            self.sc.release(m2);
        }
        match &s.end {
            SegEnd::Term => {
                if self.opts.comments {
                    self.comment(&bb.term_text);
                }
                self.emit_terminator(block, bb)
                    .with_context(|| format!("in {}: {}", fp.name, bb.term_text))?;
            }
            SegEnd::Call(i) => self
                .emit_call(bb, *i, s)
                .with_context(|| format!("in {}: {}", fp.name, bb.instrs[*i].text))?,
            SegEnd::Access(a) => self
                .emit_access(block, bb, *a, s)
                .with_context(|| format!("in {}: {}", fp.name, bb.instrs[a.instr].text))?,
        }
        self.sc.release(m);
        Ok(())
    }
}
