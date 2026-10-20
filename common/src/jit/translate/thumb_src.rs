// Translating Thumb code for the Cortex-M0+: it is the same instruction set, so nearly every
// instruction is copied as it is. What cannot stay is whatever depends on where the code is: pc
// relative loads and additions (the value is worked out while translating), branches (they go
// through exit slots, like in translated ARM code) and moves that read or write the pc.
//
// The point of translating code that could run in place is that it runs from the code cache in
// RAM, whatever the ROM it came from is paged in and out of. Loads and stores are copied too and
// fault the same way they do when running in place, the fault handler does the access.
//
// Addresses are the GBA address of the Thumb code, the exit slots and the block map use that
// address with bit 0 set to tell it from ARM code.
use super::*;
use crate::jit::{DISPATCH_BITS, DISPATCH_ENTRIES, STUB_TRAP};

// Longest run of Thumb instructions in a block
const MAX_THUMB_BLOCK: usize = 64;

// Emit the indirect jump dispatcher at the start of the code cache and return the halfword of the
// UDF that a miss ends up at. It is entered with the GBA address to jump to on top of the stack,
// looks it up in the table and pops the native address into the pc, every register is as it was
// (the flags are not). A target that is not in the table is stored in the miss word and the
// jump goes to the stub, the fault handler translates it, adds it to the table and continues.
pub fn emit_dispatcher(e: &mut Emit, table: u32) -> usize {
    let miss_var = table + DISPATCH_ENTRIES as u32 * 8;
    // the target ends up at [sp, #12], above the three registers. An interrupt that dispatches
    // as well could change the entry between its two words and the miss word, so there are none
    // until the target is on its way.
    e.put(thumb::push(0b111, false));
    e.put(thumb::CPSID);
    e.put(thumb::ldr_sp(0, 3));
    // r1 = byte offset of the entry: (target >> 1) & (entries - 1), times 8
    e.put(thumb::lsls_imm(1, 0, 31 - DISPATCH_BITS as u16));
    e.put(thumb::lsrs_imm(1, 1, 32 - DISPATCH_BITS as u16));
    e.put(thumb::lsls_imm(1, 1, 3));
    e.lit(2, table);
    e.put(thumb::adds_reg(1, 1, 2));
    e.put(thumb::ldr_imm(2, 1, 0));
    e.put(thumb::alu(thumb::CMP, 2, 0));
    let miss = e.fwd_cond(1);
    e.put(thumb::ldr_imm(2, 1, 1));
    let done = e.fwd();
    e.bind(miss);
    e.lit(1, miss_var);
    e.put(thumb::str_imm(0, 1, 0));
    let stub_lit = e.lit_late(2);
    e.bind(done);
    e.put(thumb::str_sp(2, 3));
    e.put(thumb::pop(0b111, false));
    e.put(thumb::CPSIE);
    e.put(thumb::pop(0, true));
    let stub = e.pos();
    e.put(STUB_TRAP);
    e.put(thumb::NOP);
    let stub_addr = e.addr_of(stub) | 1;
    e.set_pool(stub_lit, stub_addr);
    e.flush_pool(false);
    stub
}

impl<'a, 'm> Tr<'a, 'm> {
    // Jump to the GBA address on top of the stack through the dispatcher. The stack is as it was
    // without that word when the code it ends up in runs.
    fn dispatch_jump(&mut self) {
        if self.dispatch == 0 {
            self.e
                .fail(JitError::Internal("no dispatcher for Thumb code"));
            return;
        }
        self.e.put(thumb::sub_sp(1));
        self.e.put(thumb::push(1, false));
        self.e.lit(0, self.dispatch);
        self.e.put(thumb::str_sp(0, 1));
        self.e.put(thumb::pop(1, false));
        self.e.put(thumb::pop(0, true));
    }

