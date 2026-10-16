// ARMv6-M (Cortex-M0+) Thumb interpreter. Everything that needs the outside world (memory, the
// JIT, BIOS calls) goes through the Bus and the events step returns.

// Cycle costs, Cortex-M0+ with the single cycle multiplier. Everything the model charges for an
// instruction is in this table. pop {.., pc} is 3+N (the TRM says 4+N), bl is 3 and mrs/msr are 3,
// as in the spec this was written from.
pub mod cost {
    pub const ALU: u32 = 1;
    pub const MUL: u32 = 1;
    // ldr/str of any size, also the sp and pc relative forms
    pub const MEM: u32 = 2;
    // ldm/stm/push/pop are BASE + registers
    pub const MULTI_BASE: u32 = 1;
    pub const POP_PC_EXTRA: u32 = 2;
    pub const BRANCH_TAKEN: u32 = 2;
    pub const BRANCH_NOT_TAKEN: u32 = 1;
    // bx, blx, mov pc, add pc
    pub const BX: u32 = 2;
    pub const BL: u32 = 3;
    pub const SYSREG: u32 = 3;
    // dmb, dsb, isb
    pub const BARRIER: u32 = 3;
    // nop and the other hints, cps
    pub const HINT: u32 = 1;
}

// What kind of access it is decides whether it can fault on the device
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    // ldr/str/ldm/stm through a general register, faults when the address is a GBA one
    Data,
    // the second and later words of an ldm/stm, which the fault handler does in one go
    Rest,
    // pc relative load, the pc is a native address on the device
    Literal,
    // push/pop and sp relative accesses, must be native (the fault handler cannot do them)
    Stack,
}

pub trait Bus {
    fn fetch(&mut self, addr: u32) -> u16;
    fn read(&mut self, addr: u32, size: u32, kind: Access) -> u32;
    fn write(&mut self, addr: u32, size: u32, value: u32, kind: Access);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Svc(u8),
    // a bx/blx/pop to an even address, or the bx pc veneer: jump into ARM code at this address
    ArmJump(u32),
    // udf, r15 is already past it
    Udf(u16),
    Bkpt,
    Undefined(u16),
}

pub struct Step {
    pub cycles: u32,
    pub event: Option<Event>,
}

#[derive(Clone)]
pub struct Cpu {
    // r[15] is the address of the next instruction
    pub r: [u32; 16],
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
}

fn lsl(x: u32, amt: u32, c: bool) -> (u32, bool) {
    match amt {
        0 => (x, c),
        1..=31 => (x << amt, (x >> (32 - amt)) & 1 != 0),
        32 => (0, x & 1 != 0),
        _ => (0, false),
    }
}

fn lsr(x: u32, amt: u32, c: bool) -> (u32, bool) {
    match amt {
        0 => (x, c),
        1..=31 => (x >> amt, (x >> (amt - 1)) & 1 != 0),
        32 => (0, x >> 31 != 0),
        _ => (0, false),
    }
}

fn asr(x: u32, amt: u32, c: bool) -> (u32, bool) {
    match amt {
        0 => (x, c),
        1..=31 => (((x as i32) >> amt) as u32, (x >> (amt - 1)) & 1 != 0),
        _ => (((x as i32) >> 31) as u32, x >> 31 != 0),
    }
}

fn ror(x: u32, amt: u32, c: bool) -> (u32, bool) {
    if amt == 0 {
        return (x, c);
    }
    let r = x.rotate_right(amt & 31);
    (r, r >> 31 != 0)
}

// (result, carry, overflow)
fn add_with_carry(x: u32, y: u32, carry_in: bool) -> (u32, bool, bool) {
    let unsigned = x as u64 + y as u64 + carry_in as u64;
    let signed = x as i32 as i64 + y as i32 as i64 + carry_in as i64;
    let result = unsigned as u32;
    (
        result,
        unsigned != result as u64,
        signed != result as i32 as i64,
    )
}

impl Cpu {
    pub fn new() -> Self {
        Cpu {
            r: [0; 16],
            n: false,
            z: false,
            c: false,
            v: false,
        }
    }

    pub fn flags(&self) -> u32 {
        (self.n as u32) << 31
            | (self.z as u32) << 30
            | (self.c as u32) << 29
            | (self.v as u32) << 28
    }

    pub fn set_flags(&mut self, f: u32) {
        self.n = f >> 31 & 1 != 0;
        self.z = f >> 30 & 1 != 0;
        self.c = f >> 29 & 1 != 0;
        self.v = f >> 28 & 1 != 0;
    }

