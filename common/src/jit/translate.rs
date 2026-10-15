// Translates ARM basic blocks to Thumb-1 so they can run natively on the RP2040.
//
// The ARM registers r0-r14 are the same hardware registers in the translated code. Anything that
// needs scratch space saves a few low registers on the stack with push/pop, which leave the
// flags alone, and instructions that must not change the flags but can only be done with a
// flag setting Thumb instruction save and restore them with MRS/MSR (unless the flags are dead).
use super::{
    JitError,
    decode::{COND_AL, Decoded, DpOp, Insn, MemKind, MsrSrc, Offset, Op2, Shift, decode},
    emit::Emit,
    thumb::{self, LR, PC, SP, is_low},
};

// Longest run of ARM instructions translated into a single block
const MAX_BLOCK: usize = 48;

pub struct Tr<'a, 'm> {
    pub e: Emit<'a>,
    mem: &'m mut dyn FnMut(u32) -> Option<u32>,
    // words pushed on the stack by the instruction being translated
    pushed: u16,
    // address of the instruction being translated
    addr: u32,
}

#[derive(Clone, Copy)]
struct Temps {
    regs: [u8; 6],
    n: usize,
}

impl Temps {
    fn get(&self, i: usize) -> u8 {
        self.regs[i]
    }
}

fn bit(r: u8) -> u8 {
    if is_low(r) { 1 << r } else { 0 }
}

// is this immediate something a Thumb flag setting instruction can do directly
fn fits_u8(v: u32) -> bool {
    v <= 0xFF
}

enum V {
    Const(u32),
    Reg(u8),
    Shifted { rm: u8, kind: Shift, amount: u8 },
    ShiftReg { rm: u8, kind: Shift, rs: u8 },
}

impl<'a, 'm> Tr<'a, 'm> {
    pub fn new(e: Emit<'a>, mem: &'m mut dyn FnMut(u32) -> Option<u32>) -> Self {
        Tr {
            e,
            mem,
            pushed: 0,
            addr: 0,
        }
    }

    fn unsupported(&mut self) {
        let addr = self.addr;
        let word = (self.mem)(addr).unwrap_or(0);
        self.e.fail(JitError::Unsupported { addr, word });
    }

    fn pcv(&self) -> u32 {
        self.addr.wrapping_add(8)
    }

    fn pick(&mut self, avoid: u8, n: usize) -> Temps {
        let mut t = Temps { regs: [0; 6], n: 0 };
        for r in 0..8u8 {
            if t.n == n {
                break;
            }
            if avoid & (1 << r) == 0 {
                t.regs[t.n] = r;
                t.n += 1;
            }
        }
        if t.n < n {
            self.e.fail(JitError::Internal("out of temporaries"));
        }
        t
    }

    fn save(&mut self, t: &Temps) {
        if t.n == 0 {
            return;
        }
        let mut mask = 0u8;
        for i in 0..t.n {
            mask |= 1 << t.regs[i];
        }
        self.e.put(thumb::push(mask, false));
        self.pushed += t.n as u16;
    }

    fn restore(&mut self, t: &Temps) {
        if t.n == 0 {
            return;
        }
        let mut mask = 0u8;
        for i in 0..t.n {
            mask |= 1 << t.regs[i];
        }
        self.e.put(thumb::pop(mask, false));
        self.pushed -= t.n as u16;
    }

    // copy ARM register r into the low register t
    fn read_reg(&mut self, t: u8, r: u8) {
        if r == PC {
            let v = self.pcv();
            self.e.lit(t, v);
        } else if r == SP && self.pushed > 0 {
            self.e.put(thumb::add_rd_sp(t, self.pushed));
        } else {
            self.e.put(thumb::mov(t, r));
        }
    }

    // Load a constant into a low register without touching the flags
    fn load_const(&mut self, t: u8, v: u32) {
        self.e.lit(t, v);
    }

    // reg += value, without touching the flags, using t as scratch. reg must not be sp
    fn add_const(&mut self, reg: u8, t: u8, value: u32) {
        if value == 0 {
            return;
        }
        self.e.lit(t, value);
        self.e.put(thumb::add_hi(reg, t));
    }

    fn mrs(&mut self, t: u8) {
        self.e.put_all(&thumb::mrs(t));
    }

    fn msr(&mut self, t: u8) {
        self.e.put_all(&thumb::msr(t));
    }

    // ---------------------------------------------------------------------------------------
    // blocks

    // Translate the block starting at start, returns the ARM address after the last instruction
    pub fn block(&mut self, start: u32) -> u32 {
        let mut insns = [Decoded {
            cond: 0,
            insn: Insn::Unsupported,
        }; MAX_BLOCK];
        let mut n = 0;
        let mut a = start;
        let mut ends = false;
        while n < MAX_BLOCK {
            let Some(word) = (self.mem)(a) else {
                self.e.fail(JitError::BadRead(a));
                return a;
            };
            let d = decode(word, a);
            insns[n] = d;
            n += 1;
            a = a.wrapping_add(4);
            if is_terminator(&d) {
                ends = true;
                break;
            }
        }

        for i in 0..n {
            self.addr = start.wrapping_add(4 * i as u32);
            let flags_ok = flags_dead_after(&insns[..n], i);
            self.insn(insns[i], flags_ok);
            if let Some(err) = self.e.err {
                if let JitError::Internal(msg) = err {
                    self.e.err = Some(JitError::At {
                        addr: self.addr,
                        msg,
                    });
                }
                return a;
            }
            if self.e.pool_pressure() > 240 && i + 1 < n {
                self.e.flush_pool(true);
            }
        }
        if !ends {
            self.e.slot(a);
        }
        self.e.flush_pool(false);
        a
    }