    // push any register but the pc, r13 (sp) excepted, without losing another one
    fn push_register(&mut self, r: u8) {
        if r == LR {
            self.e.put(thumb::push(0, true));
        } else if is_low(r) {
            self.e.put(thumb::push(1 << r, false));
        } else {
            self.e.put(thumb::sub_sp(1));
            self.e.put(thumb::push(1, false));
            self.e.put(thumb::mov(0, r));
            self.e.put(thumb::str_sp(0, 1));
            self.e.put(thumb::pop(1, false));
        }
    }

    fn read_half(&mut self, at: u32) -> Option<u16> {
        let word = (self.mem)(at & !3)?;
        Some((word >> (8 * (at & 2))) as u16)
    }

    // the register a temporary can use next to the ones the instruction names
    fn spare(&self, avoid: [u8; 2]) -> u8 {
        (0..8u8).find(|r| !avoid.contains(r)).unwrap()
    }

    // Translate the Thumb block at the GBA address start (bit 0 clear), returns the address after
    // its last instruction.
    pub fn thumb_block(&mut self, start: u32) -> u32 {
        self.start = start | 1;
        self.start_hw = self.e.pos();
        let mut a = start;
        let mut n = 0;
        loop {
            if n == MAX_THUMB_BLOCK {
                self.e.slot(a | 1);
                self.thumb_finish();
                return a;
            }
            n += 1;
            let Some(h) = self.read_half(a) else {
                self.e.fail(JitError::BadRead(a));
                return a;
            };
            let pcv = a.wrapping_add(4);
            let rest = h >> 11;
            let mut ends = false;
            let mut size = 2;
            match h >> 12 {
                0x4 if rest == 0b01001 => {
                    // ldr rd, [pc, #imm]
                    let rd = ((h >> 8) & 7) as u8;
                    let at = (pcv & !3) + (h as u32 & 0xFF) * 4;
                    match (self.mem)(at) {
                        Some(value) => self.e.lit(rd, value),
                        None => {
                            self.e.fail(JitError::BadRead(at));
                            return a;
                        }
                    }
                }
                0x4 if h & 0xFC00 == 0x4400 => {
                    ends = self.thumb_hi_reg(h, a);
                }
                0xA if h & 0x0800 == 0 => {
                    // add rd, pc, #imm
                    let rd = ((h >> 8) & 7) as u8;
                    let value = (pcv & !3) + (h as u32 & 0xFF) * 4;
                    self.e.lit(rd, value);
                }
                0xB => {
                    if h & 0xFF00 == 0xB000 || h & 0x0600 == 0x0400 {
                        // add sp, push, pop
                        if h & 0xFE00 == 0xBC00 && h & 0x100 != 0 {
                            // pop {..., pc}: the pc is the word the dispatcher gets
                            if h & 0xFF != 0 {
                                self.e.put(h & !0x100);
                            }
                            self.dispatch_jump();
                            ends = true;
                        } else {
                            self.e.put(h);
                        }
                    } else {
                        return self.thumb_unsupported(a, h);
                    }
                }
                0xD => {
                    let cond = ((h >> 8) & 0xF) as u8;
                    if cond == 0xF {
                        self.e.put(h);
                    } else if cond == 0xE {
                        return self.thumb_unsupported(a, h);
                    } else {
                        let target = pcv.wrapping_add((h as u8 as i8 as i32 * 2) as u32) | 1;
                        self.thumb_branch(cond, target, a.wrapping_add(2) | 1);
                        ends = true;
                    }
                }
                0xE if rest == 0b11100 => {
                    let off = ((h as i32) << 21 >> 20) as u32;
                    self.thumb_branch(COND_AL, pcv.wrapping_add(off) | 1, 0);
                    ends = true;
                }
                0xF => {
                    // bl: the prefix holds the high half of the offset
                    let Some(h2) = self.read_half(a.wrapping_add(2)) else {
                        self.e.fail(JitError::BadRead(a.wrapping_add(2)));
                        return a;
                    };
                    if rest != 0b11110 || h2 >> 11 != 0b11111 {
                        return self.thumb_unsupported(a, h);
                    }
                    let off = ((h as i32) << 21 >> 9) as u32 | (h2 as u32 & 0x7FF) << 1;
                    self.thumb_call(pcv.wrapping_add(off) | 1, a.wrapping_add(4) | 1);
                    size = 4;
                    ends = true;
                }
                _ => {
                    // formats that do not depend on the pc: shifts, arithmetic, loads and stores
                    // (apart from the pc relative load above), ldm and stm, svc
                    self.e.put(h);
                }
            }
            if self.e.err.is_some() {
                return a;
            }
            a = a.wrapping_add(size);
            if ends {
                self.thumb_finish();
                return a;
            }
            if self.e.pool_due() {
                self.e.flush_pool(true);
            }
        }
    }

