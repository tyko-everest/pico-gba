// Output buffer for translated code, with the bookkeeping Thumb needs: forward branch fixups,
// a literal pool for pc relative loads, and the exit slots that link blocks together.
use super::{JitError, thumb};

// Every pool entry has a use, so this limits both
const MAX_LITERALS: usize = 48;

// The pool is flushed when the oldest literal waiting for it is this many halfwords back: an ldr
// reaches 255 words and the pool, up to MAX_LITERALS words, still has to fit in between
const POOL_REACH: usize = 240;

// ... or when there is this little room left for the literals of the next instruction
const POOL_RESERVE: usize = 8;

// An exit slot is six halfwords starting on a word boundary
pub const SLOT_HW: usize = 6;

// A frame whose registers are popped by the next code that is emitted, unless the next frame
// takes the same ones
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pop {
    mask: u8,
    // the register the flags are put back from first
    flags: Option<u8>,
}

#[derive(Clone, Copy)]
struct Use {
    at: usize,
    idx: usize,
}

pub struct Emit<'a> {
    code: &'a mut [u16],
    base: u32,
    // next halfword to write, not counting the pop that is still pending
    len: usize,
    overflow: bool,
    pub err: Option<JitError>,
    uses: [Use; MAX_LITERALS],
    nuses: usize,
    pool: [u32; MAX_LITERALS],
    npool: usize,
    // pool entries whose value is set after the load is emitted, these are never shared
    late: u64,
    pending: Option<Pop>,
}

impl<'a> Emit<'a> {
    // base is the address code[0] will run at
    pub fn new(code: &'a mut [u16], base: u32, len: usize) -> Self {
        Emit {
            code,
            base,
            len,
            overflow: false,
            err: None,
            uses: [Use { at: 0, idx: 0 }; MAX_LITERALS],
            nuses: 0,
            pool: [0; MAX_LITERALS],
            npool: 0,
            late: 0,
            pending: None,
        }
    }

    // The pop of a frame that was closed is not emitted at once: a frame that opens right after
    // with the same registers (and flags) takes it over, which saves the pop and the push.
    // Everything else settles it first.
    pub fn defer_pop(&mut self, mask: u8, flags: Option<u8>) {
        self.settle();
        self.pending = Some(Pop { mask, flags });
    }

    // take over the pending pop when it is the one of these registers
    pub fn reuse_pop(&mut self, mask: u8, flags: Option<u8>) -> bool {
        let same = self.pending == Some(Pop { mask, flags });
        if same {
            self.pending = None;
        }
        same
    }

    // emit the pending pop
    pub fn settle(&mut self) {
        if let Some(p) = self.pending.take() {
            if let Some(t) = p.flags {
                self.put_all(&thumb::msr(t));
            }
            self.put(thumb::pop(p.mask, false));
        }
    }

    // the next halfword to write
    pub fn pos(&mut self) -> usize {
        self.settle();
        self.len
    }

    pub fn fail(&mut self, err: JitError) {
        if self.err.is_none() {
            self.err = Some(err);
        }
    }

    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    pub fn addr_of(&self, hw: usize) -> u32 {
        self.base + 2 * hw as u32
    }

    pub fn pc(&mut self) -> u32 {
        let at = self.pos();
        self.addr_of(at)
    }

    pub fn put(&mut self, hw: u16) {
        self.settle();
        match self.code.get_mut(self.len) {
            Some(slot) => *slot = hw,
            None => self.overflow = true,
        }
        self.len += 1;
    }

    pub fn put_all(&mut self, hws: &[u16]) {
        for &h in hws {
            self.put(h);
        }
    }

    pub fn put_word(&mut self, word: u32) {
        self.put(word as u16);
        self.put((word >> 16) as u16);
    }

    // pad with a nop until the next halfword is on a word boundary
    pub fn align4(&mut self) {
        if self.pc() & 2 != 0 {
            self.put(thumb::NOP);
        }
    }

    // LDR rt, =value through the literal pool, does not touch the flags
    pub fn lit(&mut self, rt: u8, value: u32) {
        if self.literals_full() {
            return;
        }
        let idx =
            match (0..self.npool).find(|&i| self.pool[i] == value && self.late & (1 << i) == 0) {
                Some(i) => i,
                None => self.new_pool_entry(value),
            };
        self.use_pool(rt, idx);
    }

