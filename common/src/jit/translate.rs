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
    decode::{COND_AL, Decoded, DpOp, Insn, MemKind, MsrSrc, Offset, Op2, Shift, decode},
    emit::Emit,
    thumb::{self, LR, PC, SP, is_low},
};

// Longest run of ARM instructions translated into a single block
const MAX_BLOCK: usize = 48;

// How many instructions after a block are looked at to see which registers they read
const LOOKAHEAD: usize = 16;

// Look at the code a block exits to, so registers it does not read are free scratch registers at
// the end of the block. The differential check against Unicorn compares every register at the end
// of a block, so it reports the clobbered dead ones as mismatches: off until that knows about it.
const EXIT_LOOKAHEAD: bool = false;

// Flag sets, as masks
const FN: u8 = 8;
const FZ: u8 = 4;
const FC: u8 = 2;
const FV: u8 = 1;
const FNZ: u8 = FN | FZ;
const FNZC: u8 = FNZ | FC;
const FALL: u8 = FNZC | FV;

// The pool gets flushed when it has this little room left for the next instruction
const POOL_RESERVE: usize = 8;

pub struct Tr<'a, 'm> {
    pub e: Emit<'a>,
    // the registers and flags the code after the last block translated may read
    pub live_out: (u16, u8),
    mem: &'m mut dyn FnMut(u32) -> Option<u32>,
    // native address of the region offset table, 0 translates nothing
    regions: u32,
    // every conditional instruction gets its own skip, with a branch that always reaches
    long_skips: bool,
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
    // the destination register when it is free (it is written anyway), taken first by frames
    pref: u8,
    // low registers the instruction reads and nothing after it does
    dying: u8,
    // the flags the instruction reads, apart from its condition
    flags_in: u8,
    // the halfword the block being translated starts at and its ARM address
    start_hw: usize,
    start: u32,
    // registers known to hold a constant at this point of the block
    kc: [Option<u32>; 16],
    // the frame that was closed last, to open the next one without a pop and a push in between
    closed: Option<Closed>,
}

// where the code that closed a frame starts and ends, and what it popped
#[derive(Clone, Copy)]
struct Closed {
    start: usize,
    end: usize,
    mask: u8,
    flags: Option<u8>,
}

// The scratch registers of one instruction
#[derive(Clone, Copy)]
struct Frame {
    regs: [u8; 6],
    mask: u8,
    flags: Option<u8>,
    // put the flags back when the frame closes
    restore: bool,
}

impl Frame {
    fn t(&self, i: usize) -> u8 {
        self.regs[i]
    }

    fn is_temp(&self, r: u8) -> bool {
        r < 8 && self.mask & (1 << r) != 0
    }

    // The word the original value of temp r was pushed to. push stores the lowest register at
    // the lowest address and sp points at it.
    fn slot(&self, r: u8) -> u16 {
        (self.mask & ((1u8 << r) - 1)).count_ones() as u16
    }
}

