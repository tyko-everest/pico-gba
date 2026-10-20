// Translates ARM basic blocks to Thumb-1 so they can run natively on the RP2040.
//
// The ARM registers r0-r14 are the same hardware registers in the translated code and always
// hold GBA values. Loads and stores translate the address of their base register inline through
// a table of offsets indexed by the top byte (see Jit::new), so GBA memory that is flat in native
// memory is reached without a fault. When the base register is a known constant the address is
// worked out while translating and the access needs no lookup.
//
// Anything that needs scratch registers opens a frame: it takes registers that are dead at that
// point of the block for free, pushes more low registers when it needs them, works in them and
// pops them again. Push and pop leave the flags alone. A frame can also save the flags (MRS) and
// put them back (MSR) when the lowering has to clobber flags that are still needed. Which flags
// and registers are needed comes from a backward pass over the block.
use super::{
    JitError,
    decode::{
        BlockOp, COND_AL, Decoded, DpOp, Insn, MemKind, MemOp, MsrSrc, MulLongOp, MulOp, Offset,
        Op2, Shift, decode,
    },
    emit::Emit,
    thumb::{self, LR, PC, SP, is_low},
};

mod thumb_src;
pub use thumb_src::emit_dispatcher;

// Longest run of ARM instructions translated into a single block
const MAX_BLOCK: usize = 48;

// Flag sets, as masks
const FN: u8 = 8;
const FZ: u8 = 4;
const FC: u8 = 2;
const FV: u8 = 1;
const FNZ: u8 = FN | FZ;
const FNZC: u8 = FNZ | FC;
const FALL: u8 = FNZC | FV;

pub struct Tr<'a, 'm> {
    pub e: Emit<'a>,
    mem: &'m mut dyn FnMut(u32) -> Option<u32>,
    // native address of the region offset table, 0 translates nothing
    regions: u32,
    // every conditional instruction gets its own skip, with a branch that always reaches
    long_skips: bool,
    // the address of the indirect jump dispatcher, 0 when there is none
    dispatch: u32,
    // words pushed on the stack by the open frame
    pushed: u16,
    // address of the instruction being translated
    addr: u32,
    // the flags (FN..FV) read by anything after the instruction being translated
    live: u8,
    // the flags the instruction being translated sets itself
    own: u8,
    // low registers the instruction being translated may use as scratch without saving them: they
    // are dead after it and it does not read them
    free: u8,
    // the destination register when it is free (it is written anyway), what frames take first
    dest: u8,
    // low registers the instruction reads and nothing after it does
    dying: u8,
    // the flags the instruction reads, apart from its condition
    flags_in: u8,
    // the halfword the block being translated starts at and its ARM address
    start_hw: usize,
    start: u32,
    // registers known to hold a constant at this point of the block
    kc: [Option<u32>; 16],
}

// The scratch registers of one instruction
#[derive(Clone, Copy)]
struct Frame {
    mask: u8,
    flags: Option<u8>,
    // put the flags back when the frame closes
    restore: bool,
}

// the register of a temp that was not asked for, which is not a register
const NO_REG: u8 = 0xFF;

// Or'd into the flags a frame clobbers: the sequence puts the flags back itself halfway through,
// so they are saved even when nothing needs them after the instruction.
const RESTORE_MID: u8 = 16;

impl Frame {
    fn is_temp(&self, r: u8) -> bool {
        r < 8 && self.mask & (1 << r) != 0
    }

    // The word the original value of temp r was pushed to. push stores the lowest register at
    // the lowest address and sp points at it.
    fn slot(&self, r: u8) -> u16 {
        (self.mask & ((1u8 << r) - 1)).count_ones() as u16
    }
}

fn low_bit(r: u8) -> u8 {
    if is_low(r) { 1 << r } else { 0 }
}

// how a block transfer's frame looks
struct Plan {
    to: bool,
    rr: bool,
    td: bool,
    fast: bool,
    demand: usize,
}

// do n scratch registers fit among the low registers outside avoid, so that none of the
// registers the instruction uses has to be borrowed
fn fits(avoid: u8, n: usize) -> bool {
    n <= 8 - avoid.count_ones() as usize
}

#[derive(Clone, Copy)]
enum V {
    Const(u32),
    Reg(u8),
    Shifted { rm: u8, kind: Shift, amount: u8 },
    ShiftReg { rm: u8, kind: Shift, rs: u8 },
}

impl<'a, 'm> Tr<'a, 'm> {
    pub fn new(
        e: Emit<'a>,
        mem: &'m mut dyn FnMut(u32) -> Option<u32>,
        regions: u32,
        long_skips: bool,
        dispatch: u32,
    ) -> Self {
        Tr {
            dispatch,
            e,
            mem,
            regions,
            long_skips,
            pushed: 0,
            addr: 0,
            live: FALL,
            own: 0,
            free: 0,
            dest: 0,
            dying: 0,
            flags_in: 0,
            start_hw: 0,
            start: 0,
            kc: [None; 16],
        }
    }

    fn unsupported(&mut self) {
        let addr = self.addr;
        let word = (self.mem)(addr).unwrap_or(0);
        self.e.fail(JitError::Unsupported { addr, word });
    }

    // may the lowering change the flags in m: they are dead, or the instruction sets them anyway
    fn can(&self, m: u8) -> bool {
        m & self.live & !self.own == 0
    }

    fn pcv(&self) -> u32 {
        self.addr.wrapping_add(8)
    }

    // ---------------------------------------------------------------------------------------
    // frames

    // Open a frame with the scratch registers that want asks for, and return them in the same
    // order (NO_REG for the ones not asked for). Free registers (see Tr::free) cost nothing, the
    // others are pushed. Registers the instruction uses (avoid) are only taken when there is
    // nothing else, get and set cope with that. clobbers are the flags the sequence changes: when
    // any of them is still needed and not set by the instruction anyway the frame saves them in
    // one more register and puts them back when it closes. pref is taken first.
    fn open<const N: usize>(
        &mut self,
        want: [bool; N],
        avoid: u8,
        clobbers: u8,
        pref: u8,
    ) -> (Frame, [u8; N]) {
        let save = !self.can(clobbers);
        let total =
            want.iter().filter(|&&w| w).count() + (save || clobbers & RESTORE_MID != 0) as usize;
        let mut f = Frame {
            mask: 0,
            flags: None,
            restore: save,
        };
        let mut t = [NO_REG; N];
        let free = (self.free & !avoid) | pref;
        // the destination goes first in every pass
        let mut order = [0u8; 8];
        order[0] = pref.trailing_zeros().min(7) as u8;
        let mut m = if pref != 0 { 1 } else { 0 };
        for r in 0..8u8 {
            if pref != 1 << r {
                order[m] = r;
                m += 1;
            }
        }
        let mut k = 0;
        for pass in 0..3 {
            for &r in &order {
                let is_free = free & (1 << r) != 0;
                let avoided = avoid & (1 << r) != 0;
                let take = match pass {
                    0 => is_free,
                    1 => !is_free && !avoided,
                    _ => !is_free && avoided,
                };
                if take && k < total {
                    match (0..N).filter(|&i| want[i]).nth(k) {
                        Some(i) => t[i] = r,
                        None => f.flags = Some(r),
                    }
                    if !is_free {
                        f.mask |= 1 << r;
                    }
                    k += 1;
                }
            }
        }
        // A frame that pops and pushes the same registers right after is not needed: the values
        // are still in their slots and the registers are scratch anyway. With saved flags the
        // restore in between is not needed either, unless the instruction reads them.
        if (f.flags.is_none() || self.flags_in == 0) && self.e.reuse_pop(f.mask, f.flags) {
            self.pushed += f.mask.count_ones() as u16;
            return (f, t);
        }
        if f.mask != 0 {
            self.e.put(thumb::push(f.mask, false));
            self.pushed += f.mask.count_ones() as u16;
        }
        if let Some(t) = f.flags {
            self.e.put_all(&thumb::mrs(t));
        }
        (f, t)
    }

    fn close(&mut self, f: Frame) {
        let restore = f.flags.filter(|_| f.restore);
        if f.mask == 0 {
            if let Some(t) = restore {
                self.e.put_all(&thumb::msr(t));
            }
            return;
        }
        self.e.defer_pop(f.mask, restore);
        self.pushed -= f.mask.count_ones() as u16;
    }

    fn restore_flags(&mut self, f: &Frame) {
        if let Some(t) = f.flags {
            self.e.put_all(&thumb::msr(t));
        }
    }