    // The end of a block: the literals
    fn thumb_finish(&mut self) {
        self.e.settle();
        self.e.flush_pool(false);
    }

    fn thumb_unsupported(&mut self, addr: u32, word: u16) -> u32 {
        self.e.fail(JitError::Unsupported {
            addr,
            word: word as u32,
        });
        addr
    }

    // Format 5, the add, compare and move between any registers and bx. Returns whether the
    // block ends here.
    fn thumb_hi_reg(&mut self, h: u16, a: u32) -> bool {
        let op = (h >> 8) & 3;
        let rd = ((h & 7) | (h >> 4) & 8) as u8;
        let rs = ((h >> 3) & 0xF) as u8;
        let pcv = a.wrapping_add(4);
        if op == 3 {
            if rs == PC {
                // bx pc: an ARM code follows the next instruction, a veneer
                self.e.slot(pcv.wrapping_add(2) & !3);
            } else {
                self.push_register(rs);
                self.dispatch_jump();
            }
            return true;
        }
        if rd == PC {
            if rs == PC {
                self.e.fail(JitError::Unsupported {
                    addr: a,
                    word: h as u32,
                });
                return true;
            }
            self.thumb_jump_reg(op, rs, pcv);
            return true;
        }
        if rs != PC {
            self.e.put(h);
            return false;
        }
        // reads the pc: the value is a constant
        if op == 2 && is_low(rd) {
            self.e.lit(rd, pcv);
            return false;
        }
        let t = self.spare([rd, rd]);
        self.e.put(thumb::push(1 << t, false));
        self.e.lit(t, pcv);
        self.e.put(match op {
            0 => thumb::add_hi(rd, t),
            1 => 0x4500 | ((rd as u16 >> 3) << 7) | (t as u16) << 3 | (rd as u16 & 7),
            _ => thumb::mov(rd, t),
        });
        self.e.put(thumb::pop(1 << t, false));
        false
    }

    // mov pc, rs and add pc, rs: a jump to the register, the target stays Thumb
    fn thumb_jump_reg(&mut self, op: u16, rs: u8, pcv: u32) {
        if op == 1 || rs == SP {
            self.e.fail(JitError::Unsupported {
                addr: self.start & !1,
                word: 0x4487,
            });
            return;
        }
        // the target goes in a slot reserved on the stack, with bit 0 set to stay in Thumb
        // (N and Z are not kept)
        self.e.put(thumb::sub_sp(1));
        self.e.put(thumb::push(0b11, false));
        self.e.put(thumb::mov(0, rs));
        if op == 0 {
            self.e.lit(1, pcv);
            self.e.put(thumb::add_hi(0, 1));
        }
        self.e.put(thumb::movs_imm(1, 1));
        self.e.put(thumb::alu(thumb::ORR, 0, 1));
        self.e.put(thumb::str_sp(0, 2));
        self.e.put(thumb::pop(0b11, false));
        self.dispatch_jump();
    }

    // b and b<cond>. next is the address of the instruction after (with bit 0) for a condition.
    fn thumb_branch(&mut self, cond: u8, target: u32, next: u32) {
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
    }

    // bl: lr is the GBA address of the instruction after it, like on the device, and the code
    // that returns to it goes through the dispatcher
    fn thumb_call(&mut self, target: u32, next: u32) {
        self.e.put(thumb::push(1, false));
        self.e.lit(0, next);
        self.e.put(thumb::mov(LR, 0));
        self.e.put(thumb::pop(1, false));
        self.e.slot(target);
    }
}