    // a literal whose value is filled in later with set_pool
    pub fn lit_late(&mut self, rt: u8) -> usize {
        if self.literals_full() {
            return 0;
        }
        let idx = self.new_pool_entry(0);
        self.late |= 1 << idx;
        self.use_pool(rt, idx);
        idx
    }

    pub fn set_pool(&mut self, idx: usize, value: u32) {
        self.pool[idx] = value;
    }

    fn literals_full(&mut self) -> bool {
        let full = self.nuses == MAX_LITERALS;
        if full {
            self.fail(JitError::Internal("too many literal uses"));
        }
        full
    }

    fn new_pool_entry(&mut self, value: u32) -> usize {
        self.pool[self.npool] = value;
        self.npool += 1;
        self.npool - 1
    }

    fn use_pool(&mut self, rt: u8, idx: usize) {
        let at = self.pos();
        self.uses[self.nuses] = Use { at, idx };
        self.nuses += 1;
        self.put(thumb::ldr_pc(rt, 0));
    }

    // is it time to flush the pool
    pub fn pool_due(&self) -> bool {
        if self.nuses == 0 {
            return false;
        }
        // the oldest literal use still waiting for a pool, counting the pending pop
        let pending = self
            .pending
            .map_or(0, |p| 1 + 2 * p.flags.is_some() as usize);
        self.len + pending - self.uses[0].at > POOL_REACH
            || MAX_LITERALS - self.nuses < POOL_RESERVE
    }

    // Place the pending literals here and point their loads at them. When code can fall into
    // this spot it is branched over.
    pub fn flush_pool(&mut self, branch_over: bool) {
        if self.nuses == 0 {
            return;
        }
        let over = self.pos();
        if branch_over {
            self.put(thumb::b(0));
        }
        self.align4();
        let pool_hw = self.len;
        for i in 0..self.npool {
            let v = self.pool[i];
            self.put_word(v);
        }
        for i in 0..self.nuses {
            let u = self.uses[i];
            let use_pc = (self.addr_of(u.at) + 4) & !3;
            let target = self.addr_of(pool_hw + 2 * u.idx);
            let words = (target - use_pc) / 4;
            if words > 255 {
                self.fail(JitError::Internal("literal out of range"));
                continue;
            }
            if let Some(h) = self.code.get_mut(u.at) {
                *h |= words as u16;
            }
        }
        if branch_over {
            let imm = self.len as i32 - (over as i32 + 2);
            if imm > 1023 {
                self.fail(JitError::Internal("pool branch out of range"));
            } else if let Some(h) = self.code.get_mut(over) {
                *h = thumb::b(imm);
            }
        }
        self.nuses = 0;
        self.npool = 0;
        self.late = 0;
    }

    // Emit a placeholder conditional branch to patch with bind
    pub fn fwd_cond(&mut self, cond: u8) -> usize {
        let at = self.pos();
        self.put(thumb::b_cond(cond, 0));
        at
    }

    // Branch back to the halfword to, the caller has checked that it is in reach
    pub fn back_cond(&mut self, cond: u8, to: usize) {
        let imm = to as i32 - (self.pos() as i32 + 2);
        self.put(thumb::b_cond(cond, imm));
    }

    pub fn back(&mut self, to: usize) {
        let imm = to as i32 - (self.pos() as i32 + 2);
        self.put(thumb::b(imm));
    }

    pub fn fwd(&mut self) -> usize {
        let at = self.pos();
        self.put(thumb::b(0));
        at
    }

    // Point the branch from fwd or fwd_cond at the current position
    pub fn bind(&mut self, at: usize) {
        let imm = self.pos() as i32 - (at as i32 + 2);
        let Some(&h) = self.code.get(at) else {
            return;
        };
        if h >> 12 == 0xD {
            if imm > 127 {
                self.fail(JitError::CondRange);
            } else {
                self.code[at] = (h & 0xFF00) | (imm as u16 & 0xFF);
            }
        } else if imm > 1023 {
            self.fail(JitError::Internal("branch out of range"));
        } else {
            self.code[at] = thumb::b(imm);
        }
    }

    // Emit an unresolved exit to the ARM address target and return the halfword it starts at.
    // The slot is a trap followed by the target, the fault handler later rewrites it into a jump
    // to the translated block.
    pub fn slot(&mut self, target: u32) -> usize {
        self.align4();
        let at = self.len;
        self.put(thumb::TRAP);
        self.put(thumb::NOP);
        self.put_word(target);
        self.put(thumb::NOP);
        self.put(thumb::NOP);
        at
    }
}