    fn insn(&mut self, d: Decoded, flags_ok: bool) {
        let addr = self.addr;
        match d.insn {
            Insn::Unsupported => self.unsupported(),
            Insn::Branch { link, target } => self.branch(d.cond, link, target),
            Insn::Bx { rm } => self.bx(d.cond, rm),
            _ => {
                if self.writes_pc(&d.insn) {
                    self.pc_write(d);
                    return;
                }
                let skip = if d.cond != COND_AL {
                    Some(self.skip_open(d.cond, &d.insn))
                } else {
                    None
                };
                match d.insn {
                    Insn::Dp { op, s, rd, rn, op2 } => self.dp(op, s, rd, rn, op2, flags_ok),
                    Insn::Mul {
                        acc,
                        s,
                        rd,
                        rm,
                        rs,
                        rn,
                    } => self.mul(acc, s, rd, rm, rs, rn, flags_ok),
                    Insn::Mem {
                        load,
                        kind,
                        pre,
                        up,
                        wb,
                        rd,
                        rn,
                        off,
                    } => self.mem_access(load, kind, pre, up, wb, rd, rn, off, flags_ok),
                    Insn::Block {
                        load,
                        pre,
                        up,
                        wb,
                        rn,
                        list,
                    } => self.block_transfer(load, pre, up, wb, rn, list, flags_ok),
                    Insn::Swi { num } => self.e.put(thumb::svc(num)),
                    Insn::Mrs { rd, spsr } => self.mrs_insn(rd, spsr, flags_ok),
                    Insn::Msr { spsr, flags, src } => self.msr_insn(spsr, flags, src),
                    _ => {
                        let _ = addr;
                        self.unsupported()
                    }
                }
                if let Some(s) = skip {
                    self.skip_close(s);
                }
            }
        }
    }

    // Branch over the translation of a conditional instruction when the condition fails
    fn skip_open(&mut self, cond: u8, insn: &Insn) -> (usize, bool) {
        let big = matches!(insn, Insn::Block { list, .. } if list.count_ones() > 5);
        if big {
            // b<cond> over the following long branch, b skips the instruction
            self.e.put(thumb::b_cond(cond, 0));
            (self.e.fwd(), true)
        } else {
            (self.e.fwd_cond(cond ^ 1), false)
        }
    }

    fn skip_close(&mut self, (at, big): (usize, bool)) {
        if big {
            self.e.bind(at);
        } else {
            self.e.bind_cond(at);
        }
    }

    fn writes_pc(&self, insn: &Insn) -> bool {
        match *insn {
            Insn::Dp { op, rd, .. } => !op.is_test() && rd == PC,
            Insn::Mem { load, rd, .. } => load && rd == PC,
            Insn::Block { load, list, .. } => load && list & (1 << 15) != 0,
            _ => false,
        }
    }

    // ---------------------------------------------------------------------------------------
    // branches

    fn branch(&mut self, cond: u8, link: bool, target: u32) {
        let next = self.addr.wrapping_add(4);
        if !link {
            if cond == COND_AL {
                self.e.slot(target);
            } else {
                // b<cond> taken; fall-through slot; taken slot
                if self.e.pc() & 2 == 0 {
                    self.e.put(thumb::NOP);
                }
                let at = self.e.fwd_cond(cond);
                self.e.slot(next);
                self.e.bind_cond(at);
                self.e.slot(target);
            }
            return;
        }
        let skip = if cond != COND_AL {
            Some(self.e.fwd_cond(cond ^ 1))
        } else {
            None
        };
        // lr = the translated code after the call, which is the slot for the next instruction
        self.e.put(thumb::push(1, false));
        let lit = self.e.lit_late(0);
        self.e.put(thumb::mov(LR, 0));
        self.e.put(thumb::pop(1, false));
        self.e.slot(target);
        if let Some(s) = skip {
            self.e.bind_cond(s);
        }
        let ret = self.e.slot(next);
        let ret_addr = self.e.addr_of(ret) | 1;
        self.e.set_pool(lit, ret_addr);
    }

    fn bx(&mut self, cond: u8, rm: u8) {
        if rm == PC {
            self.unsupported();
            return;
        }
        if cond == COND_AL {
            self.e.put(thumb::bx(rm));
        } else {
            let at = self.e.fwd_cond(cond ^ 1);
            self.e.put(thumb::bx(rm));
            self.e.bind_cond(at);
            let next = self.addr.wrapping_add(4);
            self.e.slot(next);
        }
    }

    // Instructions that load the pc: jump through the stack with a pop so no register is lost
    fn pc_write(&mut self, d: Decoded) {
        let next = self.addr.wrapping_add(4);
        let skip = if d.cond != COND_AL {
            Some(self.e.fwd_cond(d.cond ^ 1))
        } else {
            None
        };
        match d.insn {
            Insn::Block {
                load: true,
                pre: false,
                up: true,
                wb: true,
                rn: SP,
                list,
            } if list & 0x7FFF & !0xFF == 0 || list & 0x7FFF == 0 => {
                // pop {low registers, pc}
                self.e.put(thumb::pop((list & 0xFF) as u8, true));
            }
            Insn::Block {
                load: true,
                pre: false,
                up: true,
                wb: true,
                rn: SP,
                list,
            } => {
                // high registers as well: load those first, then pop the pc
                self.block_transfer(true, false, true, false, SP, list & 0x7FFF, false);
                // the transfer above does not move sp, do it with the pc pop
                let words = (list & 0x7FFF).count_ones() as u16;
                if words > 0 {
                    self.e.put(thumb::add_sp(words));
                }
                self.e.put(thumb::pop(0, true));
            }
            Insn::Mem {
                load: true,
                kind: MemKind::Word,
                pre: true,
                up,
                wb: false,
                rn: PC,
                off: Offset::Imm(imm),
                ..
            } => {
                // ldr pc, [pc, #imm], a jump through a literal
                let a = self.pcv();
                let a = if up {
                    a.wrapping_add(imm)
                } else {
                    a.wrapping_sub(imm)
                };
                match (self.mem)(a) {
                    Some(target) if target & 1 == 0 => {
                        self.e.slot(target);
                    }
                    Some(target) => {
                        // Thumb target
                        self.e.put(thumb::push(1, false));
                        self.e.lit(0, target);
                        self.e.put(thumb::mov(LR, 0));
                        self.e.put(thumb::pop(1, false));
                        self.e.put(thumb::bx(LR));
                    }
                    None => self.e.fail(JitError::BadRead(a)),
                }
            }
            _ => self.unsupported(),
        }
        if let Some(s) = skip {
            self.e.bind_cond(s);
            self.e.slot(next);
        }
    }

    // ---------------------------------------------------------------------------------------
    // data processing