    // a register as an instruction reads it, the pc reads as the instruction's address + 4
    fn rd(&self, i: u32) -> u32 {
        if i == 15 {
            self.r[15].wrapping_add(2)
        } else {
            self.r[i as usize]
        }
    }

    fn nz(&mut self, v: u32) {
        self.n = v >> 31 != 0;
        self.z = v == 0;
    }

    // adds/subs/cmp/cmn/adcs/sbcs: all four flags
    fn add_flags(&mut self, x: u32, y: u32, carry_in: bool) -> u32 {
        let (r, c, v) = add_with_carry(x, y, carry_in);
        self.nz(r);
        self.c = c;
        self.v = v;
        r
    }

    fn cond(&self, cond: u32) -> bool {
        match cond {
            0 => self.z,
            1 => !self.z,
            2 => self.c,
            3 => !self.c,
            4 => self.n,
            5 => !self.n,
            6 => self.v,
            7 => !self.v,
            8 => self.c && !self.z,
            9 => !self.c || self.z,
            10 => self.n == self.v,
            11 => self.n != self.v,
            12 => !self.z && self.n == self.v,
            _ => self.z || self.n != self.v,
        }
    }

    // Jump after a bx/blx/pop pc: bit 0 set stays in Thumb, otherwise it is ARM code for the JIT
    fn exchange(&mut self, target: u32) -> Option<Event> {
        if target & 1 != 0 {
            self.r[15] = target & !1;
            None
        } else {
            Some(Event::ArmJump(target))
        }
    }

    fn load<B: Bus>(
        &mut self,
        bus: &mut B,
        rt: u32,
        addr: u32,
        size: u32,
        signed: bool,
        kind: Access,
    ) {
        let mut v = bus.read(addr, size, kind);
        if signed {
            v = match size {
                1 => v as u8 as i8 as i32 as u32,
                _ => v as u16 as i16 as i32 as u32,
            };
        }
        self.r[rt as usize] = v;
    }