fn bit(r: u8) -> u8 {
    if is_low(r) { 1 << r } else { 0 }
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
    ) -> Self {
        Tr {
            e,
            live_out: (0xFFFF, FALL),
            mem,
            regions,
            long_skips,
            pushed: 0,
            addr: 0,
            live: FALL,
            own: 0,
            free: 0,
            pref: 0,
            dying: 0,
            flags_in: 0,
            start_hw: 0,
            start: 0,
            kc: [None; 16],
            closed: None,
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

    // Open a frame with n scratch registers, plus one for the flags when they are saved. Free
    // registers (see Tr::free) cost nothing, the others are pushed. Registers the instruction
    // uses (avoid) are only taken when there is nothing else, get and set cope with that.
    fn open(&mut self, n: usize, avoid: u8, save_flags: bool, restore_flags: bool) -> Frame {
        let total = n + save_flags as usize;
        let mut f = Frame {
            regs: [0; 6],
            mask: 0,
            flags: None,
            restore: save_flags && restore_flags,
        };
        if total > 6 {
            self.e.fail(JitError::Internal("frame too big"));
            return f;
        }
        let free = (self.free & !avoid) | self.pref;
        // the destination goes first in every pass
        let mut order = [0u8; 8];
        order[0] = self.pref.trailing_zeros().min(7) as u8;
        let mut m = if self.pref != 0 { 1 } else { 0 };
        for r in 0..8u8 {
            if self.pref != 1 << r {
                order[m] = r;
                m += 1;
            }
        }
        let mut k = 0;
        for pass in 0..3 {
            for &r in &order {
                let is_free = free & (1 << r) != 0;
                let avoided = avoid & (1 << r) != 0;
                let want = match pass {
                    0 => is_free,
                    1 => !is_free && !avoided,
                    _ => !is_free && avoided,
                };
                if want && k < total {
                    if k < n {
                        f.regs[k] = r;
                    } else {
                        f.flags = Some(r);
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
        if let Some(c) = self.closed.take()
            && f.mask != 0
            && c.end == self.e.len
            && !self.e.at_barrier()
            && c.mask == f.mask
            && c.flags == f.flags
            && (f.flags.is_none() || self.flags_in == 0)
        {
            self.e.len = c.start;
            self.pushed += f.mask.count_ones() as u16;
            return f;
        }
        if f.mask != 0 {
            self.e.put(thumb::push(f.mask, false));
            self.pushed += f.mask.count_ones() as u16;
        }
        if let Some(t) = f.flags {
            self.e.put_all(&thumb::mrs(t));
        }
        f
    }

    fn close(&mut self, f: Frame) {
        let start = self.e.len;
        if f.restore {
            self.restore_flags(&f);
        }
        if f.mask != 0 {
            self.e.put(thumb::pop(f.mask, false));
            self.pushed -= f.mask.count_ones() as u16;
            self.closed = Some(Closed {
                start,
                end: self.e.len,
                mask: f.mask,
                flags: f.flags.filter(|_| f.restore),
            });
        }
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

    // ARM register r = the low register src
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
            self.e.put(thumb::alu(thumb::MVN, t, t));
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
            let d = decode(word, a);
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
                && !writes_flags(p);
        }
        // The flags and registers live after each instruction, one backward pass starting from
        // what the code after the block reads. A branch back into the block adds what is live
        // at its target and the pass is repeated.
        let (mut out_r, mut out_f, back) = self.exit_live(start, &insns[n - 1], a, ends);
        let mut live = [0u8; MAX_BLOCK];
        let mut rlive = [0u16; MAX_BLOCK];
        loop {
            let (mut l, mut rl) = (out_f, out_r);
            let mut target = (0, 0);
            for i in (0..n).rev() {
                live[i] = l;
                rlive[i] = rl;
                let d = &insns[i];
                let mut reads = flags_read(d);
                if d.cond != COND_AL && !in_group[i] {
                    reads |= cond_flags(d.cond);
                }
                let (kills, rkills) = if d.cond == COND_AL {
                    (flags_written(d).0, regs_written(d))
                } else {
                    (0, 0)
                };
                l = reads | (l & !kills);
                rl = regs_read(d) | (rl & !rkills);
                if back == Some(i) {
                    target = (rl, l);
                }
            }
            if back.is_none() || (target.0 & !out_r == 0 && target.1 & !out_f == 0) {
                break;
            }
            out_r |= target.0;
            out_f |= target.1;
        }
        self.live_out = (out_r, out_f);

        self.kc = [None; 16];
        self.start_hw = self.e.len;
        self.start = start;
        let mut skip: Option<(usize, bool)> = None;
        for i in 0..n {
            let d = insns[i];
            self.addr = start.wrapping_add(4 * i as u32);
            self.live = live[i];
            self.own = flags_written(&d).0 | flags_written(&d).1;
            let read = regs_read(&d);
            // the destination of these is written whatever it held
            self.pref = match d.insn {
                Insn::Dp { .. }
                | Insn::Mul { .. }
                | Insn::Mem { load: true, .. }
                | Insn::Mrs { .. } => (regs_written(&d) & !read) as u8,
                _ => 0,
            };
            self.free = !(rlive[i] | read) as u8 | self.pref;
            self.dying = (read & !rlive[i]) as u8;
            self.flags_in = flags_read(&d);
            let grouped = d.cond != COND_AL && !is_terminator(&d);
            if grouped && !in_group[i] {
                skip = Some(self.skip_open(d.cond));
            }
            self.insn(d);
            self.track(&d);
            if grouped && (i + 1 == n || !in_group[i + 1]) {
                self.skip_close(skip.take().unwrap());
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
            if skip.is_none()
                && i + 1 < n
                && (self.e.pool_pressure() > 240 || self.e.pool_room() < POOL_RESERVE)
            {
                self.e.flush_pool(true);
            }
        }
        if !ends {
            self.e.slot(a);
        }
        self.e.flush_pool(false);
        a
    }

    // The registers and flags the code after a block of n instructions reads, and the index of
    // the instruction its last branch goes back to when that is inside the block.
    fn exit_live(
        &mut self,
        start: u32,
        last: &Decoded,
        next: u32,
        ends: bool,
    ) -> (u16, u8, Option<usize>) {
        if !EXIT_LOOKAHEAD {
            return (0xFFFF, FALL, None);
        }
        if !ends {
            let (r, f) = self.scan_live(next);
            return (r, f, None);
        }
        let Insn::Branch {
            link: false,
            target,
        } = last.insn
        else {
            return (0xFFFF, FALL, None);
        };
        let (mut r, mut f) = (0, 0);
        let mut back = None;
        if (start..next).contains(&target) && target & 3 == start & 3 {
            back = Some(((target - start) / 4) as usize);
        } else {
            (r, f) = self.scan_live(target);
        }
        if last.cond != COND_AL {
            let (r2, f2) = self.scan_live(next);
            (r, f) = (r | r2, f | f2);
        }
        (r, f, back)
    }

    // The registers and flags the straight line code from addr reads before it writes them.
    // Anything it cannot see (branches, unreadable memory) counts as read.
    fn scan_live(&mut self, addr: u32) -> (u16, u8) {
        let mut insns = [Decoded {
            cond: 0,
            insn: Insn::Unsupported,
        }; LOOKAHEAD];
        let mut n = 0;
        let mut a = addr;
        while n < LOOKAHEAD {
            let Some(word) = (self.mem)(a) else {
                break;
            };
            insns[n] = decode(word, a);
            n += 1;
            a = a.wrapping_add(4);
            if is_terminator(&insns[n - 1]) {
                break;
            }
        }
        let (mut r, mut f) = (0xFFFF, FALL);
        for d in insns[..n].iter().rev() {
            let (mut reads, mut kills, mut rkills) = (flags_read(d), 0, 0);
            if d.cond == COND_AL {
                kills = flags_written(d).0;
                rkills = regs_written(d);
            } else {
                reads |= cond_flags(d.cond);
            }
            f = reads | (f & !kills);
            r = regs_read(d) | (r & !rkills);
        }
        (r, f)
    }

    fn insn(&mut self, d: Decoded) {
        match d.insn {
            Insn::Unsupported => self.unsupported(),
            Insn::Branch { link, target } => self.branch(d.cond, link, target),
            Insn::Bx { rm } => self.bx(d.cond, rm),
            _ if writes_pc(&d.insn) => self.pc_write(d),
            Insn::Dp { op, s, rd, rn, op2 } => self.dp(op, s, rd, rn, op2),
            Insn::Mul {
                acc,
                s,
                rd,
                rm,
                rs,
                rn,
            } => self.mul(acc, s, rd, rm, rs, rn),
            Insn::Mem {
                load,
                kind,
                pre,
                up,
                wb,
                rd,
                rn,
                off,
            } => self.mem_access(load, kind, pre, up, wb, rd, rn, off),
            Insn::Block {
                load,
                pre,
                up,
                wb,
                rn,
                list,
            } => self.block_transfer(load, pre, up, wb, rn, list),
            Insn::Swi { num } => self.e.put(thumb::svc(num)),
            Insn::Mrs { rd, spsr } => self.mrs_insn(rd, spsr),
            Insn::Msr { spsr, flags, src } => self.msr_insn(spsr, flags, src),
        }
    }

    // Branch over the translation of conditional instructions when the condition fails
    fn skip_open(&mut self, cond: u8) -> (usize, bool) {
        if self.long_skips {
            // b<cond> over the following long branch, b skips the instruction
            self.e.put(thumb::b_cond(cond, 0));
            (self.e.fwd(), true)
        } else {
            (self.e.fwd_cond(cond ^ 1), false)
        }
    }

    fn skip_close(&mut self, (at, long): (usize, bool)) {
        if long {
            self.e.bind(at);
        } else {
            self.e.bind_cond(at);
        }
    }

    // ---------------------------------------------------------------------------------------
    // branches

    fn branch(&mut self, cond: u8, link: bool, target: u32) {
        let next = self.addr.wrapping_add(4);
        if !link {
            // a loop over the whole block jumps back inside it, if that is in reach
            let back = |ahead: usize| self.start_hw as i32 - (self.e.len + ahead + 2) as i32;
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
            } => {
                let rest = list & 0x7FFF;
                if rest & (1 << SP) != 0 {
                    self.unsupported();
                } else if rest & !0xFF == 0 {
                    // pop {low registers, pc}
                    self.e.put(thumb::pop(rest as u8, true));
                } else {
                    // high registers as well: pop those first, then the pc
                    self.pop_general(rest);
                    self.e.put(thumb::pop(0, true));
                }
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
        let save = cl & self.live & !self.own != 0;
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
                    && self.dying & bit(rm) != 0
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
        let n =
            !(direct || ro_test || fold) as usize + vtmp as usize + aux as usize + ratmp as usize;

        let mut avoid = if op.uses_rn() { bit(rn) } else { 0 };
        if !ro_test {
            avoid |= bit(rd);
        }
        match v {
            V::Reg(rm) | V::Shifted { rm, .. } => avoid |= bit(rm),
            V::ShiftReg { rm, rs, .. } => avoid |= bit(rm) | bit(rs),
            V::Const(_) => {}
        }
        self.pref = 0;
        let f = self.open(n, avoid, save || hazard, save);
        let mut k = 0;
        let mut next = || {
            k += 1;
            f.t(k - 1)
        };
        let res_tmp = if direct || ro_test || fold {
            rd
        } else {
            next()
        };
        let tv = match inplace {
            Some(rm) => rm,
            None if vtmp => next(),
            None => res_tmp,
        };
        let res = if fold { tv } else { res_tmp };
        let ta = if aux { next() } else { 0 };
        let tr = if ratmp { next() } else { 0 };
        if direct && f.is_temp(rd) {
            return self
                .e
                .fail(JitError::Internal("frame took the destination"));
        }

        // operand 2, in vr
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
                self.e.put(thumb::alu(
                    match kind {
                        Shift::Lsl => thumb::LSL,
                        Shift::Lsr => thumb::LSR,
                        Shift::Asr => thumb::ASR,
                        Shift::Ror => thumb::ROR,
                    },
                    vdst,
                    amount,
                ));
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
            DpOp::Mvn => self.e.put(thumb::alu(thumb::MVN, res, vr)),
            DpOp::Tst | DpOp::Cmp | DpOp::Cmn => {
                let ra = self.get_ro(&f, rn, tr);
                if imm_form {
                    self.e.put(thumb::cmp_imm(ra, c as u16));
                } else {
                    let code = match op {
                        DpOp::Tst => thumb::TST,
                        DpOp::Cmp => thumb::CMP,
                        _ => thumb::CMN,
                    };
                    self.e.put(thumb::alu(code, ra, vr));
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
                    V::Const(0) if op == DpOp::Rsb => self.e.put(thumb::alu(thumb::NEG, res, ra)),
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
                self.e.put(match op {
                    DpOp::And => thumb::alu(thumb::AND, res, b),
                    DpOp::Eor | DpOp::Teq => thumb::alu(thumb::EOR, res, b),
                    DpOp::Orr => thumb::alu(thumb::ORR, res, b),
                    DpOp::Bic => thumb::alu(thumb::BIC, res, b),
                    DpOp::Adc => thumb::alu(thumb::ADC, res, b),
                    DpOp::Sbc => thumb::alu(thumb::SBC, res, b),
                    _ => thumb::add_hi(res, b),
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
        let f = self.open(2, bit(rd), false, false);
        let (t, m) = (f.t(0), f.t(1));
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
        let f = self.open(1, 0, false, false);
        let t = f.t(0);
        match src {
            MsrSrc::Reg(rm) => self.get_copy(&f, rm, t),
            MsrSrc::Imm(v) => self.e.lit(t, v),
        }
        self.e.put_all(&thumb::msr(t));
        self.close(f);
    }

    // ---------------------------------------------------------------------------------------
    // multiply

    fn mul(&mut self, acc: bool, s: bool, rd: u8, rm: u8, rs: u8, rn: u8) {
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
        let clob = !self.can(FNZ);
        let f = self.open(
            2 + acc as usize,
            bit(rd) | bit(rm) | bit(rs) | bit(rn),
            clob,
            clob,
        );
        let (p, o) = (f.t(0), f.t(1));
        self.get_copy(&f, rm, p);
        let b = self.get_ro(&f, rs, o);
        self.e.put(thumb::alu(thumb::MUL, p, b));
        if acc {
            let a = self.get_ro(&f, rn, f.t(2));
            self.e.put(thumb::add_hi(p, a));
            if s {
                self.e.put(thumb::movs_reg(p, p));
            }
        }
        self.set(&f, rd, p);
        self.close(f);
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
    ) {
        if rd == PC || (load && rd == SP) {
            return self.unsupported();
        }
        if rn == PC {
            // a pc relative load of a constant
            let (true, true, false, Offset::Imm(imm)) = (load, pre, wb, off) else {
                return self.unsupported();
            };
            let Some(value) = self.pc_literal(kind, up, imm) else {
                return;
            };
            let value = Op2::Imm {
                value,
                rotated: false,
            };
            return self.dp(DpOp::Mov, false, rd, 0, value);
        }
        if rn == SP && self.sp_access(load, kind, pre, up, wb, rd, off) {
            return;
        }
        if let (Some(base), Offset::Imm(i)) = (self.kc[rn as usize], off)
            && rn != SP
            && self.mem_const(load, kind, pre, up, wb, rd, rn, base, i)
        {
            return;
        }

        let size_shift = match kind {
            MemKind::Word => 2,
            MemKind::Half | MemKind::SignedHalf => 1,
            MemKind::Byte | MemKind::SignedByte => 0,
        };
        let update = !pre || wb;
        // The offset is folded into the access when it fits, else it goes through a register
        // that is added to the address (pre) and to the base register (update).
        let mut imm5 = None;
        let mut need_to = false;
        // a post indexed immediate is added to the base register with one adds or subs
        let mut post_imm = None;
        // the flags the offset computation and the lookup change
        let translate = self.regions != 0 && rn != SP;
        let mut cl = if translate { FALL } else { 0 };
        match off {
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
        let clob = !self.can(cl);
        let rm_bit = match off {
            Offset::Reg { rm, .. } => bit(rm),
            _ => 0,
        };
        // A pre indexed offset is added before the lookup, so the region is the one the access
        // lands in (ldr rd, [r0, #-8] from the IO base is an IWRAM mirror)
        let add_base = need_to && pre;
        let ea = translate && add_base;
        // temps: the base, the offset (also the table pointer), a copy of rn or the address, the data
        let want_to = translate || need_to;
        let want_rr = translate && (ea || !is_low(rn));
        let avoid = bit(rn) | bit(rd) | rm_bit;
        let temps = 1 + want_to as usize + want_rr as usize;
        // a data register when rd is high, or when the frame may have to borrow rd
        let want_td = !is_low(rd) || temps + clob as usize > 8 - avoid.count_ones() as usize;
        let f = self.open(temps + want_td as usize, avoid, clob, clob);
        // add_to would use tn, which the load has overwritten with rd
        if (update && need_to && load && f.is_temp(rn) && f.t(0) == rd)
            || (post_imm.is_some() && f.is_temp(rn))
        {
            return self
                .e
                .fail(JitError::Internal("the pushed base is in the way"));
        }
        let tn = f.t(0);
        let to = f.t(1);
        let tr = f.t(1 + want_to as usize);
        let td = f.t(1 + want_to as usize + want_rr as usize);

        // the register the access goes through, and the register holding the signed offset
        let (base, to_reg) = if ea {
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
            td
        } else {
            self.get_ro(&f, rd, td)
        };
        match (load, kind) {
            (true, MemKind::Word) => self.e.put(thumb::ldr_imm(rt, base, i5)),
            (false, MemKind::Word) => self.e.put(thumb::str_imm(rt, base, i5)),
            (true, MemKind::Byte) => self.e.put(thumb::ldrb_imm(rt, base, i5)),
            (false, MemKind::Byte) => self.e.put(thumb::strb_imm(rt, base, i5)),
            (true, MemKind::Half) => self.e.put(thumb::ldrh_imm(rt, base, i5)),
            (false, MemKind::Half) => self.e.put(thumb::strh_imm(rt, base, i5)),
            (true, MemKind::SignedByte) => {
                self.e.put(thumb::ldrb_imm(rt, base, i5));
                self.e.put(thumb::sxtb(rt, rt));
            }
            (true, MemKind::SignedHalf) => {
                self.e.put(thumb::ldrh_imm(rt, base, i5));
                self.e.put(thumb::sxth(rt, rt));
            }
            _ => return self.unsupported(),
        }
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
        } else if update && need_to && rn != SP && !(load && rd == rn) {
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

    // The value of the word a pc relative load reads, as the load gives it. The pool is read
    // while translating, like the code is.
    fn pc_literal(&mut self, kind: MemKind, up: bool, imm: u32) -> Option<u32> {
        let pcv = self.pcv();
        let a = if up {
            pcv.wrapping_add(imm)
        } else {
            pcv.wrapping_sub(imm)
        };
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
                Insn::Mem {
                    load: true,
                    kind,
                    pre: true,
                    wb: false,
                    up,
                    rd,
                    rn: PC,
                    off: Offset::Imm(imm),
                } => {
                    let value = self.pc_literal(kind, up, imm);
                    new[0] = value.map(|v| (rd, v));
                }
                Insn::Mem {
                    load,
                    wb: true,
                    up,
                    rd,
                    rn,
                    off: Offset::Imm(i),
                    ..
                } if rn != PC && !(load && rd == rn) => {
                    new[1] = self.known(rn).map(|v| {
                        (
                            rn,
                            if up {
                                v.wrapping_add(i)
                            } else {
                                v.wrapping_sub(i)
                            },
                        )
                    });
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
        let page = (self.mem)((e & !1) + 4 * ((gba >> 9) & 0x1FF))?;
        // a page that is not there points at its own GBA address
        (page >> 24 != gba >> 24).then(|| page + (gba & 0x1FF))
    }

    // A load or store through a register with a known value. Returns false for the ones that
    // need the lookup.
    #[allow(clippy::too_many_arguments)]
    fn mem_const(
        &mut self,
        load: bool,
        kind: MemKind,
        pre: bool,
        up: bool,
        wb: bool,
        rd: u8,
        rn: u8,
        base: u32,
        imm: u32,
    ) -> bool {
        let moved = if up {
            base.wrapping_add(imm)
        } else {
            base.wrapping_sub(imm)
        };
        let ea = if pre { moved } else { base };
        let size = match kind {
            MemKind::Word => 4,
            MemKind::Half | MemKind::SignedHalf => 2,
            _ => 1,
        };
        // an unaligned word or halfword is not a plain access
        if ea & (size - 1) != 0 {
            return false;
        }
        let Some(native) = self.native(ea) else {
            return false;
        };
        let update = (!pre || wb) && !(load && rd == rn);
        let low_load = load && is_low(rd);
        // the address register (rd for a load), the data of a store from a high register, the
        // new base register value
        let want_ta = !low_load;
        let want_td = !load && !is_low(rd);
        let want_tb = update && !is_low(rn) && low_load;
        self.pref = 0;
        let f = self.open(
            want_ta as usize + want_td as usize + want_tb as usize,
            bit(rd) | bit(rn),
            false,
            false,
        );
        let mut k = 0;
        let mut next = || {
            k += 1;
            f.t(k - 1)
        };
        let ta = if want_ta { next() } else { rd };
        let td = if want_td { next() } else { 0 };
        let tb = if want_tb { next() } else { ta };
        self.e.lit(ta, native);
        let rt = if load { ta } else { self.get_ro(&f, rd, td) };
        match (load, kind) {
            (true, MemKind::Word) => self.e.put(thumb::ldr_imm(rt, ta, 0)),
            (false, MemKind::Word) => self.e.put(thumb::str_imm(rt, ta, 0)),
            (true, MemKind::Byte) => self.e.put(thumb::ldrb_imm(rt, ta, 0)),
            (false, MemKind::Byte) => self.e.put(thumb::strb_imm(rt, ta, 0)),
            (true, MemKind::Half) => self.e.put(thumb::ldrh_imm(rt, ta, 0)),
            (false, MemKind::Half) => self.e.put(thumb::strh_imm(rt, ta, 0)),
            (true, MemKind::SignedByte) => {
                self.e.put(thumb::ldrb_imm(rt, ta, 0));
                self.e.put(thumb::sxtb(rt, rt));
            }
            (true, MemKind::SignedHalf) => {
                self.e.put(thumb::ldrh_imm(rt, ta, 0));
                self.e.put(thumb::sxth(rt, rt));
            }
            _ => {
                self.unsupported();
            }
        }
        if load && !low_load {
            self.set(&f, rd, ta);
        }
        if update {
            if is_low(rn) {
                self.e.lit(rn, moved);
            } else {
                self.e.lit(tb, moved);
                self.e.put(thumb::mov(rn, tb));
            }
        }
        self.close(f);
        true
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
        self.e.bind_cond(flat);
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
                        thumb::alu(thumb::NEG, to, src)
                    });
                } else {
                    self.shift_imm(kind, to, src, amount);
                    if !up {
                        self.e.put(thumb::alu(thumb::NEG, to, to));
                    }
                }
                to
            }
        }
    }

    // The sp forms that need no translation of the address. Returns false for the rest.
    #[allow(clippy::too_many_arguments)]
    fn sp_access(
        &mut self,
        load: bool,
        kind: MemKind,
        pre: bool,
        up: bool,
        wb: bool,
        rd: u8,
        off: Offset,
    ) -> bool {
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
            let at = Offset::Imm(words * 4 - i);
            self.mem_access(load, kind, true, true, false, rd, SP, at);
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

    fn block_transfer(&mut self, load: bool, pre: bool, up: bool, wb: bool, rn: u8, list: u16) {
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
        let save = translate && !self.can(FALL);
        let used = bit(rn) | low_list;
        let free = 8 - used.count_ones() as usize;
        // What the frame holds: tn, to (the lookup scratch, also the offset and the register
        // that other registers go through), a copy of rn for the lookup when it is high or moved
        // by an offset, and the flags. ldmia/stmia need every scratch register outside the list.
        let plan = |add_imm: bool| {
            let want_to = translate || start != 0 || (update && !add_imm);
            let fixed = 1 + want_to as usize + save as usize;
            let rr = translate && (!is_low(rn) || start != 0);
            let fast = all_low && free >= fixed + rr as usize;
            let want_td = !fast && !want_to;
            // when the frame has to borrow registers rn can be one of them, the lookup then
            // reads it through a copy
            let demand = fixed + rr as usize + want_td as usize;
            let rr = rr || (translate && free < demand);
            (want_to, rr, want_td, fast, demand)
        };
        // the base register moves with an adds or subs when that may set the flags and no
        // register has to be borrowed for it
        let mut add_imm = update && is_low(rn) && (save || self.can(FALL));
        let mut p = plan(add_imm);
        if add_imm && free < p.4 {
            add_imm = false;
            p = plan(false);
        }
        let (want_to, want_rr, want_td, fast, _) = p;
        let f = self.open(
            1 + want_to as usize + want_rr as usize + want_td as usize,
            used,
            save,
            save,
        );
        let tn = f.t(0);
        let to = f.t(1);
        let tr = f.t(1 + want_to as usize);
        let td = if want_td {
            f.t(1 + want_to as usize + want_rr as usize)
        } else {
            to
        };

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
        let low = list as u8;
        let nl = low.count_ones() as u16;
        let nh = (list >> 8).count_ones() as u16;
        // without a low register r0 is borrowed
        let borrow = low == 0;
        let t = if borrow {
            0
        } else {
            low.trailing_zeros() as u8
        };
        self.e.put(thumb::sub_sp(nh));
        self.e.put(thumb::push(low | borrow as u8, false));
        let mut k = nl + borrow as u16;
        for r in 8..15u8 {
            if list & (1 << r) != 0 {
                self.e.put(thumb::mov(t, r));
                self.e.put(thumb::str_sp(t, k));
                k += 1;
            }
        }
        if borrow {
            self.e.put(thumb::pop(1 << t, false));
        } else {
            self.e.put(thumb::ldr_sp(t, 0));
        }
    }

    // ldmia sp!, {list} with registers pop cannot reach, the list has no pc
    fn pop_general(&mut self, list: u16) {
        let low = list as u8;
        let nl = low.count_ones() as u16;
        let nh = (list >> 8).count_ones() as u16;
        // without a low register r0 is borrowed
        let borrow = low == 0;
        let t = if borrow {
            0
        } else {
            low.trailing_zeros() as u8
        };
        if borrow {
            self.e.put(thumb::push(1 << t, false));
        }
        let mut k = nl + borrow as u16;
        for r in 8..15u8 {
            if list & (1 << r) != 0 {
                self.e.put(thumb::ldr_sp(t, k));
                self.e.put(thumb::mov(r, t));
                k += 1;
            }
        }
        // the low registers last, t gets its own value back
        self.e.put(thumb::pop(low | borrow as u8, false));
        self.e.put(thumb::add_sp(nh));
    }
}

fn writes_pc(insn: &Insn) -> bool {
    match *insn {
        Insn::Dp { op, rd, .. } => !op.is_test() && rd == PC,
        Insn::Mem { load, rd, .. } => load && rd == PC,
        Insn::Block { load, list, .. } => load && list & (1 << 15) != 0,
        _ => false,
    }
}

fn is_terminator(d: &Decoded) -> bool {
    matches!(
        d.insn,
        Insn::Branch { .. } | Insn::Bx { .. } | Insn::Unsupported
    ) || writes_pc(&d.insn)
}

// does the instruction change the flags, which ends a condition group
fn writes_flags(d: &Decoded) -> bool {
    match d.insn {
        Insn::Dp { s, .. } | Insn::Mul { s, .. } => s,
        Insn::Msr { spsr, flags, .. } => flags && !spsr,
        _ => false,
    }
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
        Insn::Mul { s: true, .. } => (FNZ, 0),
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
        Insn::Mul {
            acc, rm, rs, rn, ..
        } => bit(rm) | bit(rs) | if acc { bit(rn) } else { 0 },
        Insn::Mem {
            load, rd, rn, off, ..
        } => {
            let o = match off {
                Offset::Imm(_) => 0,
                Offset::Reg { rm, .. } => bit(rm),
            };
            o | bit(rn) | if load { 0 } else { bit(rd) }
        }
        Insn::Block { load, rn, list, .. } => bit(rn) | if load { 0 } else { list },
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
        Insn::Mul { rd, .. } | Insn::Mrs { rd, .. } => bit(rd),
        Insn::Mem {
            load, wb, rd, rn, ..
        } => (if load { bit(rd) } else { 0 }) | if wb { bit(rn) } else { 0 },
        Insn::Block {
            load, wb, rn, list, ..
        } => (if load { list } else { 0 }) | if wb { bit(rn) } else { 0 },
        Insn::Branch { link: true, .. } => bit(LR),
        _ => 0,
    }
}