    // Copy ARM register r into the low register t. t must be a temp of the frame.
    fn get_copy(&mut self, f: &Frame, r: u8, t: u8) {
        if r == PC {
            let v = self.pcv();
            self.e.lit(t, v);
        } else if r == SP {
            if self.pushed > 0 {
                self.e.put(thumb::add_rd_sp(t, self.pushed));
            } else {
                self.e.put(thumb::mov(t, SP));
            }
        } else if f.is_temp(r) {
            self.e.put(thumb::ldr_sp(t, f.slot(r)));
        } else if r != t {
            self.e.put(thumb::mov(t, r));
        }
    }

    // A low register holding ARM register r, which the caller must only read: r itself when it
    // can be, else t after a copy.
    fn get_ro(&mut self, f: &Frame, r: u8, t: u8) -> u8 {
        if is_low(r) && !f.is_temp(r) {
            r
        } else {
            self.get_copy(f, r, t);
            t
        }
    }

    fn set(&mut self, f: &Frame, r: u8, src: u8) {
        if f.is_temp(r) {
            self.e.put(thumb::str_sp(src, f.slot(r)));
        } else if r != src {
            self.e.put(thumb::mov(r, src));
        }
    }

    // ARM register r += the low register off, scratch is a temp that may be clobbered
    fn add_to(&mut self, f: &Frame, r: u8, off: u8, scratch: u8) {
        if f.is_temp(r) {
            self.e.put(thumb::ldr_sp(scratch, f.slot(r)));
            self.e.put(thumb::add_hi(scratch, off));
            self.e.put(thumb::str_sp(scratch, f.slot(r)));
        } else {
            self.e.put(thumb::add_hi(r, off));
        }
    }

    // Load a constant into a low register. With nz_ok a movs (or movs and mvns for a small
    // negative number) is shorter than the literal, with set_nz the flags have to describe the
    // value either way.
    fn mat_const(&mut self, t: u8, c: u32, set_nz: bool, nz_ok: bool) {
        if c <= 0xFF && nz_ok {
            self.e.put(thumb::movs_imm(t, c as u16));
        } else if !c <= 0xFF && nz_ok {
            self.e.put(thumb::movs_imm(t, !c as u16));
            self.e.put(thumb::mvn(t, t));
        } else {
            self.e.lit(t, c);
            if set_nz {
                self.e.put(thumb::movs_reg(t, t));
            }
        }
    }

    fn shift_imm(&mut self, kind: Shift, rd: u8, rm: u8, amount: u8) {
        let a = amount as u16;
        self.e.put(match kind {
            Shift::Lsl => thumb::lsls_imm(rd, rm, a),
            Shift::Lsr => thumb::lsrs_imm(rd, rm, a),
            _ => thumb::asrs_imm(rd, rm, a),
        });
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
            let mut d = decode(word, a);
            // a pc relative load reads a constant, which is known while translating
            if let Insn::Mem(MemOp {
                load: true,
                kind,
                pre: true,
                up,
                wb: false,
                rd,
                rn: PC,
                off: Offset::Imm(imm),
            }) = d.insn
                && rd != PC
            {
                let Some(value) = self.pc_literal(a, kind, up, imm) else {
                    return a;
                };
                d.insn = Insn::Dp {
                    op: DpOp::Mov,
                    s: false,
                    rd,
                    rn: 0,
                    op2: Op2::Imm {
                        value,
                        rotated: false,
                    },
                };
            }
            insns[n] = d;
            n += 1;
            a = a.wrapping_add(4);
            if is_terminator(&d) {
                ends = true;
                break;
            }
        }

        // Conditional instructions with the same condition share one skip, up to an instruction
        // that writes the flags. The members after the first know the condition holds.
        let mut in_group = [false; MAX_BLOCK];
        for i in 1..n {
            let (p, d) = (&insns[i - 1], &insns[i]);
            in_group[i] = !self.long_skips
                && d.cond != COND_AL
                && d.cond == p.cond
                && !is_terminator(d)
                && flags_written(p).0 == 0;
        }
        // The flags and registers live after each instruction, one backward pass. The code after
        // the block may read anything.
        let mut live = [0u8; MAX_BLOCK];
        let mut rlive = [0u16; MAX_BLOCK];
        let (mut l, mut rl) = (FALL, 0xFFFF);
        for i in (0..n).rev() {
            (live[i], rlive[i]) = (l, rl);
            (l, rl) = step(&insns[i], !in_group[i], l, rl);
        }