    pub fn step<B: Bus>(&mut self, bus: &mut B) -> Step {
        let at = self.r[15];
        let hw = bus.fetch(at) as u32;
        self.r[15] = at.wrapping_add(2);
        let mut event = None;
        let mut cycles = cost::ALU;
        let rd3 = hw & 7;
        let rn3 = (hw >> 3) & 7;
        match hw >> 11 {
            // lsls/lsrs/asrs rd, rm, #imm5
            0..=2 => {
                let imm = (hw >> 6) & 31;
                let x = self.r[rn3 as usize];
                let (r, c) = match hw >> 11 {
                    0 => lsl(x, imm, self.c),
                    1 => lsr(x, if imm == 0 { 32 } else { imm }, self.c),
                    _ => asr(x, if imm == 0 { 32 } else { imm }, self.c),
                };
                self.r[rd3 as usize] = r;
                self.nz(r);
                self.c = c;
            }
            3 => {
                let x = self.r[rn3 as usize];
                let field = (hw >> 6) & 7;
                let r = match (hw >> 9) & 3 {
                    0 => self.add_flags(x, self.r[field as usize], false),
                    1 => self.add_flags(x, !self.r[field as usize], true),
                    2 => self.add_flags(x, field, false),
                    _ => self.add_flags(x, !field, true),
                };
                self.r[rd3 as usize] = r;
            }
            // movs/cmp/adds/subs rd, #imm8
            4..=7 => {
                let rd = ((hw >> 8) & 7) as usize;
                let imm = hw & 0xFF;
                match (hw >> 11) & 3 {
                    0 => {
                        self.r[rd] = imm;
                        self.nz(imm);
                    }
                    1 => {
                        self.add_flags(self.r[rd], !imm, true);
                    }
                    2 => self.r[rd] = self.add_flags(self.r[rd], imm, false),
                    _ => self.r[rd] = self.add_flags(self.r[rd], !imm, true),
                }
            }
            8 if hw & 0x400 == 0 => {
                self.alu(hw);
                if (hw >> 6) & 15 == 13 {
                    cycles = cost::MUL;
                }
            }
            8 => {
                let rm = (hw >> 3) & 15;
                let rd = (hw & 7) | (hw >> 4) & 8;
                match (hw >> 8) & 3 {
                    0 => {
                        let v = self.rd(rd).wrapping_add(self.rd(rm));
                        if rd == 15 {
                            self.r[15] = v & !1;
                            cycles = cost::BX;
                        } else {
                            self.r[rd as usize] = v;
                        }
                    }
                    1 => {
                        self.add_flags(self.rd(rd), !self.rd(rm), true);
                    }
                    2 => {
                        let v = self.rd(rm);
                        if rd == 15 {
                            self.r[15] = v & !1;
                            cycles = cost::BX;
                        } else {
                            self.r[rd as usize] = v;
                        }
                    }
                    _ => {
                        cycles = cost::BX;
                        if rm == 15 {
                            // bx pc, the Thumb to ARM veneer. The ARM code follows the next
                            // instruction, as jit_fault expects.
                            event = Some(Event::ArmJump(at.wrapping_add(4) & !3));
                        } else {
                            let target = self.r[rm as usize];
                            if hw & 0x80 != 0 {
                                self.r[14] = at.wrapping_add(2) | 1;
                            }
                            event = self.exchange(target);
                        }
                    }
                }
            }
            // ldr rt, [pc, #imm8 * 4]
            9 => {
                let addr = (at.wrapping_add(4) & !3).wrapping_add((hw & 0xFF) * 4);
                self.load(bus, (hw >> 8) & 7, addr, 4, false, Access::Literal);
                cycles = cost::MEM;
            }
            // load/store with a register offset
            10 | 11 => {
                let addr = self.r[rn3 as usize].wrapping_add(self.r[((hw >> 6) & 7) as usize]);
                let rt = rd3;
                match (hw >> 9) & 7 {
                    0 => bus.write(addr, 4, self.r[rt as usize], Access::Data),
                    1 => bus.write(addr, 2, self.r[rt as usize] & 0xFFFF, Access::Data),
                    2 => bus.write(addr, 1, self.r[rt as usize] & 0xFF, Access::Data),
                    3 => self.load(bus, rt, addr, 1, true, Access::Data),
                    4 => self.load(bus, rt, addr, 4, false, Access::Data),
                    5 => self.load(bus, rt, addr, 2, false, Access::Data),
                    6 => self.load(bus, rt, addr, 1, false, Access::Data),
                    _ => self.load(bus, rt, addr, 2, true, Access::Data),
                }
                cycles = cost::MEM;
            }
            // load/store with an immediate offset
            12..=17 => {
                let imm = (hw >> 6) & 31;
                let base = self.r[rn3 as usize];
                let (size, load) = match hw >> 11 {
                    12 => (4, false),
                    13 => (4, true),
                    14 => (1, false),
                    15 => (1, true),
                    16 => (2, false),
                    _ => (2, true),
                };
                let addr = base.wrapping_add(imm * size);
                if load {
                    self.load(bus, rd3, addr, size, false, Access::Data);
                } else {
                    let mask = if size == 4 {
                        u32::MAX
                    } else {
                        (1 << (8 * size)) - 1
                    };
                    bus.write(addr, size, self.r[rd3 as usize] & mask, Access::Data);
                }
                cycles = cost::MEM;
            }
            // str/ldr rt, [sp, #imm8 * 4]
            18 | 19 => {
                let addr = self.r[13].wrapping_add((hw & 0xFF) * 4);
                let rt = (hw >> 8) & 7;
                if hw >> 11 == 18 {
                    bus.write(addr, 4, self.r[rt as usize], Access::Stack);
                } else {
                    self.load(bus, rt, addr, 4, false, Access::Stack);
                }
                cycles = cost::MEM;
            }
            // adr rd, label
            20 => {
                self.r[((hw >> 8) & 7) as usize] =
                    (at.wrapping_add(4) & !3).wrapping_add((hw & 0xFF) * 4)
            }
            // add rd, sp, #imm8 * 4
            21 => self.r[((hw >> 8) & 7) as usize] = self.r[13].wrapping_add((hw & 0xFF) * 4),
            22 | 23 => return self.misc(bus, hw),
            // stmia/ldmia rn!, {list}
            24 | 25 => {
                let rn = ((hw >> 8) & 7) as usize;
                let list = hw & 0xFF;
                if list == 0 {
                    return Step {
                        cycles,
                        event: Some(Event::Undefined(hw as u16)),
                    };
                }
                let base = self.r[rn];
                let load = hw >> 11 == 25;
                let mut addr = base;
                let mut first = true;
                for reg in 0..8 {
                    if list & (1 << reg) == 0 {
                        continue;
                    }
                    let kind = if first { Access::Data } else { Access::Rest };
                    first = false;
                    if load {
                        self.r[reg] = bus.read(addr, 4, kind);
                    } else {
                        bus.write(addr, 4, self.r[reg], kind);
                    }
                    addr = addr.wrapping_add(4);
                }
                if !(load && list & (1 << rn) != 0) {
                    self.r[rn] = addr;
                }
                cycles = cost::MULTI_BASE + list.count_ones();
            }
            26 | 27 => {
                let cond = (hw >> 8) & 15;
                match cond {
                    14 => {
                        return Step {
                            cycles,
                            event: Some(Event::Udf(hw as u16)),
                        };
                    }
                    15 => {
                        return Step {
                            cycles,
                            event: Some(Event::Svc(hw as u8)),
                        };
                    }
                    _ => {
                        if self.cond(cond) {
                            let off = (hw as u8 as i8 as i32) << 1;
                            self.r[15] = at.wrapping_add(4).wrapping_add(off as u32);
                            cycles = cost::BRANCH_TAKEN;
                        } else {
                            cycles = cost::BRANCH_NOT_TAKEN;
                        }
                    }
                }
            }
            28 => {
                let off = (((hw & 0x7FF) << 21) as i32 >> 20) as u32;
                self.r[15] = at.wrapping_add(4).wrapping_add(off);
                cycles = cost::BRANCH_TAKEN;
            }
            30 | 31 => return self.wide(bus, hw, at),
            _ => event = Some(Event::Undefined(hw as u16)),
        }
        Step { cycles, event }
    }

