//! Calls and returns: argument passing and the logical frame shift.

use super::*;

impl<'a> Gen<'a> {
    /// Emitted at the start of the segment after a call: fetch the return value from
    /// the callee frame and release a variadic spill area.
    pub(super) fn finish_call(&mut self, bb: &Block, i: usize) -> Result<()> {
        let inst = &bb.instrs[i];
        let (fn_ty, args) = match &inst.op {
            Op::Call { fn_ty, args, .. } => (*fn_ty, args),
            _ => unreachable!(),
        };
        if let Some(dest) = inst.dest {
            let (cell, size) = self.slot(dest)?;
            let f = self.lay.frame;
            for k in 0..size {
                self.e.clear(cell + k as Cell);
                self.e
                    .move_cell(f + self.lay.ret + k as Cell, cell + k as Cell);
            }
        }
        if call_is_variadic(self.types(), fn_ty) {
            let (_, total) = va_spill_layout(self.types(), fn_ty, args)?;
            self.adjust_msp(total as u64);
        }
        Ok(())
    }

    /// MSP += delta (mod 2^ptrbits).
    pub(super) fn adjust_msp(&mut self, delta: u64) {
        let n = self.ptr_limbs();
        let m = self.sc.mark();
        let w = Work::alloc(&mut self.sc, n);
        load_packed(&mut self.e, &mut self.sc, self.lay.msp, w);
        add_const(&mut self.e, &mut self.sc, w, mask_bits(delta, 8 * n as u32));
        self.store_to_cells(self.lay.msp, n, w);
        self.sc.release(m);
    }

    pub(super) fn extra_slot(&self, block: usize, instr: usize) -> Result<(Cell, usize)> {
        let s = self
            .fp()
            .extra_slots
            .get(&(block, instr))
            .ok_or_else(|| anyhow!("internal error: missing hidden slot"))?;
        Ok((self.slot_cell(s), s.size))
    }

    /// (offset, size) of each parameter in the parameter area, for a list of parameter types.
    fn param_offsets(&self, tys: &[TypeId]) -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        let mut off = 0;
        for &t in tys {
            let s = self.types().size(t);
            v.push((off, s));
            off += s;
        }
        v
    }

    pub(super) fn emit_call(&mut self, bb: &Block, i: usize, seg: &Segment) -> Result<()> {
        let (callee, fn_ty, args) = match &bb.instrs[i].op {
            Op::Call {
                callee,
                fn_ty,
                args,
            } => (callee, *fn_ty, args),
            _ => unreachable!(),
        };
        let lay = self.lay.clone();
        let f = lay.frame;
        // Resolve the callee: a defined function (directly, or as the runtime function
        // that implements an intrinsic) or a function pointer.
        enum Target {
            Plan(usize),
            Pointer(Operand),
        }
        let mut args: Vec<Operand> = args.clone();
        let mut rewritten = false;
        let target = match callee {
            Callee::Intrinsic(name) => {
                let rt = intrinsic_runtime_call(name)
                    .ok_or_else(|| anyhow!("intrinsic {} is not a call", name))?;
                args.truncate(3);
                rewritten = true;
                Target::Plan(self.plan.func_by_name(rt).ok_or_else(|| {
                    anyhow!(
                        "call to undefined function @{} (link the llvbf runtime?)",
                        rt
                    )
                })?)
            }
            Callee::Direct(fid) => {
                let name = &self.m.functions[*fid].name;
                Target::Plan(
                    self.plan
                        .func_plan
                        .get(fid)
                        .copied()
                        .ok_or_else(|| anyhow!("call to undefined function @{}", name))?,
                )
            }
            Callee::Indirect(op) => Target::Pointer(op.clone()),
        };
        // Parameter layout: from the runtime function's own signature for rewritten
        // intrinsics (operands are zero-extended / truncated to the parameter widths),
        // otherwise from the call's function type.
        let sig_ty = match (&target, rewritten) {
            (Target::Plan(idx), true) => self.m.functions[self.plan.funcs[*idx].fid].ty,
            _ => fn_ty,
        };
        let (param_tys, variadic) = self.types().func_sig(sig_ty)?;
        let param_tys = param_tys.to_vec();
        if let Target::Plan(idx) = &target {
            let callee_fn = &self.m.functions[self.plan.funcs[*idx].fid];
            let (callee_params, callee_variadic) = self.types().func_sig(callee_fn.ty)?;
            if callee_variadic != variadic || callee_params.len() != param_tys.len() {
                bail!(
                    "call signature does not match the definition of @{}",
                    callee_fn.name
                );
            }
        }
        if args.len() < param_tys.len() || (!variadic && args.len() != param_tys.len()) {
            bail!(
                "argument count mismatch in call ({} vs {})",
                args.len(),
                param_tys.len()
            );
        }
        let offs = self.param_offsets(&param_tys);
        let fixed_size: usize = offs.iter().map(|(_, s)| *s).sum();
        args.truncate(param_tys.len());
        self.comment("call: pass arguments");
        for (k, arg) in args.iter().enumerate() {
            let (off, size) = offs[k];
            self.transfer_operand(arg, 0, f + lay.params + off as Cell, size)?;
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
        // The memory stack pointer travels with the frame only when the callee (or
        // something it calls) uses it.
        let callee_needs_msp = match &target {
            Target::Plan(idx) => self.plan.needs_msp[self.plan.funcs[*idx].fid],
            Target::Pointer(_) => true,
        };
        if callee_needs_msp {
            for k in 0..self.ptr_limbs() {
                self.e.clear(f + lay.msp + k as Cell);
                self.e
                    .copy_cell(lay.msp + k as Cell, f + lay.msp + k as Cell, lay.tmp);
            }
        }
        self.e.set(f + lay.active, 1);
        self.e.clear(f + lay.home);
        match &target {
            Target::Plan(idx) => {
                let id = self.plan.funcs[*idx].entry_id;
                self.set_pc(f, id);
            }
            Target::Pointer(op) => {
                let pl = self.ptr_limbs();
                let m = self.sc.mark();
                let w = Work::alloc(&mut self.sc, pl);
                self.load_operand(op, w)?;
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
        // The logical frame shift: all following code addresses the callee frame.
        let p = self.e.pos();
        self.e.set_pos(p - f);
        Ok(())
    }

    pub(super) fn emit_ret(&mut self, value: &Option<Operand>) -> Result<()> {
        let lay = self.lay.clone();
        if let Some(op) = value {
            let size = self.types().size(op.ty);
            let m = self.sc.mark();
            let w = Work::alloc(&mut self.sc, size);
            self.load_any(op, w)?;
            self.store_to_cells(lay.ret, size, w);
            self.sc.release(m);
        }
        self.e.clear(lay.active);
        self.prepare_dispatch_cells(-lay.frame);
        let p = self.e.pos();
        self.e.set_pos(p + lay.frame);
        Ok(())
    }
}