        self.kc = [None; 16];
        self.start_hw = self.e.pos();
        self.start = start;
        let mut skip = None;
        for i in 0..n {
            let d = insns[i];
            self.addr = start.wrapping_add(4 * i as u32);
            self.live = live[i];
            self.own = flags_written(&d).0 | flags_written(&d).1;
            let read = regs_read(&d);
            // the destination of these is written whatever it held
            self.dest = match d.insn {
                Insn::Dp { .. }
                | Insn::Mul(_)
                | Insn::Mem(MemOp { load: true, .. })
                | Insn::Mrs { .. } => (regs_written(&d) & !read) as u8,
                _ => 0,
            };
            self.free = !(rlive[i] | read) as u8 | self.dest;
            self.dying = (read & !rlive[i]) as u8;
            self.flags_in = flags_read(&d);
            let grouped = d.cond != COND_AL && !is_terminator(&d);
            if grouped && !in_group[i] {
                skip = Some(self.skip_open(d.cond));
            }
            self.insn(d);
            self.track(&d);
            if grouped && (i + 1 == n || !in_group[i + 1]) {
                self.e.bind(skip.take().unwrap());
            }
            if let Some(err) = self.e.err {
                if let JitError::Internal(msg) = err {
                    self.e.err = Some(JitError::At {
                        addr: self.addr,
                        msg,
                    });
                }
                return a;
            }
            if skip.is_none() && i + 1 < n && self.e.pool_due() {
                self.e.flush_pool(true);
            }
        }
        if !ends {
            self.e.slot(a);
        }
        self.e.settle();
        self.e.flush_pool(false);
        a
    }

    fn insn(&mut self, d: Decoded) {
        match d.insn {
            Insn::Unsupported => self.unsupported(),
            Insn::Branch { link, target } => self.branch(d.cond, link, target),
            Insn::Bx { rm } => self.bx(d.cond, rm),
            _ if writes_pc(&d.insn) => self.pc_write(d),
            Insn::Dp { op, s, rd, rn, op2 } => self.dp(op, s, rd, rn, op2),
            Insn::Mul(m) => self.mul(&m),
            Insn::MulLong(m) => self.mul_long(&m),
            Insn::Mem(m) => self.mem_access(&m),
            Insn::Block(b) => self.block_transfer(&b),
            Insn::Swi { num } => self.e.put(thumb::svc(num)),
            Insn::Mrs { rd, spsr } => self.mrs_insn(rd, spsr),
            Insn::Msr { spsr, flags, src } => self.msr_insn(spsr, flags, src),
        }
    }

    // Branch over the translation of conditional instructions when the condition fails
    fn skip_open(&mut self, cond: u8) -> usize {
        if self.long_skips {
            // b<cond> over the following long branch, b skips the instruction
            self.e.put(thumb::b_cond(cond, 0));
            self.e.fwd()
        } else {
            self.e.fwd_cond(cond ^ 1)
        }
    }

    // Branch over the code that follows when the condition fails. cond_close ends it and gives
    // the instruction after this one an exit for the case it was skipped, returning that slot.
    fn cond_open(&mut self, cond: u8) -> Option<usize> {
        (cond != COND_AL).then(|| self.e.fwd_cond(cond ^ 1))
    }

    fn cond_close(&mut self, skip: Option<usize>) -> Option<usize> {
        let at = skip?;
        self.e.bind(at);
        Some(self.e.slot(self.addr.wrapping_add(4)))
    }

    // ---------------------------------------------------------------------------------------
    // branches

    fn branch(&mut self, cond: u8, link: bool, target: u32) {
        let next = self.addr.wrapping_add(4);
        if !link {
            // a loop over the whole block jumps back inside it, if that is in reach
            let here = self.e.pos();
            let back = |ahead: usize| self.start_hw as i32 - (here + ahead + 2) as i32;
            if target == self.start && cond == COND_AL && back(0) >= -1024 {
                self.e.back(self.start_hw);
            } else if target == self.start && back(0) >= -128 {
                self.e.back_cond(cond, self.start_hw);
                self.e.slot(next);
            } else if target == self.start && back(1) >= -1024 {
                self.e.put(thumb::b_cond(cond ^ 1, 0));
                self.e.back(self.start_hw);
                self.e.slot(next);
            } else if cond == COND_AL {
                self.e.slot(target);
            } else {
                // b<cond> taken; fall-through slot; taken slot
                if self.e.pc() & 2 == 0 {
                    self.e.put(thumb::NOP);
                }
                let at = self.e.fwd_cond(cond);
                self.e.slot(next);
                self.e.bind(at);
                self.e.slot(target);
            }
            return;
        }
        let skip = self.cond_open(cond);
        // lr = the translated code after the call, which is the slot for the next instruction
        self.e.put(thumb::push(1, false));
        let lit = self.e.lit_late(0);
        self.e.put(thumb::mov(LR, 0));
        self.e.put(thumb::pop(1, false));
        self.e.slot(target);
        let ret = match self.cond_close(skip) {
            Some(ret) => ret,
            None => self.e.slot(next),
        };
        let ret_addr = self.e.addr_of(ret) | 1;
        self.e.set_pool(lit, ret_addr);
    }

    fn bx(&mut self, cond: u8, rm: u8) {
        if rm == PC {
            self.unsupported();
            return;
        }
        let skip = self.cond_open(cond);
        self.e.put(thumb::bx(rm));
        self.cond_close(skip);
    }

    // Instructions that load the pc: jump through the stack with a pop so no register is lost
    fn pc_write(&mut self, d: Decoded) {
        let skip = self.cond_open(d.cond);
        match d.insn {
            Insn::Block(BlockOp {
                load: true,
                pre: false,
                up: true,
                wb: true,
                rn: SP,
                list,
            }) => {
                let rest = list & 0x7FFF;
                if rest & (1 << SP) != 0 {
                    self.unsupported();
                } else if rest & !0xFF == 0 {
                    self.e.put(thumb::pop(rest as u8, true));
                } else {
                    // high registers as well: pop those first, then the pc
                    self.pop_general(rest);
                    self.e.put(thumb::pop(0, true));
                }
            }
            Insn::Mem(MemOp {
                load: true,
                kind: MemKind::Word,
                pre: true,
                up,
                wb: false,
                rn: PC,
                off: Offset::Imm(imm),
                ..
            }) => {
                // ldr pc, [pc, #imm], a jump through a literal
                let a = off_addr(self.pcv(), up, imm);
                match (self.mem)(a) {
                    Some(target) if target & 1 == 0 => {
                        self.e.slot(target);
                    }
                    Some(target) => {
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
        self.cond_close(skip);
    }

    // ---------------------------------------------------------------------------------------
    // data processing

    fn dp(&mut self, op: DpOp, s: bool, rd: u8, rn: u8, op2: Op2) {
        let pcv = self.pcv();
        let (mut v, rotated) = match op2 {
            Op2::Imm { value, rotated } => (V::Const(value), rotated),
            Op2::Reg {
                rm,
                kind: Shift::Lsl,
                amount: 0,
            } => (if rm == PC { V::Const(pcv) } else { V::Reg(rm) }, false),
            // RRX has no Thumb form
            Op2::Reg {
                kind: Shift::Ror,
                amount: 0,
                ..
            } => return self.unsupported(),
            Op2::Reg { rm, kind, amount } if rm != PC => (V::Shifted { rm, kind, amount }, false),
            Op2::RegShift { rm, kind, rs }
                if rm != PC && rs != PC && (rn != PC || !op.uses_rn()) =>
            {
                (V::ShiftReg { rm, kind, rs }, false)
            }
            _ => return self.unsupported(),
        };
        // MVN of a constant is a MOV of the inverted constant, the carry still comes from the
        // original immediate
        let carry_bit = matches!(v, V::Const(c) if c >> 31 != 0);
        let mut op = op;
        if op == DpOp::Mvn
            && let V::Const(c) = v
        {
            v = V::Const(!c);
            op = DpOp::Mov;
        }
        if self.dp_fast(op, s, rd, rn, v) {
            return;
        }
        if !op.is_test() && rd == SP {
            return self.unsupported();
        }

        // What the lowering looks like. The result of MOV, MVN and RSC is built in the result
        // register from operand 2, the 3 register forms are ADD, SUB and RSB with all flags, the
        // rest copies rn to the result register and operates on it.
        let ro_test = matches!(op, DpOp::Tst | DpOp::Cmp | DpOp::Cmn);
        let scratch = op == DpOp::Teq;
        let shifted = matches!(v, V::Shifted { .. } | V::ShiftReg { .. });
        // a second scratch register for a rotate amount or a shift register
        let aux = matches!(
            v,
            V::ShiftReg { .. }
                | V::Shifted {
                    kind: Shift::Ror,
                    ..
                }
        );
        // a rotated immediate decides the carry of a logical S op, the op itself keeps it
        let c_preset = s && op.is_logical() && rotated && matches!(v, V::Const(_));
        // the flags the Thumb ops always change
        let mut cl = match op {
            DpOp::Mov | DpOp::Add if !s => 0,
            DpOp::Mov | DpOp::Mvn => FNZ,
            _ if op.is_logical() => FNZ,
            _ => FALL,
        };
        if shifted || c_preset {
            cl |= FNZC;
        }
        // flags that are live and not set by the instruction are saved around it, otherwise the
        // lowering may change the dead ones as it likes
        let save = !self.can(cl);
        let nz_ok = save || self.can(FNZ);
        let all_ok = save || self.can(FALL);
        // the shift clobbers the carry the instruction reads
        let hazard = op.uses_carry_in() && shifted;

        let v_in_res = matches!(op, DpOp::Mov | DpOp::Mvn | DpOp::Rsc);
        let three = matches!(op, DpOp::Sub | DpOp::Rsb) || (op == DpOp::Add && (s || all_ok));
        let commutes = matches!(
            op,
            DpOp::And | DpOp::Eor | DpOp::Teq | DpOp::Orr | DpOp::Adc | DpOp::Add
        );
        // constants that are an instruction by themselves
        let imm_form = match (op, v) {
            (DpOp::Cmp, V::Const(c)) => c <= 0xFF,
            (DpOp::Add | DpOp::Sub, V::Const(c)) => three && c <= 0xFF,
            (DpOp::Rsb, V::Const(0)) => true,
            (DpOp::And, V::Const(0xFF | 0xFFFF)) => !c_preset,
            _ => false,
        };

        // The result goes to rd itself unless that is high or the sequence still reads it
        let rm_is_rd = matches!(v, V::Reg(rm) if rm == rd);
        let rs_is_rd = matches!(v, V::ShiftReg { rs, .. } if rs == rd);
        let clobbers_rd = if v_in_res {
            rs_is_rd || (op == DpOp::Rsc && rn == rd)
        } else if three {
            rm_is_rd && !is_low(rn)
        } else {
            // a commutative op can take the operands the other way round
            rm_is_rd && rn != rd && !(commutes && is_low(rn))
        };
        let direct = is_low(rd) && !ro_test && !scratch && !clobbers_rd;
        // a shifted operand 2 can be shifted where it is when nothing needs it afterwards
        let inplace = match v {
            V::Shifted { rm, .. } | V::ShiftReg { rm, .. }
                if !v_in_res
                    && self.dying & low_bit(rm) != 0
                    && (ro_test || rm != rd)
                    && !(op.uses_rn() && rm == rn) =>
            {
                Some(rm)
            }
            _ => None,
        };
        let vtmp = !v_in_res
            && inplace.is_none()
            && match v {
                V::Const(_) => !imm_form,
                V::Reg(rm) => !is_low(rm),
                _ => true,
            };
        // a commutative op can work in the register operand 2 was built in
        let fold = !direct
            && !ro_test
            && !three
            && !v_in_res
            && commutes
            && (vtmp || inplace.is_some())
            && is_low(rn);
        // rn as a source register of its own
        let ratmp = (ro_test || op == DpOp::Rsc) && !is_low(rn);
        let own_res = direct || ro_test || fold;

        let mut avoid = if op.uses_rn() { low_bit(rn) } else { 0 };
        if !ro_test {
            avoid |= low_bit(rd);
        }
        match v {
            V::Reg(rm) | V::Shifted { rm, .. } => avoid |= low_bit(rm),
            V::ShiftReg { rm, rs, .. } => avoid |= low_bit(rm) | low_bit(rs),
            V::Const(_) => {}
        }
        let clobbers = if hazard { cl | RESTORE_MID } else { cl };
        // rd is avoided, so it is not the destination frames favour
        let (f, [t_res, t_v, ta, tr]) = self.open([!own_res, vtmp, aux, ratmp], avoid, clobbers, 0);
        let res_tmp = if own_res { rd } else { t_res };
        let tv = match inplace {
            Some(rm) => rm,
            None if vtmp => t_v,
            None => res_tmp,
        };
        let res = if fold { tv } else { res_tmp };
        if direct && f.is_temp(rd) {
            return self
                .e
                .fail(JitError::Internal("frame took the destination"));
        }

        let vdst = if v_in_res { res } else { tv };
        let vr = match v {
            V::Const(c) => {
                if c_preset {
                    if carry_bit {
                        self.e.put(thumb::movs_imm(vdst, 1));
                        self.e.put(thumb::lsrs_imm(vdst, vdst, 1));
                    } else {
                        self.e.put(thumb::movs_imm(vdst, 0));
                        self.e.put(thumb::lsls_imm(vdst, vdst, 1));
                    }
                }
                if !imm_form {
                    self.mat_const(vdst, c, s && op == DpOp::Mov, nz_ok);
                }
                vdst
            }
            V::Reg(rm) => match op {
                DpOp::Mov if s && is_low(rm) && !f.is_temp(rm) => {
                    self.e.put(thumb::movs_reg(res, rm));
                    res
                }
                DpOp::Mov | DpOp::Rsc => {
                    self.get_copy(&f, rm, res);
                    if s && op == DpOp::Mov {
                        self.e.put(thumb::movs_reg(res, res));
                    }
                    res
                }
                DpOp::Mvn => self.get_ro(&f, rm, res),
                _ => self.get_ro(&f, rm, tv),
            },
            V::Shifted {
                rm,
                kind: Shift::Ror,
                amount,
            } => {
                self.get_copy(&f, rm, vdst);
                self.e.put(thumb::movs_imm(ta, amount as u16));
                self.e.put(thumb::alu(thumb::ROR, vdst, ta));
                vdst
            }
            V::Shifted { rm, kind, amount } => {
                let src = self.get_ro(&f, rm, vdst);
                self.shift_imm(kind, vdst, src, amount);
                vdst
            }
            V::ShiftReg { rm, kind, rs } => {
                self.get_copy(&f, rm, vdst);
                let amount = self.get_ro(&f, rs, ta);
                self.e.put(thumb::alu(thumb::shift_op(kind), vdst, amount));
                vdst
            }
        };
        if hazard {
            self.restore_flags(&f);
        }

        let c = if let V::Const(c) = v { c } else { 0 };
        match op {
            // built in res, the flags of a constant, copy or shift are already set
            DpOp::Mov => {}
            DpOp::Mvn => self.e.put(thumb::mvn(res, vr)),
            DpOp::Tst | DpOp::Cmp | DpOp::Cmn => {
                let ra = self.get_ro(&f, rn, tr);
                if imm_form {
                    self.e.put(thumb::cmp_imm(ra, c as u16));
                } else {
                    if let Some(code) = thumb::dp_op(op) {
                        self.e.put(thumb::alu(code, ra, vr));
                    }
                }
            }
            DpOp::Rsc => {
                let ra = self.get_ro(&f, rn, tr);
                self.e.put(thumb::alu(thumb::SBC, res, ra));
            }
            _ if three => {
                // rn itself when it is a low register, else copied to the result register
                let ra = self.get_ro(&f, rn, res);
                let add = op == DpOp::Add;
                match v {
                    V::Const(0) if op == DpOp::Rsb => self.e.put(thumb::neg(res, ra)),
                    V::Const(c) if imm_form && c <= 7 => self.e.put(if add {
                        thumb::adds_imm3(res, ra, c as u16)
                    } else {
                        thumb::subs_imm3(res, ra, c as u16)
                    }),
                    V::Const(c) if imm_form => {
                        if res != ra {
                            self.e.put(thumb::mov(res, ra));
                        }
                        self.e.put(if add {
                            thumb::adds_imm8(res, c as u16)
                        } else {
                            thumb::subs_imm8(res, c as u16)
                        });
                    }
                    _ => self.e.put(match op {
                        DpOp::Add => thumb::adds_reg(res, ra, vr),
                        DpOp::Sub => thumb::subs_reg(res, ra, vr),
                        _ => thumb::subs_reg(res, vr, ra),
                    }),
                }
            }
            DpOp::And if imm_form => {
                let ra = self.get_ro(&f, rn, res);
                self.e.put(if c == 0xFF {
                    thumb::uxtb(res, ra)
                } else {
                    thumb::uxth(res, ra)
                });
                if s {
                    self.e.put(thumb::movs_reg(res, res));
                }
            }
            _ => {
                // rd is operand 2 and also the result: operate on rn the other way round
                let b = if vr == res && rn != res {
                    rn
                } else {
                    self.get_copy(&f, rn, res);
                    vr
                };
                self.e.put(match thumb::dp_op(op) {
                    Some(code) => thumb::alu(code, res, b),
                    None => thumb::add_hi(res, b),
                });
            }
        }
        if !(direct || ro_test || scratch) {
            self.set(&f, rd, res);
        }
        self.close(f);
    }

    // Translations without a frame: the sp forms and register moves and adds that do not
    // touch the flags. Returns false when the generic path has to handle it.
    fn dp_fast(&mut self, op: DpOp, s: bool, rd: u8, rn: u8, v: V) -> bool {
        let low_add = self.can(FALL);
        let e = &mut self.e;
        match (op, v) {
            (DpOp::Mov, V::Reg(rm)) if !s => {
                if rd != rm {
                    e.put(thumb::mov(rd, rm));
                }
            }
            (DpOp::Add | DpOp::Sub, V::Const(c)) if !s => {
                let add = op == DpOp::Add;
                if rd == SP && rn == SP && c % 4 == 0 && c <= 4096 {
                    let mut left = c / 4;
                    while left > 0 {
                        let k = left.min(127) as u16;
                        e.put(if add {
                            thumb::add_sp(k)
                        } else {
                            thumb::sub_sp(k)
                        });
                        left -= k as u32;
                    }
                } else if add && rn == SP && is_low(rd) && c % 4 == 0 && c <= 1020 {
                    e.put(thumb::add_rd_sp(rd, (c / 4) as u16));
                } else {
                    return false;
                }
            }
            // ADD between any registers leaves the flags alone, but three low ones are one
            // instruction when it may set them
            (DpOp::Add, V::Reg(rm))
                if !s
                    && rn != PC
                    && rd != PC
                    && !(is_low(rd) && is_low(rn) && is_low(rm) && low_add) =>
            {
                if rd == rn {
                    e.put(thumb::add_hi(rd, rm));
                } else if rd == rm {
                    e.put(thumb::add_hi(rd, rn));
                } else {
                    e.put(thumb::mov(rd, rn));
                    e.put(thumb::add_hi(rd, rm));
                }
            }
            _ => return false,
        }
        true
    }

    // ---------------------------------------------------------------------------------------
    // status registers
    //
    // There are no modes or interrupt masks here, only the flags exist. Reads give the flags with
    // the system mode bits and writes only change the flags.

    fn mrs_insn(&mut self, rd: u8, spsr: bool) {
        if rd == PC || rd == SP {
            return self.unsupported();
        }
        let mode = Op2::Imm {
            value: 0x1F,
            rotated: false,
        };
        if spsr {
            // there is no saved status, hand out the mode bits alone
            return self.dp(DpOp::Mov, false, rd, 0, mode);
        }
        let (f, [t, m]) = self.open([true; 2], low_bit(rd), 0, self.dest);
        self.e.put_all(&thumb::mrs(t));
        self.e.lit(m, 0x1F);
        // the flag bits are the top of the word, adding sets the mode bits
        self.e.put(thumb::add_hi(t, m));
        self.set(&f, rd, t);
        self.close(f);
    }

    fn msr_insn(&mut self, spsr: bool, flags: bool, src: MsrSrc) {
        if spsr || !flags {
            return;
        }
        if let MsrSrc::Reg(rm) = src {
            if rm == PC {
                return self.unsupported();
            }
            if is_low(rm) {
                self.e.put_all(&thumb::msr(rm));
                return;
            }
        }
        let (f, [t]) = self.open([true], 0, 0, 0);
        match src {
            MsrSrc::Reg(rm) => self.get_copy(&f, rm, t),
            MsrSrc::Imm(v) => self.e.lit(t, v),
        }
        self.e.put_all(&thumb::msr(t));
        self.close(f);
    }

    // ---------------------------------------------------------------------------------------
    // multiply

    fn mul(&mut self, m: &MulOp) {
        let MulOp {
            acc,
            s,
            rd,
            rm,
            rs,
            rn,
        } = *m;
        if rd == PC || rm == PC || rs == PC || (acc && rn == PC) || rd == SP {
            return self.unsupported();
        }
        let all_low = is_low(rd) && is_low(rm) && is_low(rs);
        if !acc && all_low && self.can(FNZ) && (rd == rm || rd == rs) {
            let other = if rd == rm { rs } else { rm };
            self.e.put(thumb::alu(thumb::MUL, rd, other));
            return;
        }
        // muls always sets N and Z
        let (f, [p, o, ta]) = self.open(
            [true, true, acc],
            low_bit(rd) | low_bit(rm) | low_bit(rs) | low_bit(rn),
            FNZ,
            self.dest,
        );
        self.get_copy(&f, rm, p);
        let b = self.get_ro(&f, rs, o);
        self.e.put(thumb::alu(thumb::MUL, p, b));
        if acc {
            let a = self.get_ro(&f, rn, ta);
            self.e.put(thumb::add_hi(p, a));
            if s {
                self.e.put(thumb::movs_reg(p, p));
            }
        }
        self.set(&f, rd, p);
        self.close(f);
    }

    // The Cortex-M0+ has no long multiply. The 64 bit product is built from the four 16 bit
    // partial products, the signed forms correct the high word of the unsigned product.
    fn mul_long(&mut self, m: &MulLongOp) {
        let MulLongOp {
            signed,
            acc,
            s,
            rdlo,
            rdhi,
            rm,
            rs,
        } = *m;
        let bad = |r: u8| r == PC || r == SP;
        if s || bad(rdlo) || bad(rdhi) || bad(rm) || bad(rs) || rdlo == rdhi {
            return self.unsupported();
        }
        let (f, [a, b, c, d, e]) = self.open(
            [true; 5],
            low_bit(rdlo) | low_bit(rdhi) | low_bit(rm) | low_bit(rs),
            FALL,
            0,
        );
        use thumb::{ADC, AND, MUL};
        self.get_copy(&f, rm, a);
        self.get_copy(&f, rs, b);
        // a_lo, a_hi (c), b_lo, b_hi (d)
        self.e.put(thumb::lsrs_imm(c, a, 16));
        self.e.put(thumb::lsls_imm(a, a, 16));
        self.e.put(thumb::lsrs_imm(a, a, 16));
        self.e.put(thumb::lsrs_imm(d, b, 16));
        self.e.put(thumb::lsls_imm(b, b, 16));
        self.e.put(thumb::lsrs_imm(b, b, 16));
        // e = a_lo * b_hi, a = a_lo * b_lo, b = a_hi * b_lo, c = a_hi * b_hi
        self.e.put(thumb::mov(e, a));
        self.e.put(thumb::alu(MUL, e, d));
        self.e.put(thumb::alu(MUL, a, b));
        self.e.put(thumb::alu(MUL, b, c));
        self.e.put(thumb::alu(MUL, c, d));
        // e = the middle terms, its carry is worth 1 << 48
        self.e.put(thumb::adds_reg(e, e, b));
        self.e.put(thumb::movs_imm(b, 0));
        self.e.put(thumb::alu(ADC, b, b));
        self.e.put(thumb::lsls_imm(b, b, 16));
        self.e.put(thumb::adds_reg(c, c, b));
        // low word, its carry goes to the high word
        self.e.put(thumb::lsls_imm(b, e, 16));
        self.e.put(thumb::adds_reg(a, a, b));
        self.e.put(thumb::movs_imm(b, 0));
        self.e.put(thumb::alu(ADC, b, b));
        self.e.put(thumb::lsrs_imm(e, e, 16));
        self.e.put(thumb::adds_reg(c, c, e));
        self.e.put(thumb::adds_reg(c, c, b));
        if signed {
            // hi -= (rm < 0 ? rs : 0) + (rs < 0 ? rm : 0)
            self.get_copy(&f, rm, d);
            self.get_copy(&f, rs, e);
            self.e.put(thumb::asrs_imm(b, d, 31));
            self.e.put(thumb::alu(AND, b, e));
            self.e.put(thumb::subs_reg(c, c, b));
            self.e.put(thumb::asrs_imm(b, e, 31));
            self.e.put(thumb::alu(AND, b, d));
            self.e.put(thumb::subs_reg(c, c, b));
        }
        if acc {
            self.get_copy(&f, rdlo, d);
            self.get_copy(&f, rdhi, e);
            self.e.put(thumb::adds_reg(a, a, d));
            self.e.put(thumb::alu(ADC, c, e));
        }
        self.set(&f, rdlo, a);
        self.set(&f, rdhi, c);
        self.close(f);
    }

    // ---------------------------------------------------------------------------------------
    // single data transfer

    fn mem_access(&mut self, m: &MemOp) {
        let MemOp {
            load,
            kind,
            pre,
            up,
            rd,
            rn,
            off,
            ..
        } = *m;
        // the pc relative loads that can be done were turned into moves by block
        if rd == PC || rn == PC || (load && rd == SP) {
            return self.unsupported();
        }
        if rn == SP && self.sp_access(m) {
            return;
        }

        let size_shift = kind.size_shift();
        let update = m.update();
        // the base register is rewritten, unless the load of rd wins
        let writes_base = update && !(load && rd == rn);
        // a base with a known value gives a native address, and the new base, that are worked
        // out now, so the access needs no lookup and no offset
        let konst = self.const_address(m);
        // The offset is folded into the access when it fits, else it goes through a register
        // that is added to the address (pre) and to the base register (update).
        let mut imm5 = None;
        let mut need_to = false;
        // a post indexed immediate is added to the base register with one adds or subs
        let mut post_imm = None;
        // the flags the offset computation and the lookup change
        let translate = self.regions != 0 && rn != SP && konst.is_none();
        let mut cl = if translate { FALL } else { 0 };
        match off {
            _ if konst.is_some() => imm5 = Some(0),
            Offset::Imm(0) => imm5 = Some(0),
            Offset::Imm(i) => {
                if !update && up && i & ((1 << size_shift) - 1) == 0 && (i >> size_shift) <= 31 {
                    imm5 = Some((i >> size_shift) as u16);
                } else if !pre && is_low(rn) && i <= 0xFF && (!self.can(cl) || self.can(FALL)) {
                    imm5 = Some(0);
                    post_imm = Some(i as u16);
                } else {
                    need_to = true;
                }
            }
            Offset::Reg {
                kind: sk, amount, ..
            } => {
                if sk == Shift::Ror {
                    return self.unsupported();
                }
                need_to = true;
                if !(sk == Shift::Lsl && amount == 0) {
                    cl |= FNZC;
                }
                if !up {
                    cl |= FALL;
                }
            }
        }
        let save = !self.can(cl);
        let rm_bit = match off {
            Offset::Reg { rm, .. } => low_bit(rm),
            _ => 0,
        };
        // A pre indexed offset is added before the lookup, so the region is the one the access
        // lands in (ldr rd, [r0, #-8] from the IO base is an IWRAM mirror)
        let add_base = need_to && pre;
        let ea = translate && add_base;
        // temps: the base, the offset (also the table pointer), a copy of rn or the address, the data
        let want_to = translate || need_to;
        let want_rr = translate && (ea || !is_low(rn));
        let avoid = low_bit(rn) | low_bit(rd) | rm_bit;
        let temps = 1 + want_to as usize + want_rr as usize;
        // a data register when rd is high, or when the frame may have to borrow rd
        let want_td = !is_low(rd) || !fits(avoid, temps + save as usize);
        // A known address is built in the destination of a load into a low register, else in tn.
        // tr holds the new base when that is a high register and nothing else is free to.
        let low_load = load && is_low(rd);
        let (want, clobbers, pref) = if konst.is_some() {
            let want_tr = writes_base && !is_low(rn) && low_load;
            ([!low_load, false, want_tr, !load && !is_low(rd)], 0, 0)
        } else {
            ([true, want_to, want_rr, want_td], cl, self.dest)
        };
        let (f, [tn, to, tr, td]) = self.open(want, avoid, clobbers, pref);
        let tn = if want[0] { tn } else { rd };
        // rn is only borrowed when there is a td, the copy for the lookup then goes there
        let tr = if want[2] { tr } else { td };
        // add_to would use tn, which the load has overwritten with rd
        if (update && need_to && load && f.is_temp(rn) && tn == rd)
            || (post_imm.is_some() && f.is_temp(rn))
        {
            return self
                .e
                .fail(JitError::Internal("the pushed base is in the way"));
        }

        // the register the access goes through, and the register holding the signed offset
        let (base, to_reg) = if let Some((native, _)) = konst {
            self.e.lit(tn, native);
            (tn, to)
        } else if ea {
            let to_reg = self.mem_offset(&f, off, up, update, to);
            let rr = self.get_ro(&f, rn, tr);
            if rr != tr {
                self.e.put(thumb::mov(tr, rr));
            }
            self.e.put(thumb::add_hi(tr, to_reg));
            self.lookup(tn, to, tr);
            (tn, to_reg)
        } else {
            let base = if translate {
                let rr = self.get_ro(&f, rn, tr);
                self.lookup(tn, to, rr);
                tn
            } else if add_base {
                self.get_copy(&f, rn, tn);
                tn
            } else {
                self.get_ro(&f, rn, tn)
            };
            let to_reg = if need_to {
                self.mem_offset(&f, off, up, update, to)
            } else {
                to
            };
            if add_base {
                self.e.put(thumb::add_hi(base, to_reg));
            }
            (base, to_reg)
        };

        let i5 = imm5.unwrap_or(0);
        // a load into a borrowed rd goes through td, set puts it in rd's save slot
        let rt_direct = is_low(rd) && !f.is_temp(rd);
        let rt = if load && rt_direct {
            rd
        } else if load {
            if konst.is_some() { tn } else { td }
        } else {
            self.get_ro(&f, rd, td)
        };
        self.mem_op(load, kind, rt, base, i5);
        if load && !rt_direct {
            self.set(&f, rd, rt);
        }
        // the ARM base register gets the offset, if rd is the base the load wins
        if let Some(i) = post_imm {
            if !(load && rd == rn) {
                self.e.put(match up {
                    true => thumb::adds_imm8(rn, i),
                    false => thumb::subs_imm8(rn, i),
                });
            }
        } else if let Some((_, moved)) = konst {
            if writes_base {
                if is_low(rn) {
                    self.e.lit(rn, moved);
                } else {
                    let tb = if want[2] { tr } else { tn };
                    self.e.lit(tb, moved);
                    self.e.put(thumb::mov(rn, tb));
                }
            }
        } else if writes_base && need_to && rn != SP {
            if ea {
                self.set(&f, rn, tr);
            } else {
                self.add_to(&f, rn, to_reg, tn);
            }
        }
        self.close(f);
        // sp was only copied, writeback to it happens once nothing is pushed any more
        if rn == SP && update {
            match off {
                Offset::Imm(0) => {}
                Offset::Imm(i) if i % 4 == 0 && i / 4 <= 127 => {
                    let w = (i / 4) as u16;
                    self.e.put(if up {
                        thumb::add_sp(w)
                    } else {
                        thumb::sub_sp(w)
                    });
                }
                _ => self.unsupported(),
            }
        }
    }

    // A load or store of rt through base, the offset is already divided by the access size.
    // The signed loads sign extend what the plain ones give.
    fn mem_op(&mut self, load: bool, kind: MemKind, rt: u8, base: u8, imm5: u16) {
        let e = &mut self.e;
        match (load, kind) {
            (true, MemKind::Word) => e.put(thumb::ldr_imm(rt, base, imm5)),
            (false, MemKind::Word) => e.put(thumb::str_imm(rt, base, imm5)),
            (true, MemKind::Byte) => e.put(thumb::ldrb_imm(rt, base, imm5)),
            (false, MemKind::Byte) => e.put(thumb::strb_imm(rt, base, imm5)),
            (true, MemKind::Half) => e.put(thumb::ldrh_imm(rt, base, imm5)),
            (false, MemKind::Half) => e.put(thumb::strh_imm(rt, base, imm5)),
            (true, MemKind::SignedByte) => {
                e.put(thumb::ldrb_imm(rt, base, imm5));
                e.put(thumb::sxtb(rt, rt));
            }
            (true, MemKind::SignedHalf) => {
                e.put(thumb::ldrh_imm(rt, base, imm5));
                e.put(thumb::sxth(rt, rt));
            }
            // the decoder gives no signed stores
            (false, MemKind::SignedByte | MemKind::SignedHalf) => {
                e.fail(JitError::Internal("signed store"))
            }
        }
    }

    // The value of the word a pc relative load reads, as the load gives it. The pool is read
    // while translating, like the code is.
    fn pc_literal(&mut self, at: u32, kind: MemKind, up: bool, imm: u32) -> Option<u32> {
        let a = off_addr(at.wrapping_add(8), up, imm);
        let Some(w) = (self.mem)(a & !3) else {
            self.e.fail(JitError::BadRead(a));
            return None;
        };
        let shift = (a & 3) * 8;
        Some(match kind {
            MemKind::Word => w.rotate_right(shift),
            MemKind::Byte => (w >> shift) & 0xFF,
            MemKind::SignedByte => ((w >> shift) as u8 as i8 as i32) as u32,
            MemKind::Half => (w >> shift) & 0xFFFF,
            MemKind::SignedHalf => ((w >> shift) as u16 as i16 as i32) as u32,
        })
    }

    // ---------------------------------------------------------------------------------------
    // constants
    //
    // Registers that hold a known value (from a mov, a pc relative load, or arithmetic on those)
    // are followed through the block. An access through one of them goes to a native address
    // that is known while translating, when the region table says where the page is.

    fn known(&self, r: u8) -> Option<u32> {
        if r == PC {
            Some(self.pcv())
        } else {
            self.kc[r as usize]
        }
    }

    // the value of a data processing instruction whose operands are known
    fn fold(&self, op: DpOp, rn: u8, op2: Op2) -> Option<u32> {
        let b = match op2 {
            Op2::Imm { value, .. } => value,
            Op2::Reg { rm, kind, amount } => {
                let m = self.known(rm)?;
                match (kind, amount) {
                    (Shift::Lsl, a) => m.checked_shl(a as u32).unwrap_or(0),
                    (Shift::Lsr, 0) => 0,
                    (Shift::Lsr, a) => m >> a,
                    (Shift::Asr, 0) => ((m as i32) >> 31) as u32,
                    (Shift::Asr, a) => ((m as i32) >> a) as u32,
                    // RRX needs the carry
                    (Shift::Ror, 0) => return None,
                    (Shift::Ror, a) => m.rotate_right(a as u32),
                }
            }
            Op2::RegShift { .. } => return None,
        };
        let a = if op.uses_rn() { self.known(rn)? } else { 0 };
        Some(match op {
            DpOp::And => a & b,
            DpOp::Eor => a ^ b,
            DpOp::Sub => a.wrapping_sub(b),
            DpOp::Rsb => b.wrapping_sub(a),
            DpOp::Add => a.wrapping_add(b),
            DpOp::Orr => a | b,
            DpOp::Mov => b,
            DpOp::Bic => a & !b,
            DpOp::Mvn => !b,
            _ => return None,
        })
    }

    // update the known registers for an instruction that was just translated
    fn track(&mut self, d: &Decoded) {
        let mut new = [None; 2];
        if d.cond == COND_AL {
            match d.insn {
                Insn::Dp {
                    op, rd, rn, op2, ..
                } if !op.is_test() => {
                    new[0] = self.fold(op, rn, op2).map(|v| (rd, v));
                }
                Insn::Mem(MemOp {
                    load,
                    wb: true,
                    up,
                    rd,
                    rn,
                    off: Offset::Imm(i),
                    ..
                }) if rn != PC && !(load && rd == rn) => {
                    new[1] = self.known(rn).map(|v| (rn, off_addr(v, up, i)));
                }
                _ => {}
            }
        }
        // a BIOS call changes registers the decoder does not know about
        let written = if matches!(d.insn, Insn::Swi { .. }) {
            0xFFFF
        } else {
            regs_written(d)
        };
        for r in 0..16 {
            if written & (1 << r) != 0 {
                self.kc[r] = None;
            }
        }
        for (r, v) in new.into_iter().flatten() {
            self.kc[r as usize] = Some(v);
        }
    }

    // The native address of a GBA address when the region table tells, or None: the embedder
    // answers reads of the table through the memory callback. Pages that are not there yet and
    // the regions that fault have no address.
    fn native(&mut self, gba: u32) -> Option<u32> {
        if self.regions == 0 {
            return None;
        }
        let e = (self.mem)(self.regions + 4 * (gba >> 24))?;
        if e & 1 == 0 {
            return (e != 0).then(|| gba.wrapping_add(e));
        }
        // EWRAM pages come and go, a native address of one must not end up in code
        if gba >> 24 == 2 {
            return None;
        }
        let page = (self.mem)((e & !1) + 4 * ((gba >> 9) & 0x1FF))?;
        // a page that is not there points at its own GBA address
        (page >> 24 != gba >> 24).then(|| page + (gba & 0x1FF))
    }

    // The native address an access through a register with a known value goes to, and the value
    // the base register gets, when the region table tells.
    fn const_address(&mut self, m: &MemOp) -> Option<(u32, u32)> {
        let (Some(base), Offset::Imm(imm)) = (self.kc[m.rn as usize], m.off) else {
            return None;
        };
        if m.rn == SP {
            return None;
        }
        let moved = off_addr(base, m.up, imm);
        let ea = if m.pre { moved } else { base };
        // an unaligned word or halfword is not a plain access
        if ea & ((1 << m.kind.size_shift()) - 1) != 0 {
            return None;
        }
        Some((self.native(ea)?, moved))
    }

    // tn = the native address of the GBA address in rr. The table entry for its top byte is
    // either an even offset to add, or an odd page table address: 512 entries for the 512 byte
    // pages of a 256 KiB window that repeats, each the native address of its page. tb is a
    // scratch register.
    fn lookup(&mut self, tn: u8, tb: u8, rr: u8) {
        self.e.put(thumb::lsrs_imm(tn, rr, 24));
        self.e.put(thumb::lsls_imm(tn, tn, 2));
        self.e.lit(tb, self.regions);
        self.e.put(thumb::ldr_reg(tn, tb, tn));
        // the carry is bit 0, clear for an offset
        self.e.put(thumb::lsrs_imm(tb, tn, 1));
        let flat = self.e.fwd_cond(3);
        // tn = page - (rr & !511), so that adding rr below lands in the page
        self.e.put(thumb::lsls_imm(tn, tb, 1));
        self.e.put(thumb::lsls_imm(tb, rr, 14));
        self.e.put(thumb::lsrs_imm(tb, tb, 23));
        self.e.put(thumb::lsls_imm(tb, tb, 2));
        self.e.put(thumb::ldr_reg(tn, tn, tb));
        self.e.put(thumb::lsrs_imm(tb, rr, 9));
        self.e.put(thumb::lsls_imm(tb, tb, 9));
        self.e.put(thumb::subs_reg(tn, tn, tb));
        self.e.bind(flat);
        self.e.put(thumb::add_hi(tn, rr));
    }

    // The signed offset of a load or store in a low register, to when it needs one. A register
    // offset is used in place when nothing reads it after the access.
    fn mem_offset(&mut self, f: &Frame, off: Offset, up: bool, update: bool, to: u8) -> u8 {
        match off {
            Offset::Imm(i) => {
                self.e.lit(to, if up { i } else { i.wrapping_neg() });
                to
            }
            Offset::Reg {
                rm,
                kind: Shift::Lsl,
                amount: 0,
            } if up && !update => self.get_ro(f, rm, to),
            Offset::Reg { rm, kind, amount } => {
                let src = self.get_ro(f, rm, to);
                if kind == Shift::Lsl && amount == 0 {
                    self.e.put(if up {
                        thumb::mov(to, src)
                    } else {
                        thumb::neg(to, src)
                    });
                } else {
                    self.shift_imm(kind, to, src, amount);
                    if !up {
                        self.e.put(thumb::neg(to, to));
                    }
                }
                to
            }
        }
    }

    // The sp forms that need no translation of the address. Returns false for the rest.
    fn sp_access(&mut self, m: &MemOp) -> bool {
        let MemOp {
            load,
            kind,
            pre,
            up,
            wb,
            rd,
            off,
            ..
        } = *m;
        let Offset::Imm(i) = off else {
            return false;
        };
        let word = kind == MemKind::Word;
        // str rt, [sp, #-4]! is a push, ldr rt, [sp], #4 is a pop
        if word && i == 4 {
            if !load && pre && !up && wb && (is_low(rd) || rd == LR) {
                let low = if is_low(rd) { 1u8 << rd } else { 0 };
                self.e.put(thumb::push(low, rd == LR));
                return true;
            }
            if load && !pre && up && is_low(rd) {
                self.e.put(thumb::pop(1 << rd, false));
                return true;
            }
        }
        // Scratch registers are pushed just below sp, so data below sp has to move sp first
        if pre && !up && i != 0 {
            let words = i.div_ceil(4);
            if words > 127 || (wb && i % 4 != 0) {
                self.unsupported();
                return true;
            }
            self.e.put(thumb::sub_sp(words as u16));
            self.mem_access(&MemOp {
                pre: true,
                up: true,
                wb: false,
                off: Offset::Imm(words * 4 - i),
                ..*m
            });
            if !wb {
                self.e.put(thumb::add_sp(words as u16));
            }
            return true;
        }
        if word && is_low(rd) && pre && up && !wb && i % 4 == 0 && i / 4 <= 255 {
            let w = (i / 4) as u16;
            self.e.put(if load {
                thumb::ldr_sp(rd, w)
            } else {
                thumb::str_sp(rd, w)
            });
            return true;
        }
        false
    }

    // ---------------------------------------------------------------------------------------
    // block data transfer

    fn block_transfer(&mut self, b: &BlockOp) {
        let BlockOp {
            load,
            pre,
            up,
            wb,
            rn,
            list,
        } = *b;
        let n = list.count_ones();
        // the pc is only loaded by pc_write, sp can be stored through another base but not loaded
        let sp_in_list = list & (1 << SP) != 0;
        if n == 0 || rn == PC || list & (1 << PC) != 0 || (sp_in_list && (load || rn == SP)) {
            return self.unsupported();
        }
        let low_list = list as u8;
        let all_low = list & 0xFF00 == 0;

        if rn == SP {
            if wb && !load && !up && pre {
                if list & !(0xFF | 1 << LR) == 0 {
                    self.e.put(thumb::push(low_list, list & (1 << LR) != 0));
                } else {
                    self.push_general(list);
                }
            } else if wb && load && up && !pre {
                if all_low {
                    self.e.put(thumb::pop(low_list, false));
                } else {
                    self.pop_general(list);
                }
            } else {
                self.unsupported();
            }
            return;
        }

        // the lowest address is transferred first in every mode
        let nb = 4 * n as i32;
        let (start, delta) = match (up, pre) {
            (true, false) => (0, nb),
            (true, true) => (4, nb),
            (false, false) => (4 - nb, -nb),
            (false, true) => (-nb, -nb),
        };
        let base_in_list = list & (1 << rn) != 0;
        let update = wb && !(load && base_in_list);
        let translate = self.regions != 0;
        let clobbers = if translate { FALL } else { 0 };
        let save = !self.can(clobbers);
        let used = low_bit(rn) | low_list;
        // What the frame holds: tn, to (the lookup scratch, also the offset and the register
        // that other registers go through), a copy of rn for the lookup when it is high or moved
        // by an offset, and the flags. ldmia/stmia need every scratch register outside the list.
        let plan = |add_imm: bool| {
            let to = translate || start != 0 || (update && !add_imm);
            let fixed = 1 + to as usize + save as usize;
            let rr = translate && (!is_low(rn) || start != 0);
            let fast = all_low && fits(used, fixed + rr as usize);
            let td = !fast && !to;
            // when the frame has to borrow registers rn can be one of them, the lookup then
            // reads it through a copy
            let demand = fixed + rr as usize + td as usize;
            let rr = rr || (translate && !fits(used, demand));
            Plan {
                to,
                rr,
                td,
                fast,
                demand,
            }
        };
        // the base register moves with an adds or subs when that may set the flags and no
        // register has to be borrowed for it
        let mut add_imm = update && is_low(rn) && (save || self.can(FALL));
        let mut p = plan(add_imm);
        if add_imm && !fits(used, p.demand) {
            add_imm = false;
            p = plan(false);
        }
        let fast = p.fast;
        let (f, [tn, to, tr, td]) = self.open([true, p.to, p.rr, p.td], used, clobbers, self.dest);
        let td = if p.td { td } else { to };

        if translate {
            // an offset moves the base before the lookup, like for a single transfer
            let rr = if start != 0 {
                self.e.lit(to, start as u32);
                self.get_copy(&f, rn, tr);
                self.e.put(thumb::add_hi(tr, to));
                tr
            } else {
                self.get_ro(&f, rn, tr)
            };
            self.lookup(tn, to, rr);
        } else {
            self.get_copy(&f, rn, tn);
            if start != 0 {
                self.e.lit(to, start as u32);
                self.e.put(thumb::add_hi(tn, to));
            }
        }
        if fast {
            if f.mask & low_list != 0 {
                return self
                    .e
                    .fail(JitError::Internal("ldm temps overlap the list"));
            }
            self.e.put(if load {
                thumb::ldmia(tn, low_list)
            } else {
                thumb::stmia(tn, low_list)
            });
        } else {
            let mut k = 0;
            for r in 0..15u8 {
                if list & (1 << r) == 0 {
                    continue;
                }
                if is_low(r) && !f.is_temp(r) {
                    self.e.put(if load {
                        thumb::ldr_imm(r, tn, k)
                    } else {
                        thumb::str_imm(r, tn, k)
                    });
                } else if load {
                    self.e.put(thumb::ldr_imm(td, tn, k));
                    self.set(&f, r, td);
                } else {
                    self.get_copy(&f, r, td);
                    self.e.put(thumb::str_imm(td, tn, k));
                }
                k += 1;
            }
        }
        if add_imm {
            self.e.put(if delta > 0 {
                thumb::adds_imm8(rn, delta as u16)
            } else {
                thumb::subs_imm8(rn, -delta as u16)
            });
        } else if update {
            self.e.lit(to, delta as u32);
            self.add_to(&f, rn, to, tn);
        }
        self.close(f);
    }

    // stmdb sp!, {list} with registers push cannot reach
    // The low registers sit below the high ones on the stack. The high registers go through the
    // lowest low register of the list, which is free to use once stored and before it is loaded.
    // Without one r0 is borrowed and kept in an extra word under the data.
    fn push_general(&mut self, list: u16) {
        let l = ListSplit::new(list);
        self.e.put(thumb::sub_sp(l.nh));
        self.e.put(thumb::push(l.pushed(), false));
        let mut k = l.words();
        for r in 8..15u8 {
            if list & (1 << r) != 0 {
                self.e.put(thumb::mov(l.t, r));
                self.e.put(thumb::str_sp(l.t, k));
                k += 1;
            }
        }
        if l.borrow {
            self.e.put(thumb::pop(1 << l.t, false));
        } else {
            self.e.put(thumb::ldr_sp(l.t, 0));
        }
    }

    // ldmia sp!, {list} with registers pop cannot reach, the list has no pc
    fn pop_general(&mut self, list: u16) {
        let l = ListSplit::new(list);
        if l.borrow {
            self.e.put(thumb::push(1 << l.t, false));
        }
        let mut k = l.words();
        for r in 8..15u8 {
            if list & (1 << r) != 0 {
                self.e.put(thumb::ldr_sp(l.t, k));
                self.e.put(thumb::mov(r, l.t));
                k += 1;
            }
        }
        // the low registers last, t gets its own value back
        self.e.put(thumb::pop(l.pushed(), false));
        self.e.put(thumb::add_sp(l.nh));
    }
}

// A register list as push_general and pop_general see it
struct ListSplit {
    low: u8,
    nl: u16,
    // how many high registers
    nh: u16,
    // the list has no low register, so r0 is borrowed
    borrow: bool,
    // the low register the high ones go through
    t: u8,
}

impl ListSplit {
    fn new(list: u16) -> Self {
        let low = list as u8;
        let borrow = low == 0;
        ListSplit {
            low,
            nl: low.count_ones() as u16,
            nh: (list >> 8).count_ones() as u16,
            borrow,
            t: if borrow {
                0
            } else {
                low.trailing_zeros() as u8
            },
        }
    }

    // the low registers on the stack, with the borrowed one
    fn pushed(&self) -> u8 {
        self.low | self.borrow as u8
    }

    // the words they take, which is where the high registers' slots start
    fn words(&self) -> u16 {
        self.nl + self.borrow as u16
    }
}

fn off_addr(base: u32, up: bool, imm: u32) -> u32 {
    if up {
        base.wrapping_add(imm)
    } else {
        base.wrapping_sub(imm)
    }
}

fn writes_pc(insn: &Insn) -> bool {
    match *insn {
        Insn::Dp { op, rd, .. } => !op.is_test() && rd == PC,
        Insn::Mem(m) => m.load && m.rd == PC,
        Insn::Block(b) => b.load && b.list & (1 << 15) != 0,
        _ => false,
    }
}

fn is_terminator(d: &Decoded) -> bool {
    matches!(
        d.insn,
        Insn::Branch { .. } | Insn::Bx { .. } | Insn::Unsupported
    ) || writes_pc(&d.insn)
}

// The flags the instruction sets for sure, and the ones it sets or leaves alone depending on the
// operands (a logical S op with a register shift gets C from the shifter unless the amount is 0).
// Logical S ops keep C and V otherwise, multiplies leave C and V alone.
fn flags_written(d: &Decoded) -> (u8, u8) {
    match d.insn {
        Insn::Dp {
            op, s: true, op2, ..
        } if op.is_logical() => match op2 {
            Op2::Imm { rotated, .. } => (FNZ | if rotated { FC } else { 0 }, 0),
            Op2::Reg {
                kind: Shift::Lsl,
                amount: 0,
                ..
            } => (FNZ, 0),
            Op2::Reg { .. } => (FNZC, 0),
            Op2::RegShift { .. } => (FNZ, FC),
        },
        Insn::Dp { s: true, .. } => (FALL, 0),
        Insn::Mul(MulOp { s: true, .. }) => (FNZ, 0),
        Insn::Msr { spsr, flags, .. } if flags && !spsr => (FALL, 0),
        _ => (0, 0),
    }
}

// The flags the instruction needs as they are, apart from its condition
fn flags_read(d: &Decoded) -> u8 {
    match d.insn {
        Insn::Dp { op, s, op2, .. } => {
            let shifts = matches!(op2, Op2::RegShift { .. });
            if op.uses_carry_in() || (s && op.is_logical() && shifts) {
                FC
            } else {
                0
            }
        }
        Insn::Mrs { .. } => FALL,
        _ => 0,
    }
}

// The flags and registers live before an instruction, given the ones live after it. Its
// condition is read unless a skip shared with the instruction before it already tests it, and a
// conditional instruction kills nothing because it may not run.
fn step(d: &Decoded, tests_cond: bool, flags: u8, regs: u16) -> (u8, u16) {
    let mut reads = flags_read(d);
    let (mut kills, mut rkills) = (flags_written(d).0, regs_written(d));
    if d.cond != COND_AL {
        if tests_cond {
            reads |= cond_flags(d.cond);
        }
        (kills, rkills) = (0, 0);
    }
    (reads | (flags & !kills), regs_read(d) | (regs & !rkills))
}

// the flags a condition code reads
fn cond_flags(cond: u8) -> u8 {
    match cond {
        0 | 1 => FZ,
        2 | 3 => FC,
        4 | 5 => FN,
        6 | 7 => FV,
        8 | 9 => FC | FZ,
        10 | 11 => FN | FV,
        12 | 13 => FN | FZ | FV,
        _ => 0,
    }
}

// the registers (bit r for rN, pc included) an instruction reads
fn regs_read(d: &Decoded) -> u16 {
    let bit = |r: u8| 1u16 << r;
    match d.insn {
        Insn::Dp { op, rn, op2, .. } => {
            let o = match op2 {
                Op2::Imm { .. } => 0,
                Op2::Reg { rm, .. } => bit(rm),
                Op2::RegShift { rm, rs, .. } => bit(rm) | bit(rs),
            };
            o | if op.uses_rn() { bit(rn) } else { 0 }
        }
        Insn::Mul(m) => bit(m.rm) | bit(m.rs) | if m.acc { bit(m.rn) } else { 0 },
        Insn::MulLong(m) => {
            bit(m.rm) | bit(m.rs) | if m.acc { bit(m.rdlo) | bit(m.rdhi) } else { 0 }
        }
        Insn::Mem(m) => {
            let o = match m.off {
                Offset::Imm(_) => 0,
                Offset::Reg { rm, .. } => bit(rm),
            };
            o | bit(m.rn) | if m.load { 0 } else { bit(m.rd) }
        }
        Insn::Block(b) => bit(b.rn) | if b.load { 0 } else { b.list },
        Insn::Msr {
            src: MsrSrc::Reg(rm),
            ..
        } => bit(rm),
        Insn::Bx { rm } => bit(rm),
        Insn::Mrs { .. } | Insn::Msr { .. } | Insn::Branch { .. } => 0,
        // the callee or the BIOS may use anything
        Insn::Swi { .. } | Insn::Unsupported => 0xFFFF,
    }
}

// the registers an instruction writes whenever it executes
fn regs_written(d: &Decoded) -> u16 {
    let bit = |r: u8| 1u16 << r;
    match d.insn {
        Insn::Dp { op, rd, .. } => {
            if op.is_test() {
                0
            } else {
                bit(rd)
            }
        }
        Insn::Mul(MulOp { rd, .. }) | Insn::Mrs { rd, .. } => bit(rd),
        Insn::MulLong(m) => bit(m.rdlo) | bit(m.rdhi),
        Insn::Mem(m) => (if m.load { bit(m.rd) } else { 0 }) | if m.wb { bit(m.rn) } else { 0 },
        Insn::Block(b) => (if b.load { b.list } else { 0 }) | if b.wb { bit(b.rn) } else { 0 },
        Insn::Branch { link: true, .. } => bit(LR),
        _ => 0,
    }
}
