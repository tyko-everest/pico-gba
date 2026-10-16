// Output buffer for translated code, with the bookkeeping Thumb needs: forward branch fixups,
// a literal pool for pc relative loads, and the exit slots that link blocks together.
use super::{JitError, thumb};

const MAX_USES: usize = 48;
const MAX_POOL: usize = 48;

// the error for a conditional branch that does not reach, translate retries with long skips
pub const COND_RANGE: &str = "conditional branch out of range";

// An exit slot is six halfwords starting on a word boundary
pub const SLOT_HW: usize = 6;

#[derive(Clone, Copy)]
struct Use {
    at: usize,
    idx: usize,
}

pub struct Emit<'a> {
    code: &'a mut [u16],
    base: u32,
    // next halfword to write
    pub len: usize,
    overflow: bool,
    pub err: Option<JitError>,
    uses: [Use; MAX_USES],
    nuses: usize,
    pool: [u32; MAX_POOL],
    npool: usize,
    // pool entries whose value is set after the load is emitted, these are never shared
    late: u64,
    // the last position something was bound to or jumped over, code before it can not be
    // changed by what is emitted after it
    barrier: usize,
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
            uses: [Use { at: 0, idx: 0 }; MAX_USES],
            nuses: 0,
            pool: [0; MAX_POOL],
            npool: 0,
            late: 0,
            barrier: len,
        }
    }

    // is the current position a branch target, or was code just placed that must stay
    pub fn at_barrier(&self) -> bool {
        self.barrier == self.len
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

    pub fn pc(&self) -> u32 {
        self.addr_of(self.len)
    }

    pub fn put(&mut self, hw: u16) {
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
        let idx =
            match (0..self.npool).find(|&i| self.pool[i] == value && self.late & (1 << i) == 0) {
                Some(i) => i,
                None => self.new_pool_entry(value),
            };
        self.use_pool(rt, idx);
    }

    // a literal whose value is filled in later with set_pool
    pub fn lit_late(&mut self, rt: u8) -> usize {
        let idx = self.new_pool_entry(0);
        self.late |= 1 << idx;
        self.use_pool(rt, idx);
        idx
    }

    pub fn set_pool(&mut self, idx: usize, value: u32) {
        self.pool[idx] = value;
    }

    fn new_pool_entry(&mut self, value: u32) -> usize {
        if self.npool == MAX_POOL {
            self.fail(JitError::Internal("literal pool full"));
            return 0;
        }
        self.pool[self.npool] = value;
        self.npool += 1;
        self.npool - 1
    }

    fn use_pool(&mut self, rt: u8, idx: usize) {
        if self.nuses == MAX_USES {
            self.fail(JitError::Internal("too many literal uses"));
            return;
        }
        self.uses[self.nuses] = Use { at: self.len, idx };
        self.nuses += 1;
        self.put(thumb::ldr_pc(rt, 0));
    }

    // halfwords since the oldest literal use that is still waiting for a pool
    pub fn pool_pressure(&self) -> usize {
        if self.nuses == 0 {
            0
        } else {
            self.len - self.uses[0].at
        }
    }

    // How many more literal uses and pool entries fit before the pool must be flushed
    pub fn pool_room(&self) -> usize {
        (MAX_USES - self.nuses).min(MAX_POOL - self.npool)
    }

    // Place the pending literals here and point their loads at them. When code can fall into
    // this spot it is branched over.
    pub fn flush_pool(&mut self, branch_over: bool) {
        if self.nuses == 0 {
            return;
        }
        let over = self.len;
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
        self.barrier = self.len;
    }

    // Emit a placeholder conditional branch to patch with bind_cond
    pub fn fwd_cond(&mut self, cond: u8) -> usize {
        let at = self.len;
        self.put(thumb::b_cond(cond, 0));
        at
    }

    pub fn bind_cond(&mut self, at: usize) {
        self.barrier = self.len;
        let imm = self.len as i32 - (at as i32 + 2);
        if imm > 127 {
            self.fail(JitError::Internal(COND_RANGE));
            return;
        }
        if let Some(h) = self.code.get_mut(at) {
            *h = (*h & 0xFF00) | (imm as u16 & 0xFF);
        }
    }

    // Branch back to the halfword to, if it is in reach
    pub fn back_cond(&mut self, cond: u8, to: usize) -> bool {
        let imm = to as i32 - (self.len as i32 + 2);
        if imm < -128 {
            return false;
        }
        self.put(thumb::b_cond(cond, imm));
        true
    }

    pub fn back(&mut self, to: usize) -> bool {
        let imm = to as i32 - (self.len as i32 + 2);
        if imm < -1024 {
            return false;
        }
        self.put(thumb::b(imm));
        true
    }

    pub fn fwd(&mut self) -> usize {
        let at = self.len;
        self.put(thumb::b(0));
        at
    }

    pub fn bind(&mut self, at: usize) {
        self.barrier = self.len;
        let imm = self.len as i32 - (at as i32 + 2);
        if imm > 1023 {
            self.fail(JitError::Internal("branch out of range"));
            return;
        }
        if let Some(h) = self.code.get_mut(at) {
            *h = thumb::b(imm);
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
        self.barrier = self.len;
        at
    }
}
