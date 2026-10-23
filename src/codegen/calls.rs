//! Calls and returns: argument passing and the logical frame shift.

use super::*;

impl<'a> Gen<'a> {
    pub(super) fn finish_call(&mut self, bb: &llvm_ir::BasicBlock, i: usize) -> Result<()> {
        let call = match &bb.instrs[i] {
            Instruction::Call(c) => c,
            _ => unreachable!(),
        };
        if let Some(dest) = &call.dest {
            let (cell, size) = self.slot(dest)?;
            let f = self.lay.frame;
            for k in 0..size {
                self.e.clear(cell + k as Cell);
                self.e
                    .move_cell(f + self.lay.ret + k as Cell, cell + k as Cell);
            }
        }
        if call_is_variadic(call) {
            let (_, total) = va_spill_layout(self.module, self.ti, call)?;
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

    pub(super) fn param_offsets(&self, tys: &[TypeRef]) -> Result<Vec<(usize, usize)>> {
        let mut v = Vec::new();
        let mut off = 0;
        for t in tys {
            let s = self.ti.size_of(t)?;
            v.push((off, s));
            off += s;
        }
        Ok(v)
    }

    pub(super) fn emit_call(
        &mut self,
        bb: &llvm_ir::BasicBlock,
        i: usize,
        seg: &Segment,
    ) -> Result<()> {
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
            let idx = self.plan.func_by_name.get(&target).ok_or_else(|| {
                anyhow!(
                    "call to undefined function @{} (link the llvbf runtime?)",
                    target
                )
            })?;
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
                bail!(
                    "call signature does not match the definition of @{}",
                    callee.name
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
            self.e
                .copy_cell(lay.msp + k as Cell, f + lay.msp + k as Cell, lay.tmp);
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

    pub(super) fn emit_ret(&mut self, ret: &llvm_ir::terminator::Ret) -> Result<()> {
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
}