    fn dp(&mut self, op: DpOp, s: bool, rd: u8, rn: u8, op2: Op2, flags_ok: bool) {
        let res = !op.is_test();
        let pcv = self.pcv();
        // normalise the operand
        let (mut v, rotated) = match op2 {
            Op2::Imm { value, rotated } => (V::Const(value), rotated),
            Op2::Reg {
                rm,
                kind: Shift::Lsl,
                amount: 0,
            } => {
                if rm == PC {
                    (V::Const(pcv), false)
                } else {
                    (V::Reg(rm), false)
                }
            }
            Op2::Reg {
                kind: Shift::Ror,
                amount: 0,
                ..
            } => {
                // RRX
                self.unsupported();
                return;
            }
            Op2::Reg { rm, kind, amount } => {
                if rm == PC {
                    self.unsupported();
                    return;
                }
                (V::Shifted { rm, kind, amount }, false)
            }
            Op2::RegShift { rm, kind, rs } => {
                if rm == PC || rs == PC {
                    self.unsupported();
                    return;
                }
                (V::ShiftReg { rm, kind, rs }, false)
            }
        };
        if res && rd == SP && !matches!((op, rn), (DpOp::Add | DpOp::Sub, SP)) {
            // sp is only written directly when nothing is pushed on top of it
        }

        let clobber_ok = s || flags_ok;
        let mut op = op;
        // MVN of a constant is a MOV of the inverted constant, the carry still comes from the
        // original immediate
        let carry_bit = match v {
            V::Const(c) => c >> 31 != 0,
            _ => false,
        };
        if op == DpOp::Mvn
            && let V::Const(c) = v
        {
            v = V::Const(!c);
            op = DpOp::Mov;
        }

        // ---- fast paths that need no scratch registers
        if self.dp_fast(op, s, rd, rn, &v, clobber_ok, rotated) {
            return;
        }

        // ---- generic path
        if res && rd == SP {
            self.unsupported();
            return;
        }
        let uses_rn = op.uses_rn();
        let flagfree_form = match op {
            DpOp::Mov => matches!(v, V::Const(_) | V::Reg(_)),
            DpOp::Add => matches!(v, V::Const(_) | V::Reg(_)),
            _ => false,
        };
        let shifted = matches!(v, V::Shifted { .. } | V::ShiftReg { .. });
        let wrap = !clobber_ok && !flagfree_form;
        let carry_hazard = op.uses_carry_in() && shifted;
        let use_tf = wrap || carry_hazard;
        let c_preset = s && op.is_logical() && rotated && matches!(v, V::Const(_));

        // which operand registers are used in place
        let v_inplace = match v {
            V::Reg(r) if is_low(r) => Some(r),
            _ => None,
        };
        let u_inplace = if uses_rn && is_low(rn) && rn != PC {
            Some(rn)
        } else {
            None
        };

        // can the result be built in rd
        let rd_low = res && is_low(rd);
        let commutative = matches!(op, DpOp::And | DpOp::Eor | DpOp::Orr | DpOp::Adc);
        let two_op_forward = matches!(
            op,
            DpOp::And | DpOp::Eor | DpOp::Orr | DpOp::Bic | DpOp::Adc | DpOp::Sbc
        );
        let reverse_two_op = matches!(op, DpOp::Rsc);
        let mut swap = false;
        let mut r_is_rd = rd_low;
        if rd_low && two_op_forward {
            // R = u then R op= v, clobbers v when it lives in rd
            if v_inplace == Some(rd) && u_inplace != Some(rd) {
                if commutative && uses_rn {
                    swap = true;
                } else {
                    r_is_rd = false;
                }
            }
        }
        if rd_low && reverse_two_op {
            // R = v then R op= u
            if u_inplace == Some(rd) && v_inplace != Some(rd) {
                r_is_rd = false;
            }
        }
        if op == DpOp::Teq {
            r_is_rd = false;
        }
        let need_r = (res || op == DpOp::Teq) && !r_is_rd;

        // count temporaries
        let mut n = 0;
        let mut avoid = 0u8;
        if rd_low {
            avoid |= bit(rd);
        }
        if let Some(r) = u_inplace {
            avoid |= bit(r);
        }
        match v {
            V::Reg(r) => avoid |= bit(r),
            V::Shifted { rm, .. } => avoid |= bit(rm),
            V::ShiftReg { rm, rs, .. } => avoid |= bit(rm) | bit(rs),
            V::Const(_) => {}
        }
        let tv_idx = match v {
            V::Const(_) | V::Shifted { .. } | V::ShiftReg { .. } => {
                n += 1;
                Some(n - 1)
            }
            V::Reg(r) if !is_low(r) => {
                n += 1;
                Some(n - 1)
            }
            _ => None,
        };
        let ta_idx = if matches!(
            v,
            V::Shifted {
                kind: Shift::Ror,
                ..
            }
        ) {
            n += 1;
            Some(n - 1)
        } else if let V::ShiftReg { rs, .. } = v
            && !is_low(rs)
        {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tu_idx = if uses_rn && u_inplace.is_none() {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tr_idx = if need_r {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tc_idx = if c_preset {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tf_idx = if use_tf {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let temps = self.pick(avoid, n);
        if self.e.err.is_some() {
            return;
        }
        self.save(&temps);
        if let Some(i) = tf_idx {
            self.mrs(temps.get(i));
        }

        // ---- operand 2
        let vreg = match v {
            V::Const(c) => {
                let t = temps.get(tv_idx.unwrap());
                self.load_const(t, c);
                t
            }
            V::Reg(r) => {
                if is_low(r) {
                    r
                } else {
                    let t = temps.get(tv_idx.unwrap());
                    self.read_reg(t, r);
                    t
                }
            }
            V::Shifted { rm, kind, amount } => {
                let t = temps.get(tv_idx.unwrap());
                match kind {
                    Shift::Ror => {
                        self.read_reg(t, rm);
                        let c = temps.get(ta_idx.unwrap());
                        self.load_const(c, amount as u32);
                        self.e.put(thumb::alu(thumb::ROR, t, c));
                    }
                    _ => {
                        let src = if is_low(rm) {
                            rm
                        } else {
                            self.read_reg(t, rm);
                            t
                        };
                        let a = amount as u16;
                        self.e.put(match kind {
                            Shift::Lsl => thumb::lsls_imm(t, src, a),
                            Shift::Lsr => thumb::lsrs_imm(t, src, a),
                            _ => thumb::asrs_imm(t, src, a),
                        });
                    }
                }
                t
            }
            V::ShiftReg { rm, kind, rs } => {
                let t = temps.get(tv_idx.unwrap());
                self.read_reg(t, rm);
                let rsr = if is_low(rs) {
                    rs
                } else {
                    let x = temps.get(ta_idx.unwrap());
                    self.read_reg(x, rs);
                    x
                };
                let code = match kind {
                    Shift::Lsl => thumb::LSL,
                    Shift::Lsr => thumb::LSR,
                    Shift::Asr => thumb::ASR,
                    Shift::Ror => thumb::ROR,
                };
                self.e.put(thumb::alu(code, t, rsr));
                t
            }
        };
        if carry_hazard {
            // the shift clobbered the carry the instruction reads
            self.msr(temps.get(tf_idx.unwrap()));
        }

        // ---- operand 1
        let ureg = if uses_rn {
            match u_inplace {
                Some(r) => r,
                None => {
                    let t = temps.get(tu_idx.unwrap());
                    self.read_reg(t, rn);
                    t
                }
            }
        } else {
            0
        };

        // the carry out of a rotated immediate is set before the logical op, which keeps it
        if c_preset {
            let t = temps.get(tc_idx.unwrap());
            if carry_bit {
                self.e.put(thumb::movs_imm(t, 1));
                self.e.put(thumb::lsrs_imm(t, t, 1));
            } else {
                self.e.put(thumb::movs_imm(t, 0));
                self.e.put(thumb::lsls_imm(t, t, 1));
            }
        }

        let r = if need_r {
            temps.get(tr_idx.unwrap())
        } else {
            rd
        };
        let (u0, v0) = if swap { (vreg, ureg) } else { (ureg, vreg) };
        let flag_free = !clobber_ok;
        match op {
            DpOp::Mov => {
                if s {
                    self.e.put(thumb::movs_reg(r, vreg));
                } else if r != vreg {
                    self.e.put(thumb::mov(r, vreg));
                }
            }
            DpOp::Mvn => self.e.put(thumb::alu(thumb::MVN, r, vreg)),
            DpOp::Add => {
                if flag_free {
                    if r == v0 && r != u0 {
                        self.e.put(thumb::add_hi(r, u0));
                    } else {
                        if r != u0 {
                            self.e.put(thumb::mov(r, u0));
                        }
                        self.e.put(thumb::add_hi(r, v0));
                    }
                } else {
                    self.e.put(thumb::adds_reg(r, u0, v0));
                }
            }
            DpOp::Sub => self.e.put(thumb::subs_reg(r, u0, v0)),
            DpOp::Rsb => self.e.put(thumb::subs_reg(r, vreg, ureg)),
            DpOp::And | DpOp::Eor | DpOp::Orr | DpOp::Bic | DpOp::Adc | DpOp::Sbc => {
                if r != u0 {
                    self.e.put(thumb::mov(r, u0));
                }
                let code = match op {
                    DpOp::And => thumb::AND,
                    DpOp::Eor => thumb::EOR,
                    DpOp::Orr => thumb::ORR,
                    DpOp::Bic => thumb::BIC,
                    DpOp::Adc => thumb::ADC,
                    _ => thumb::SBC,
                };
                self.e.put(thumb::alu(code, r, v0));
            }
            DpOp::Rsc => {
                if r != vreg {
                    self.e.put(thumb::mov(r, vreg));
                }
                self.e.put(thumb::alu(thumb::SBC, r, ureg));
            }
            DpOp::Tst => self.e.put(thumb::alu(thumb::TST, ureg, vreg)),
            DpOp::Cmp => self.e.put(thumb::alu(thumb::CMP, ureg, vreg)),
            DpOp::Cmn => self.e.put(thumb::alu(thumb::CMN, ureg, vreg)),
            DpOp::Teq => {
                if r != ureg {
                    self.e.put(thumb::mov(r, ureg));
                }
                self.e.put(thumb::alu(thumb::EOR, r, vreg));
            }
        }
        if res && r != rd {
            self.e.put(thumb::mov(rd, r));
        }
        if wrap {
            self.msr(temps.get(tf_idx.unwrap()));
        }
        self.restore(&temps);
    }

    // Single instruction translations. Returns false when the generic path has to handle it.
    fn dp_fast(
        &mut self,
        op: DpOp,
        s: bool,
        rd: u8,
        rn: u8,
        v: &V,
        clobber_ok: bool,
        rotated: bool,
    ) -> bool {
        let e = &mut self.e;
        match (op, v) {
            (DpOp::Mov, V::Const(c)) if !(s && rotated) => {
                if is_low(rd) {
                    if fits_u8(*c) && clobber_ok {
                        e.put(thumb::movs_imm(rd, *c as u16));
                    } else if !s {
                        e.lit(rd, *c);
                    } else {
                        return false;
                    }
                    return true;
                }
                false
            }
            (DpOp::Mov, V::Reg(rm)) => {
                if !s {
                    if rd != *rm {
                        e.put(thumb::mov(rd, *rm));
                    }
                    true
                } else if is_low(rd) && is_low(*rm) {
                    e.put(thumb::movs_reg(rd, *rm));
                    true
                } else {
                    false
                }
            }
            (DpOp::Mov, V::Shifted { rm, kind, amount })
                if clobber_ok && is_low(rd) && is_low(*rm) && *kind != Shift::Ror =>
            {
                let a = *amount as u16;
                e.put(match kind {
                    Shift::Lsl => thumb::lsls_imm(rd, *rm, a),
                    Shift::Lsr => thumb::lsrs_imm(rd, *rm, a),
                    _ => thumb::asrs_imm(rd, *rm, a),
                });
                true
            }
            (DpOp::Cmp, V::Const(c)) if is_low(rn) && fits_u8(*c) => {
                e.put(thumb::cmp_imm(rn, *c as u16));
                true
            }
            (DpOp::Cmp | DpOp::Cmn | DpOp::Tst, V::Reg(rm)) if is_low(rn) && is_low(*rm) => {
                let code = match op {
                    DpOp::Cmp => thumb::CMP,
                    DpOp::Cmn => thumb::CMN,
                    _ => thumb::TST,
                };
                e.put(thumb::alu(code, rn, *rm));
                true
            }
            (DpOp::Add | DpOp::Sub, V::Const(c)) => {
                if rd == SP && rn == SP && c % 4 == 0 && *c <= 508 {
                    e.put(if op == DpOp::Add {
                        thumb::add_sp((*c / 4) as u16)
                    } else {
                        thumb::sub_sp((*c / 4) as u16)
                    });
                    return true;
                }
                if op == DpOp::Add && rn == SP && is_low(rd) && c % 4 == 0 && *c <= 1020 {
                    e.put(thumb::add_rd_sp(rd, (*c / 4) as u16));
                    return true;
                }
                if clobber_ok && is_low(rd) && is_low(rn) && rn != PC {
                    if rd == rn && fits_u8(*c) {
                        e.put(if op == DpOp::Add {
                            thumb::adds_imm8(rd, *c as u16)
                        } else {
                            thumb::subs_imm8(rd, *c as u16)
                        });
                        return true;
                    }
                    if *c <= 7 {
                        e.put(if op == DpOp::Add {
                            thumb::adds_imm3(rd, rn, *c as u16)
                        } else {
                            thumb::subs_imm3(rd, rn, *c as u16)
                        });
                        return true;
                    }
                }
                false
            }
            (DpOp::Add | DpOp::Sub, V::Reg(rm))
                if clobber_ok && is_low(rd) && is_low(rn) && is_low(*rm) =>
            {
                e.put(if op == DpOp::Add {
                    thumb::adds_reg(rd, rn, *rm)
                } else {
                    thumb::subs_reg(rd, rn, *rm)
                });
                true
            }
            (DpOp::Add, V::Reg(rm)) if !clobber_ok && rn != PC && *rm != PC && rd != PC => {
                // ADD between any registers leaves the flags alone
                if rd == rn {
                    e.put(thumb::add_hi(rd, *rm));
                } else if rd == *rm {
                    e.put(thumb::add_hi(rd, rn));
                } else {
                    e.put(thumb::mov(rd, rn));
                    e.put(thumb::add_hi(rd, *rm));
                }
                true
            }
            (DpOp::And | DpOp::Eor | DpOp::Orr | DpOp::Bic, V::Reg(rm))
                if clobber_ok && is_low(rd) && is_low(rn) && is_low(*rm) =>
            {
                let code = match op {
                    DpOp::And => thumb::AND,
                    DpOp::Eor => thumb::EOR,
                    DpOp::Orr => thumb::ORR,
                    _ => thumb::BIC,
                };
                if rd == rn {
                    e.put(thumb::alu(code, rd, *rm));
                    true
                } else if rd == *rm && op != DpOp::Bic {
                    e.put(thumb::alu(code, rd, rn));
                    true
                } else {
                    false
                }
            }
            (DpOp::Mvn, V::Reg(rm)) if clobber_ok && is_low(rd) && is_low(*rm) => {
                e.put(thumb::alu(thumb::MVN, rd, *rm));
                true
            }
            (DpOp::Rsb, V::Const(0)) if clobber_ok && is_low(rd) && is_low(rn) => {
                e.put(thumb::alu(thumb::NEG, rd, rn));
                true
            }
            _ => false,
        }
    }

    // ---------------------------------------------------------------------------------------
    // status registers
    //
    // There are no modes or interrupt masks here, only the flags exist. Reads give the flags with
    // the system mode bits and writes only change the flags.

    fn mrs_insn(&mut self, rd: u8, spsr: bool, flags_ok: bool) {
        if rd == PC || rd == SP {
            self.unsupported();
            return;
        }
        if spsr {
            // there is no saved status, hand out the mode bits alone
            self.dp(
                DpOp::Mov,
                false,
                rd,
                0,
                Op2::Imm {
                    value: 0x1F,
                    rotated: false,
                },
                flags_ok,
            );
            return;
        }
        let n = if is_low(rd) { 1 } else { 2 };
        let temps = self.pick(bit(rd), n);
        if self.e.err.is_some() {
            return;
        }
        self.save(&temps);
        let r = if is_low(rd) { rd } else { temps.get(0) };
        let t = temps.get(n - 1);
        self.mrs(r);
        self.e.lit(t, 0x1F);
        // the flag bits are the top of the word, adding sets the mode bits
        self.e.put(thumb::add_hi(r, t));
        if r != rd {
            self.e.put(thumb::mov(rd, r));
        }
        self.restore(&temps);
    }

    fn msr_insn(&mut self, spsr: bool, flags: bool, src: MsrSrc) {
        if spsr || !flags {
            return;
        }
        match src {
            MsrSrc::Reg(rm) if rm == PC => self.unsupported(),
            MsrSrc::Reg(rm) if is_low(rm) => self.msr(rm),
            _ => {
                let temps = self.pick(0, 1);
                if self.e.err.is_some() {
                    return;
                }
                self.save(&temps);
                let t = temps.get(0);
                match src {
                    MsrSrc::Reg(rm) => self.read_reg(t, rm),
                    MsrSrc::Imm(v) => self.e.lit(t, v),
                }
                self.msr(t);
                self.restore(&temps);
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // multiply

    fn mul(&mut self, acc: bool, s: bool, rd: u8, rm: u8, rs: u8, rn: u8, flags_ok: bool) {
        if rd == PC || rm == PC || rs == PC || (acc && rn == PC) || rd == SP {
            self.unsupported();
            return;
        }
        let clobber_ok = s || flags_ok;
        let wrap = !clobber_ok;
        let mut avoid = 0u8;
        for r in [rd, rm, rs] {
            avoid |= bit(r);
        }
        let mut n = 0;
        let tm = if !is_low(rm) {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let ts = if !is_low(rs) {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        // the product is built in the result register, which must not be the accumulator
        let tr = if !is_low(rd) || (acc && rd == rn) {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tf = if wrap {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let temps = self.pick(avoid, n);
        if self.e.err.is_some() {
            return;
        }
        self.save(&temps);
        if let Some(i) = tf {
            self.mrs(temps.get(i));
        }
        let mut vm = rm;
        let mut vs = rs;
        if let Some(i) = tm {
            vm = temps.get(i);
            self.read_reg(vm, rm);
        }
        if let Some(i) = ts {
            vs = temps.get(i);
            self.read_reg(vs, rs);
        }
        let r = match tr {
            Some(i) => temps.get(i),
            None => rd,
        };
        // r = vm * vs, the multiply is r *= other
        if r == vs && r != vm {
            core::mem::swap(&mut vm, &mut vs);
        }
        if r != vm {
            self.e.put(thumb::mov(r, vm));
        }
        self.e.put(thumb::alu(thumb::MUL, r, vs));
        if acc {
            // add rn flag free, then fix up N and Z when asked for
            if is_low(rn) {
                self.e.put(thumb::add_hi(r, rn));
            } else {
                self.e.put(thumb::add_hi(r, rn));
            }
            if s {
                self.e.put(thumb::movs_reg(r, r));
            }
        }
        if r != rd {
            self.e.put(thumb::mov(rd, r));
        }
        if let Some(i) = tf {
            self.msr(temps.get(i));
        }
        self.restore(&temps);
    }

    // ---------------------------------------------------------------------------------------
    // single data transfer

    #[allow(clippy::too_many_arguments)]
    fn mem_access(
        &mut self,
        load: bool,
        kind: MemKind,
        pre: bool,
        up: bool,
        wb: bool,
        rd: u8,
        rn: u8,
        off: Offset,
        flags_ok: bool,
    ) {
        let pcv = self.pcv();
        if rd == PC || (load && rd == SP) {
            self.unsupported();
            return;
        }
        // a pc relative load of a constant
        if rn == PC
            && load
            && pre
            && !wb
            && let Offset::Imm(imm) = off
        {
            let a = if up {
                pcv.wrapping_add(imm)
            } else {
                pcv.wrapping_sub(imm)
            };
            let Some(w) = (self.mem)(a & !3) else {
                self.e.fail(JitError::BadRead(a));
                return;
            };
            let shift = (a & 3) * 8;
            let value = match kind {
                MemKind::Word => w.rotate_right(shift),
                MemKind::Byte => (w >> shift) & 0xFF,
                MemKind::SignedByte => ((w >> shift) as u8 as i8 as i32) as u32,
                MemKind::Half => (w >> shift) & 0xFFFF,
                MemKind::SignedHalf => ((w >> shift) as u16 as i16 as i32) as u32,
            };
            self.dp(
                DpOp::Mov,
                false,
                rd,
                0,
                Op2::Imm {
                    value,
                    rotated: false,
                },
                flags_ok,
            );
            return;
        }
        if rn == PC {
            self.unsupported();
            return;
        }
        if rn == SP && kind == MemKind::Word {
            if let Offset::Imm(4) = off {
                // str rt, [sp, #-4]! is a push, ldr rt, [sp], #4 is a pop
                if !load && pre && !up && wb && (is_low(rd) || rd == LR) {
                    let low = if is_low(rd) { 1u8 << rd } else { 0 };
                    self.e.put(thumb::push(low, rd == LR));
                    return;
                }
                if load && !pre && up && is_low(rd) {
                    self.e.put(thumb::pop(1 << rd, false));
                    return;
                }
            }
        }
        // The temporaries go on the stack just below sp, so anything stored below sp has to
        // move sp first
        if rn == SP
            && pre
            && !up
            && let Offset::Imm(i) = off
            && i != 0
            && i % 4 == 0
            && i / 4 <= 127
        {
            self.e.put(thumb::sub_sp((i / 4) as u16));
            self.mem_access(
                load,
                kind,
                true,
                true,
                false,
                rd,
                SP,
                Offset::Imm(0),
                flags_ok,
            );
            if !wb {
                self.e.put(thumb::add_sp((i / 4) as u16));
            }
            return;
        }

        let size_shift = match kind {
            MemKind::Word => 2,
            MemKind::Half | MemKind::SignedHalf => 1,
            MemKind::Byte | MemKind::SignedByte => 0,
        };
        let direct_imm = match off {
            Offset::Imm(i) if up && pre && !wb => {
                if i & ((1 << size_shift) - 1) == 0 && (i >> size_shift) <= 31 {
                    Some((i >> size_shift) as u16)
                } else {
                    None
                }
            }
            Offset::Imm(0) => Some(0),
            _ => None,
        };
        // sp relative word access
        if rn == SP
            && kind == MemKind::Word
            && is_low(rd)
            && pre
            && !wb
            && let Offset::Imm(i) = off
            && up
            && i % 4 == 0
            && i / 4 <= 255
        {
            self.e.put(if load {
                thumb::ldr_sp(rd, (i / 4) as u16)
            } else {
                thumb::str_sp(rd, (i / 4) as u16)
            });
            return;
        }

        // what does the offset need
        let reg_off = matches!(off, Offset::Reg { .. });
        let needs_flags = match off {
            Offset::Reg { kind, amount, .. } => !(kind == Shift::Lsl && amount == 0) || !up,
            _ => false,
        };
        let wrap = needs_flags && !flags_ok;
        let imm_in_reg = !reg_off && direct_imm.is_none();
        let update = !pre || wb;
        let base_is_temp = !is_low(rn) || (load && rd == rn && is_low(rn) && false);

        let mut avoid = 0u8;
        avoid |= bit(rn) | bit(rd);
        if let Offset::Reg { rm, .. } = off {
            avoid |= bit(rm);
        }
        let mut n = 0;
        let tb = if base_is_temp {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tt = if !is_low(rd) {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let to = if reg_off || imm_in_reg || (update && direct_imm != Some(0)) {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let to2 = if let Offset::Reg { rm, .. } = off
            && !is_low(rm)
        {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let tf = if wrap {
            n += 1;
            Some(n - 1)
        } else {
            None
        };
        let temps = self.pick(avoid, n);
        if self.e.err.is_some() {
            return;
        }
        self.save(&temps);
        if let Some(i) = tf {
            self.mrs(temps.get(i));
        }
        let b = match tb {
            Some(i) => {
                let t = temps.get(i);
                self.read_reg(t, rn);
                t
            }
            None => rn,
        };
        let rt = match tt {
            Some(i) => temps.get(i),
            None => rd,
        };
        if !load && tt.is_some() {
            self.read_reg(rt, rd);
        }

        // the register offset, with the sign applied when it has to be negative
        let mut offreg = 0u8;
        let mut off_const: Option<u32> = None;
        match off {
            Offset::Imm(i) => {
                if i != 0 && (direct_imm.is_none() || update) {
                    let signed = if up { i } else { i.wrapping_neg() };
                    off_const = Some(signed);
                }
            }
            Offset::Reg {
                rm,
                kind: sk,
                amount,
            } => {
                let t = temps.get(to.unwrap());
                offreg = t;
                let src = if is_low(rm) {
                    rm
                } else {
                    let x = temps.get(to2.unwrap());
                    self.read_reg(x, rm);
                    x
                };
                if sk == Shift::Lsl && amount == 0 {
                    if t != src {
                        self.e.put(thumb::mov(t, src));
                    }
                } else if sk == Shift::Ror && amount == 0 {
                    self.unsupported();
                } else {
                    let a = amount as u16;
                    match sk {
                        Shift::Lsl => self.e.put(thumb::lsls_imm(t, src, a)),
                        Shift::Lsr => self.e.put(thumb::lsrs_imm(t, src, a)),
                        Shift::Asr => self.e.put(thumb::asrs_imm(t, src, a)),
                        Shift::Ror => {
                            self.unsupported();
                        }
                    }
                }
                if !up {
                    self.e.put(thumb::alu(thumb::NEG, t, t));
                }
            }
        }

        let access = |tr: &mut Self, base: u8, imm5: Option<u16>, offr: u8| {
            tr.access(load, kind, rt, base, imm5, offr);
        };

        if pre {
            if update {
                // base = base + offset, then access at base
                if let Some(c) = off_const {
                    let t = temps.get(to.unwrap());
                    self.add_const(b, t, c);
                } else {
                    self.e.put(thumb::add_hi(b, offreg));
                }
                access(self, b, Some(0), 0);
            } else if let Some(i5) = direct_imm {
                access(self, b, Some(i5), 0);
            } else if let Some(c) = off_const {
                let t = temps.get(to.unwrap());
                self.load_const(t, c);
                access(self, b, None, t);
            } else {
                access(self, b, None, offreg);
            }
        } else {
            access(self, b, Some(0), 0);
            if let Some(c) = off_const {
                let t = temps.get(to.unwrap());
                self.add_const(b, t, c);
            } else if reg_off {
                self.e.put(thumb::add_hi(b, offreg));
            }
        }
        // the base register copy goes back, which also keeps any translated address
        if let Some(i) = tb {
            let t = temps.get(i);
            if rn != SP && rn != PC && !(load && rd == rn) {
                if update || true {
                    self.e.put(thumb::mov(rn, t));
                }
            }
        }
        if load && tt.is_some() {
            self.e.put(thumb::mov(rd, rt));
        }
        if let Some(i) = tf {
            self.msr(temps.get(i));
        }
        self.restore(&temps);
        // sp was only copied, writeback to it happens once nothing is pushed any more
        if rn == SP && update {
            match off {
                Offset::Imm(i) if i % 4 == 0 && i / 4 <= 127 => {
                    self.e.put(if up {
                        thumb::add_sp((i / 4) as u16)
                    } else {
                        thumb::sub_sp((i / 4) as u16)
                    });
                }
                _ => self.unsupported(),
            }
        }
    }

    // one load or store of rt from [base, #imm5 * size] or [base, offr]
    fn access(&mut self, load: bool, kind: MemKind, rt: u8, base: u8, imm5: Option<u16>, offr: u8) {
        match (imm5, load, kind) {
            (Some(i), true, MemKind::Word) => self.e.put(thumb::ldr_imm(rt, base, i)),
            (Some(i), false, MemKind::Word) => self.e.put(thumb::str_imm(rt, base, i)),
            (Some(i), true, MemKind::Byte) => self.e.put(thumb::ldrb_imm(rt, base, i)),
            (Some(i), false, MemKind::Byte) => self.e.put(thumb::strb_imm(rt, base, i)),
            (Some(i), true, MemKind::Half) => self.e.put(thumb::ldrh_imm(rt, base, i)),
            (Some(i), false, MemKind::Half) => self.e.put(thumb::strh_imm(rt, base, i)),
            (Some(i), true, MemKind::SignedByte) => {
                self.e.put(thumb::ldrb_imm(rt, base, i));
                self.e.put(thumb::sxtb(rt, rt));
            }
            (Some(i), true, MemKind::SignedHalf) => {
                self.e.put(thumb::ldrh_imm(rt, base, i));
                self.e.put(thumb::sxth(rt, rt));
            }
            (None, _, _) => {
                let op = match (load, kind) {
                    (true, MemKind::Word) => thumb::LDR_REG,
                    (false, MemKind::Word) => thumb::STR_REG,
                    (true, MemKind::Byte) => thumb::LDRB_REG,
                    (false, MemKind::Byte) => thumb::STRB_REG,
                    (true, MemKind::Half) => thumb::LDRH_REG,
                    (false, MemKind::Half) => thumb::STRH_REG,
                    (true, MemKind::SignedByte) => thumb::LDRSB_REG,
                    (true, MemKind::SignedHalf) => thumb::LDRSH_REG,
                    _ => {
                        self.unsupported();
                        return;
                    }
                };
                self.e.put(thumb::mem_reg(op, rt, base, offr));
            }
            _ => self.unsupported(),
        }
    }

    // ---------------------------------------------------------------------------------------
    // block data transfer

    #[allow(clippy::too_many_arguments)]
    fn block_transfer(
        &mut self,
        load: bool,
        pre: bool,
        up: bool,
        wb: bool,
        rn: u8,
        list: u16,
        _flags_ok: bool,
    ) {
        let n = list.count_ones();
        if n == 0 || rn == PC || list & (1 << 15) != 0 && !load {
            self.unsupported();
            return;
        }
        let low_list = (list & 0xFF) as u8;
        let all_low = list & !0xFF == 0;

        // PUSH and POP forms with only reachable registers
        if rn == SP && wb {
            if !load && !up && pre && list & !(0xFF | 1 << 14) == 0 {
                self.e.put(thumb::push(low_list, list & (1 << 14) != 0));
                return;
            }
            if load && up && !pre && all_low {
                self.e.put(thumb::pop(low_list, false));
                return;
            }
        }
        // LDMIA and STMIA with writeback on a low register
        if is_low(rn) && up && !pre && all_low && wb && !(load && list & (1 << rn) != 0) {
            self.e.put(if load {
                thumb::ldmia(rn, low_list)
            } else {
                thumb::stmia(rn, low_list)
            });
            return;
        }

        // start offset of the lowest register and the final base adjustment
        let nb = 4 * n as i32;
        let (start, final_delta) = match (up, pre) {
            (true, false) => (0, nb),
            (true, true) => (4, nb),
            (false, false) => (-nb + 4, -nb),
            (false, true) => (-nb, -nb),
        };
        let final_delta = if wb { final_delta } else { 0 };

        // the stack forms that talk to sp directly
        if rn == SP && wb && !load && !up && pre {
            self.push_general(list);
            return;
        }
        if rn == SP && wb && load && up && !pre {
            self.pop_general(list);
            return;
        }
        if rn == SP {
            self.unsupported();
            return;
        }

        // general case, registers move through a base register
        let high = list & 0xFF00 != 0;
        let base_in_list = list & (1 << rn) != 0;
        let copy_base = !is_low(rn) || base_in_list;
        // one scratch register serves as the data register for high registers and as the
        // register the base adjustments go through
        let mut cnt = 0;
        let ta = if copy_base {
            cnt += 1;
            Some(cnt - 1)
        } else {
            None
        };
        let tk = {
            cnt += 1;
            cnt - 1
        };
        let td = if high { Some(tk) } else { None };
        let temps = self.pick(bit(rn) | low_list, cnt);
        if self.e.err.is_some() {
            return;
        }
        self.save(&temps);
        let kt = temps.get(tk);
        let b = match ta {
            Some(i) => {
                let t = temps.get(i);
                self.read_reg(t, rn);
                t
            }
            None => rn,
        };
        self.add_const(b, kt, start as u32);
        let mut idx = 0u16;
        for r in 0..15u8 {
            if list & (1 << r) == 0 {
                continue;
            }
            if is_low(r) {
                self.e.put(if load {
                    thumb::ldr_imm(r, b, idx)
                } else {
                    thumb::str_imm(r, b, idx)
                });
            } else {
                let d = temps.get(td.unwrap());
                if load {
                    self.e.put(thumb::ldr_imm(d, b, idx));
                    self.e.put(thumb::mov(r, d));
                } else {
                    self.read_reg(d, r);
                    self.e.put(thumb::str_imm(d, b, idx));
                }
            }
            idx += 1;
        }
        let back = (final_delta - start) as u32;
        if ta.is_some() {
            if wb && !(load && base_in_list) {
                self.add_const(b, kt, back);
                self.e.put(thumb::mov(rn, b));
            }
        } else {
            self.add_const(b, kt, back);
        }
        self.restore(&temps);
    }

    // stmdb sp!, {list} with registers push cannot reach
    fn push_general(&mut self, list: u16) {
        let n = list.count_ones() as u16;
        let low = (list & 0xFF) as u8;
        let temp = if low != 0 {
            // a register of the list is free to use once it has been stored
            low.trailing_zeros() as u8
        } else {
            // no register of the list to borrow, save one in the slot under the data
            self.pick(0, 1).get(0)
        };
        let borrowed = low != 0;
        if borrowed {
            self.e.put(thumb::sub_sp(n));
            let mut idx = 0u16;
            // store the low registers first
            for r in 0..8u8 {
                if list & (1 << r) != 0 {
                    self.e.put(thumb::str_sp(r, idx));
                    idx += 1;
                }
            }
            for r in 8..15u8 {
                if list & (1 << r) != 0 {
                    self.read_reg(temp, r);
                    self.e.put(thumb::str_sp(temp, idx));
                    idx += 1;
                }
            }
            // the borrowed register comes back from its slot
            self.e.put(thumb::ldr_sp(temp, 0));
        } else {
            self.e.put(thumb::sub_sp(n + 1));
            self.e.put(thumb::str_sp(temp, 0));
            let mut idx = 1u16;
            for r in 8..15u8 {
                if list & (1 << r) != 0 {
                    self.e.put(thumb::mov(temp, r));
                    self.e.put(thumb::str_sp(temp, idx));
                    idx += 1;
                }
            }
            self.e.put(thumb::ldr_sp(temp, 0));
            self.e.put(thumb::add_sp(1));
        }
    }

    // ldmia sp!, {list} with registers pop cannot reach
    fn pop_general(&mut self, list: u16) {
        let n = list.count_ones() as u16;
        let low = (list & 0xFF) as u8;
        let t = if low != 0 {
            low.trailing_zeros() as u8
        } else {
            0
        };
        if low != 0 {
            // the high registers load through the lowest low register, which is loaded last
            let mut idx = 0u16;
            let mut slots = [0u16; 16];
            for r in 0..15u8 {
                if list & (1 << r) != 0 {
                    slots[r as usize] = idx;
                    idx += 1;
                }
            }
            for r in 8..15u8 {
                if list & (1 << r) != 0 {
                    self.e.put(thumb::ldr_sp(t, slots[r as usize]));
                    self.e.put(thumb::mov(r, t));
                }
            }
            for r in 0..8u8 {
                if list & (1 << r) != 0 {
                    self.e.put(thumb::ldr_sp(r, slots[r as usize]));
                }
            }
            self.e.put(thumb::add_sp(n));
        } else {
            // no low register in the list, borrow one
            let tmp = self.pick(0, 1);
            let t = tmp.get(0);
            self.save(&tmp);
            let mut idx = 0u16;
            for r in 8..15u8 {
                if list & (1 << r) != 0 {
                    self.e.put(thumb::ldr_sp(t, idx + self.pushed));
                    self.e.put(thumb::mov(r, t));
                    idx += 1;
                }
            }
            self.restore(&tmp);
            self.e.put(thumb::add_sp(n));
        }
    }
}

fn is_terminator(d: &Decoded) -> bool {
    match d.insn {
        Insn::Branch { .. } | Insn::Bx { .. } => true,
        Insn::Dp { op, rd, .. } => !op.is_test() && rd == PC,
        Insn::Mem { load, rd, .. } => load && rd == PC,
        Insn::Block { load, list, .. } => load && list & (1 << 15) != 0,
        Insn::Unsupported => true,
        _ => false,
    }
}

// Are the flags dead after instruction i, nothing after it reads them before they are rewritten
fn flags_dead_after(insns: &[Decoded], i: usize) -> bool {
    for d in &insns[i + 1..] {
        if d.cond != COND_AL {
            return false;
        }
        match d.insn {
            Insn::Dp { op, s, op2, .. } => {
                if op.uses_carry_in() {
                    return false;
                }
                if let Op2::Reg {
                    kind: Shift::Ror,
                    amount: 0,
                    ..
                } = op2
                {
                    return false;
                }
                if s && !op.is_logical() {
                    // arithmetic with the S bit rewrites all four flags
                    return true;
                }
                if is_terminator(d) {
                    return false;
                }
            }
            Insn::Mul { .. } | Insn::Mem { .. } | Insn::Block { .. } | Insn::Swi { .. } => {
                if is_terminator(d) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    false
}