    // 010000: data processing on low registers
    fn alu(&mut self, hw: u32) {
        let rdn = (hw & 7) as usize;
        let rm = ((hw >> 3) & 7) as usize;
        let x = self.r[rdn];
        let y = self.r[rm];
        match (hw >> 6) & 15 {
            0 => {
                self.r[rdn] = x & y;
                self.nz(x & y);
            }
            1 => {
                self.r[rdn] = x ^ y;
                self.nz(x ^ y);
            }
            op @ 2..=4 | op @ 7 => {
                let amt = y & 0xFF;
                let (r, c) = match op {
                    2 => lsl(x, amt, self.c),
                    3 => lsr(x, amt, self.c),
                    4 => asr(x, amt, self.c),
                    _ => ror(x, amt, self.c),
                };
                self.r[rdn] = r;
                self.nz(r);
                self.c = c;
            }
            5 => self.r[rdn] = self.add_flags(x, y, self.c),
            6 => self.r[rdn] = self.add_flags(x, !y, self.c),
            8 => self.nz(x & y),
            // rsbs rd, rm, #0
            9 => self.r[rdn] = self.add_flags(!y, 0, true),
            10 => {
                self.add_flags(x, !y, true);
            }
            11 => {
                self.add_flags(x, y, false);
            }
            12 => {
                self.r[rdn] = x | y;
                self.nz(x | y);
            }
            // muls only sets N and Z
            13 => {
                let r = x.wrapping_mul(y);
                self.r[rdn] = r;
                self.nz(r);
            }
            14 => {
                self.r[rdn] = x & !y;
                self.nz(x & !y);
            }
            _ => {
                self.r[rdn] = !y;
                self.nz(!y);
            }
        }
    }

    // 1011: sp adjust, extend, push/pop, reverse, hints
    fn misc<B: Bus>(&mut self, bus: &mut B, hw: u32) -> Step {
        let rd3 = (hw & 7) as usize;
        let rm3 = ((hw >> 3) & 7) as usize;
        let mut cycles = cost::ALU;
        let mut event = None;
        match (hw >> 8) & 15 {
            0 => {
                let off = (hw & 0x7F) * 4;
                self.r[13] = if hw & 0x80 != 0 {
                    self.r[13].wrapping_sub(off)
                } else {
                    self.r[13].wrapping_add(off)
                };
            }
            2 => {
                let x = self.r[rm3];
                self.r[rd3] = match (hw >> 6) & 3 {
                    0 => x as u16 as i16 as i32 as u32,
                    1 => x as u8 as i8 as i32 as u32,
                    2 => x & 0xFFFF,
                    _ => x & 0xFF,
                };
            }
            4 | 5 => {
                let list = hw & 0xFF;
                let count = list.count_ones() + (hw >> 8 & 1);
                let mut addr = self.r[13].wrapping_sub(count * 4);
                self.r[13] = addr;
                for reg in 0..8 {
                    if list & (1 << reg) != 0 {
                        bus.write(addr, 4, self.r[reg], Access::Stack);
                        addr = addr.wrapping_add(4);
                    }
                }
                if hw & 0x100 != 0 {
                    bus.write(addr, 4, self.r[14], Access::Stack);
                }
                cycles = cost::MULTI_BASE + count;
            }
            12 | 13 => {
                let list = hw & 0xFF;
                let count = list.count_ones() + (hw >> 8 & 1);
                let mut addr = self.r[13];
                for reg in 0..8 {
                    if list & (1 << reg) != 0 {
                        self.r[reg] = bus.read(addr, 4, Access::Stack);
                        addr = addr.wrapping_add(4);
                    }
                }
                let mut target = None;
                if hw & 0x100 != 0 {
                    target = Some(bus.read(addr, 4, Access::Stack));
                }
                self.r[13] = self.r[13].wrapping_add(count * 4);
                cycles = cost::MULTI_BASE + count;
                if let Some(t) = target {
                    cycles += cost::POP_PC_EXTRA;
                    event = self.exchange(t);
                }
            }
            // cpsie/cpsid
            6 if hw & 0xFFEF == 0xB662 => cycles = cost::HINT,
            10 => {
                let x = self.r[rm3];
                self.r[rd3] = match (hw >> 6) & 3 {
                    0 => x.swap_bytes(),
                    1 => (x & 0x00FF00FF) << 8 | (x >> 8) & 0x00FF00FF,
                    3 => (x as u16).swap_bytes() as i16 as i32 as u32,
                    _ => {
                        return Step {
                            cycles,
                            event: Some(Event::Undefined(hw as u16)),
                        };
                    }
                };
            }
            14 => event = Some(Event::Bkpt),
            // nop, yield, wfe, wfi, sev (anything with a mask is an IT, which v6-M lacks)
            15 if hw & 15 == 0 && (hw >> 4) & 15 <= 4 => cycles = cost::HINT,
            _ => event = Some(Event::Undefined(hw as u16)),
        }
        Step { cycles, event }
    }

    // the 32-bit instructions ARMv6-M has: bl, mrs, msr, dmb/dsb/isb
    fn wide<B: Bus>(&mut self, bus: &mut B, hw1: u32, at: u32) -> Step {
        let hw2 = bus.fetch(at.wrapping_add(2)) as u32;
        self.r[15] = at.wrapping_add(4);
        let mut cycles = cost::ALU;
        let mut event = None;
        if hw1 & 0xF800 == 0xF000 && hw2 & 0xD000 == 0xD000 {
            let s = (hw1 >> 10) & 1;
            let i1 = !((hw2 >> 13) ^ s) & 1;
            let i2 = !((hw2 >> 11) ^ s) & 1;
            let off = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3FF) << 12 | (hw2 & 0x7FF) << 1;
            let off = ((off << 7) as i32 >> 7) as u32;
            self.r[14] = at.wrapping_add(4) | 1;
            self.r[15] = at.wrapping_add(4).wrapping_add(off);
            cycles = cost::BL;
        } else if hw1 == 0xF3EF && hw2 & 0xF000 == 0x8000 {
            let rd = ((hw2 >> 8) & 15) as usize;
            // APSR, IAPSR, EAPSR and XPSR all read the flags, XPSR also has the Thumb bit
            self.r[rd] = match hw2 & 0xFF {
                0..=2 => self.flags(),
                3 => self.flags() | 1 << 24,
                _ => 0,
            };
            cycles = cost::SYSREG;
        } else if hw1 & 0xFFF0 == 0xF380 && hw2 & 0xFF00 == 0x8800 {
            if hw2 & 0xFF <= 3 {
                self.set_flags(self.r[(hw1 & 15) as usize]);
            }
            cycles = cost::SYSREG;
        } else if hw1 == 0xF3BF && matches!(hw2 & 0xFFF0, 0x8F40 | 0x8F50 | 0x8F60) {
            cycles = cost::BARRIER;
        } else {
            event = Some(Event::Undefined(hw1 as u16));
        }
        Step { cycles, event }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flat {
        mem: Vec<u8>,
    }

    impl Bus for Flat {
        fn fetch(&mut self, a: u32) -> u16 {
            u16::from_le_bytes([self.mem[a as usize], self.mem[a as usize + 1]])
        }
        fn read(&mut self, a: u32, size: u32, _: Access) -> u32 {
            (0..size).fold(0, |v, i| v | (self.mem[(a + i) as usize] as u32) << (8 * i))
        }
        fn write(&mut self, a: u32, size: u32, value: u32, _: Access) {
            for i in 0..size {
                self.mem[(a + i) as usize] = (value >> (8 * i)) as u8;
            }
        }
    }

    // run the halfwords from address 0x100, one step per instruction word given
    fn run(cpu: &mut Cpu, code: &[u16]) -> Vec<Option<Event>> {
        let mut bus = Flat {
            mem: vec![0; 0x1000],
        };
        for (i, h) in code.iter().enumerate() {
            bus.mem[0x100 + 2 * i..0x102 + 2 * i].copy_from_slice(&h.to_le_bytes());
        }
        cpu.r[15] = 0x100;
        let end = 0x100 + 2 * code.len() as u32;
        let mut events = vec![];
        while cpu.r[15] < end {
            events.push(cpu.step(&mut bus).event);
        }
        events
    }

    fn cpu(r: &[(usize, u32)], nzcv: u32) -> Cpu {
        let mut c = Cpu::new();
        for &(i, v) in r {
            c.r[i] = v;
        }
        c.set_flags(nzcv << 28);
        c
    }

    fn nzcv(c: &Cpu) -> u32 {
        c.flags() >> 28
    }

    // r0 = hw & 7 as rdn, r1 as rm: alu op in bits 9:6
    fn alu(op: u16) -> u16 {
        0x4000 | op << 6 | 1 << 3
    }

    #[test]
    fn adds_carry_and_overflow() {
        // adds r0, r0, r1 : 0xFFFFFFFF + 1 = 0, C, Z
        let mut c = cpu(&[(0, 0xFFFF_FFFF), (1, 1)], 0);
        run(&mut c, &[0x1840]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        // 0x7FFFFFFF + 1 overflows
        let mut c = cpu(&[(0, 0x7FFF_FFFF), (1, 1)], 0);
        run(&mut c, &[0x1840]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0000, 0b1001));
        // 0x80000000 + 0x80000000 = 0, C, V, Z
        let mut c = cpu(&[(0, 0x8000_0000), (1, 0x8000_0000)], 0);
        run(&mut c, &[0x1840]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0111));
    }

    #[test]
    fn subs_carry_is_not_borrow() {
        // subs r0, r0, r1
        let mut c = cpu(&[(0, 5), (1, 5)], 0);
        run(&mut c, &[0x1A40]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        let mut c = cpu(&[(0, 4), (1, 5)], 0);
        run(&mut c, &[0x1A40]);
        assert_eq!((c.r[0], nzcv(&c)), (0xFFFF_FFFF, 0b1000));
        // 0x80000000 - 1 overflows
        let mut c = cpu(&[(0, 0x8000_0000), (1, 1)], 0);
        run(&mut c, &[0x1A40]);
        assert_eq!((c.r[0], nzcv(&c)), (0x7FFF_FFFF, 0b0011));
        // cmp r0, #0 never borrows
        let mut c = cpu(&[(0, 0)], 0);
        run(&mut c, &[0x2800]);
        assert_eq!(nzcv(&c), 0b0110);
    }

    #[test]
    fn adc_sbc_chain() {
        // adcs r0, r1 with carry in
        let mut c = cpu(&[(0, 0xFFFF_FFFF), (1, 0)], 0b0010);
        run(&mut c, &[alu(5)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        // sbcs r0, r1 : 5 - 5 - 1 (carry clear) = -1, borrow
        let mut c = cpu(&[(0, 5), (1, 5)], 0);
        run(&mut c, &[alu(6)]);
        assert_eq!((c.r[0], nzcv(&c)), (0xFFFF_FFFF, 0b1000));
        // with carry set it is a plain subtract
        let mut c = cpu(&[(0, 5), (1, 5)], 0b0010);
        run(&mut c, &[alu(6)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        // sbcs overflow: 0x80000000 - 0 - 1
        let mut c = cpu(&[(0, 0x8000_0000), (1, 0)], 0);
        run(&mut c, &[alu(6)]);
        assert_eq!((c.r[0], nzcv(&c)), (0x7FFF_FFFF, 0b0011));
    }

    #[test]
    fn register_shifts() {
        // lsls by 0 leaves carry, by 32 gives bit 0, by 33 gives 0
        let mut c = cpu(&[(0, 3), (1, 0)], 0b0010);
        run(&mut c, &[alu(2)]);
        assert_eq!((c.r[0], nzcv(&c)), (3, 0b0010));
        let mut c = cpu(&[(0, 3), (1, 32)], 0);
        run(&mut c, &[alu(2)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        let mut c = cpu(&[(0, 3), (1, 33)], 0b0010);
        run(&mut c, &[alu(2)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0100));
        // only the low byte of the amount counts
        let mut c = cpu(&[(0, 3), (1, 0x101)], 0);
        run(&mut c, &[alu(2)]);
        assert_eq!(c.r[0], 6);
        // lsrs by 32 gives bit 31, by 40 gives 0
        let mut c = cpu(&[(0, 0x8000_0000), (1, 32)], 0);
        run(&mut c, &[alu(3)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        let mut c = cpu(&[(0, 0x8000_0000), (1, 40)], 0b0010);
        run(&mut c, &[alu(3)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0100));
        // asrs by 40 of a negative number
        let mut c = cpu(&[(0, 0x8000_0000), (1, 40)], 0);
        run(&mut c, &[alu(4)]);
        assert_eq!((c.r[0], nzcv(&c)), (0xFFFF_FFFF, 0b1010));
        let mut c = cpu(&[(0, 0x7FFF_FFFF), (1, 31)], 0b0010);
        run(&mut c, &[alu(4)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        // rors by 0 keeps carry, by 32 keeps the value and sets C from bit 31
        let mut c = cpu(&[(0, 0x8000_0001), (1, 0)], 0);
        run(&mut c, &[alu(7)]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0001, 0b1000));
        let mut c = cpu(&[(0, 0x8000_0001), (1, 32)], 0);
        run(&mut c, &[alu(7)]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0001, 0b1010));
        let mut c = cpu(&[(0, 0x0000_0003), (1, 33)], 0);
        run(&mut c, &[alu(7)]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0001, 0b1010));
    }

    #[test]
    fn immediate_shifts() {
        // lsls r0, r1, #0 is movs: carry unchanged
        let mut c = cpu(&[(1, 0x8000_0000)], 0b0010);
        run(&mut c, &[0x0008]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0000, 0b1010));
        // lsls r0, r1, #1 carries out bit 31
        let mut c = cpu(&[(1, 0x8000_0000)], 0);
        run(&mut c, &[0x0048]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        // lsrs #0 means 32
        let mut c = cpu(&[(1, 0x8000_0000)], 0);
        run(&mut c, &[0x0808]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        // asrs #0 means 32
        let mut c = cpu(&[(1, 0x8000_0000)], 0);
        run(&mut c, &[0x1008]);
        assert_eq!((c.r[0], nzcv(&c)), (0xFFFF_FFFF, 0b1010));
        let mut c = cpu(&[(1, 0x7FFF_FFFF)], 0b0010);
        run(&mut c, &[0x1008]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0100));
    }

    #[test]
    fn mul_and_logic_keep_c_and_v() {
        let mut c = cpu(&[(0, 0x10000), (1, 0x10000)], 0b0011);
        run(&mut c, &[alu(13)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0111));
        let mut c = cpu(&[(0, 3), (1, 0x8000_0000)], 0b0011);
        run(&mut c, &[alu(13)]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0000, 0b1011));
        // bics, mvns, orrs, eors, tst do not touch C or V
        let mut c = cpu(&[(0, 0xFF), (1, 0xFF)], 0b0011);
        run(&mut c, &[alu(14)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0111));
        let mut c = cpu(&[(1, 0xFFFF_FFFF)], 0b0001);
        run(&mut c, &[alu(15)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0101));
        let mut c = cpu(&[(0, 0xF0), (1, 0x0F)], 0b0010);
        run(&mut c, &[alu(8)]);
        assert_eq!((c.r[0], nzcv(&c)), (0xF0, 0b0110));
    }

    #[test]
    fn negs_cmp_cmn() {
        // rsbs r0, r1, #0
        let mut c = cpu(&[(1, 0)], 0);
        run(&mut c, &[alu(9)]);
        assert_eq!((c.r[0], nzcv(&c)), (0, 0b0110));
        let mut c = cpu(&[(1, 0x8000_0000)], 0);
        run(&mut c, &[alu(9)]);
        assert_eq!((c.r[0], nzcv(&c)), (0x8000_0000, 0b1001));
        let mut c = cpu(&[(1, 1)], 0);
        run(&mut c, &[alu(9)]);
        assert_eq!((c.r[0], nzcv(&c)), (0xFFFF_FFFF, 0b1000));
        // cmp 1, 2 : borrow, negative
        let mut c = cpu(&[(0, 1), (1, 2)], 0);
        run(&mut c, &[alu(10)]);
        assert_eq!(nzcv(&c), 0b1000);
        // cmn 1, -1 : zero with carry
        let mut c = cpu(&[(0, 1), (1, 0xFFFF_FFFF)], 0);
        run(&mut c, &[alu(11)]);
        assert_eq!(nzcv(&c), 0b0110);
    }

    #[test]
    fn hi_register_ops() {
        // mov r8, r0 does not set flags; add r8, r0 neither; cmp r8, r0 does
        let mut c = cpu(&[(0, 0)], 0b1000);
        run(&mut c, &[0x4680, 0x4480]);
        assert_eq!((c.r[8], nzcv(&c)), (0, 0b1000));
        let mut c = cpu(&[(0, 5), (8, 5)], 0);
        run(&mut c, &[0x4580]);
        assert_eq!(nzcv(&c), 0b0110);
        // mov pc, r0 ignores bit 0 and stays in Thumb
        let mut bus = Flat {
            mem: vec![0; 0x1000],
        };
        let mut c = cpu(&[(0, 0x201)], 0);
        c.r[15] = 0x100;
        bus.mem[0x100..0x102].copy_from_slice(&0x4687u16.to_le_bytes());
        let s = c.step(&mut bus);
        assert_eq!((c.r[15], s.event), (0x200, None));
    }

    #[test]
    fn bx_even_is_an_arm_jump() {
        let mut bus = Flat {
            mem: vec![0; 0x1000],
        };
        let mut c = cpu(&[(0, 0x0800_0100)], 0);
        c.r[15] = 0x100;
        bus.mem[0x100..0x102].copy_from_slice(&0x4780u16.to_le_bytes());
        let s = c.step(&mut bus);
        assert_eq!(s.event, Some(Event::ArmJump(0x0800_0100)));
        assert_eq!(c.r[14], 0x103);
        // bx pc at 0x100 enters ARM at (0x100 + 4) & !3
        c.r[15] = 0x102;
        bus.mem[0x102..0x104].copy_from_slice(&0x4778u16.to_le_bytes());
        let s = c.step(&mut bus);
        assert_eq!(s.event, Some(Event::ArmJump(0x104)));
    }

    #[test]
    fn stm_ldm_and_push_pop() {
        let mut c = cpu(&[(0, 0x800), (1, 0x11), (2, 0x22), (13, 0xF00)], 0);
        // stmia r0!, {r1, r2}; ldmia r0!, {r3} is skipped; push {r1, r2, lr}; pop {r4, r5}
        run(&mut c, &[0xC006, 0xB506, 0xBC30]);
        assert_eq!(c.r[0], 0x808);
        assert_eq!((c.r[4], c.r[5]), (0x11, 0x22));
        assert_eq!(c.r[13], 0xEFC);
        // ldmia with the base in the list does not write back
        let mut c = cpu(&[(0, 0x800)], 0);
        let mut bus = Flat {
            mem: vec![0; 0x1000],
        };
        bus.mem[0x800] = 0x34;
        bus.mem[0x801] = 0x12;
        bus.mem[0x100..0x102].copy_from_slice(&0xC803u16.to_le_bytes());
        c.r[15] = 0x100;
        c.step(&mut bus);
        assert_eq!((c.r[0], c.r[1]), (0x1234, 0));
    }

    #[test]
    fn branches_and_bl() {
        // beq +4 taken when Z
        let mut bus = Flat {
            mem: vec![0; 0x1000],
        };
        let mut c = cpu(&[], 0b0100);
        c.r[15] = 0x100;
        bus.mem[0x100..0x102].copy_from_slice(&0xD002u16.to_le_bytes());
        let s = c.step(&mut bus);
        assert_eq!((c.r[15], s.cycles), (0x108, 2));
        c.set_flags(0);
        c.r[15] = 0x100;
        let s = c.step(&mut bus);
        assert_eq!((c.r[15], s.cycles), (0x102, 1));
        // bl forward by 0x1000: f000 f800 + ... imm10 = 1, imm11 = 0
        c.r[15] = 0x100;
        bus.mem[0x100..0x104].copy_from_slice(&[0x00, 0xF0, 0x00, 0xF8]);
        c.step(&mut bus);
        assert_eq!((c.r[15], c.r[14], 0x104u32), (0x104, 0x105, 0x104));
        // bl backward: f7ff fffe is bl .-0 + 0 ... offset -4: target = at
        c.r[15] = 0x100;
        bus.mem[0x100..0x104].copy_from_slice(&[0xFF, 0xF7, 0xFE, 0xFF]);
        c.step(&mut bus);
        assert_eq!(c.r[15], 0x100);
    }

    #[test]
    fn mrs_msr_and_extend() {
        // mrs r0, apsr ; msr apsr, r1
        let mut c = cpu(&[(1, 0xA000_0000)], 0b0101);
        run(&mut c, &[0xF3EF, 0x8000, 0xF381, 0x8800]);
        assert_eq!(c.r[0], 0x5000_0000);
        assert_eq!(nzcv(&c), 0b1010);
        // sxtb r2, r1 ; uxth r3, r1 ; rev r4, r1 ; revsh r5, r1
        let mut c = cpu(&[(1, 0x1234_80F1)], 0);
        run(&mut c, &[0xB24A, 0xB28B, 0xBA0C, 0xBACD]);
        assert_eq!(c.r[2], 0xFFFF_FFF1);
        assert_eq!(c.r[3], 0x80F1);
        assert_eq!(c.r[4], 0xF180_3412);
        assert_eq!(c.r[5], 0xFFFF_F180);
    }
}
