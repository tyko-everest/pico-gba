// ARMv4T ARM-mode instruction decoder, covering the integer instructions the translator can
// handle. Everything else decodes to Insn::Unsupported so the translator can report it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DpOp {
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Rsc,
    Tst,
    Teq,
    Cmp,
    Cmn,
    Orr,
    Mov,
    Bic,
    Mvn,
}

impl DpOp {
    fn from_bits(bits: u32) -> DpOp {
        use DpOp::*;
        [
            And, Eor, Sub, Rsb, Add, Adc, Sbc, Rsc, Tst, Teq, Cmp, Cmn, Orr, Mov, Bic, Mvn,
        ][(bits & 0xF) as usize]
    }

    pub fn is_test(self) -> bool {
        matches!(self, DpOp::Tst | DpOp::Teq | DpOp::Cmp | DpOp::Cmn)
    }

    pub fn is_logical(self) -> bool {
        matches!(
            self,
            DpOp::And
                | DpOp::Eor
                | DpOp::Orr
                | DpOp::Bic
                | DpOp::Mov
                | DpOp::Mvn
                | DpOp::Tst
                | DpOp::Teq
        )
    }

    pub fn uses_rn(self) -> bool {
        !matches!(self, DpOp::Mov | DpOp::Mvn)
    }

    pub fn uses_carry_in(self) -> bool {
        matches!(self, DpOp::Adc | DpOp::Sbc | DpOp::Rsc)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

impl Shift {
    fn from_bits(bits: u32) -> Shift {
        match bits & 3 {
            0 => Shift::Lsl,
            1 => Shift::Lsr,
            2 => Shift::Asr,
            _ => Shift::Ror,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op2 {
    // rotated is true when the immediate was encoded with a non zero rotation, which makes the
    // carry out of the shifter bit 31 of the value
    Imm { value: u32, rotated: bool },
    // amount is the raw encoded 5 bit amount, LSR/ASR #0 mean 32 and ROR #0 means RRX
    Reg { rm: u8, kind: Shift, amount: u8 },
    RegShift { rm: u8, kind: Shift, rs: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offset {
    Imm(u32),
    Reg { rm: u8, kind: Shift, amount: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    Word,
    Byte,
    Half,
    SignedByte,
    SignedHalf,
}

impl MemKind {
    // log2 of the number of bytes the access moves
    pub fn size_shift(self) -> u32 {
        match self {
            MemKind::Word => 2,
            MemKind::Half | MemKind::SignedHalf => 1,
            MemKind::Byte | MemKind::SignedByte => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsrSrc {
    Reg(u8),
    Imm(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MulOp {
    pub acc: bool,
    pub s: bool,
    pub rd: u8,
    pub rm: u8,
    pub rs: u8,
    pub rn: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemOp {
    pub load: bool,
    pub kind: MemKind,
    pub pre: bool,
    pub up: bool,
    pub wb: bool,
    pub rd: u8,
    pub rn: u8,
    pub off: Offset,
}

impl MemOp {
    // does the base register change
    pub fn update(&self) -> bool {
        !self.pre || self.wb
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockOp {
    pub load: bool,
    pub pre: bool,
    pub up: bool,
    pub wb: bool,
    pub rn: u8,
    pub list: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insn {
    Dp {
        op: DpOp,
        s: bool,
        rd: u8,
        rn: u8,
        op2: Op2,
    },
    Mul(MulOp),
    Mem(MemOp),
    Block(BlockOp),
    // Only the flags of the status registers are modelled, there are no modes
    Mrs {
        rd: u8,
        spsr: bool,
    },
    Msr {
        spsr: bool,
        // is the flags field part of the write
        flags: bool,
        src: MsrSrc,
    },
    Branch {
        link: bool,
        target: u32,
    },
    Bx {
        rm: u8,
    },
    // the BIOS call number, which GBA code puts in bits 16-23 of the comment field
    Swi {
        num: u8,
    },
    Unsupported,
}

#[derive(Clone, Copy, Debug)]
pub struct Decoded {
    pub cond: u8,
    pub insn: Insn,
}

pub const COND_AL: u8 = 14;

fn rot_imm(word: u32) -> (u32, bool) {
    let rot = ((word >> 8) & 0xF) * 2;
    ((word & 0xFF).rotate_right(rot), rot != 0)
}

// pc is the address of the instruction itself
pub fn decode(word: u32, pc: u32) -> Decoded {
    let cond = (word >> 28) as u8;
    let unsupported = Decoded {
        cond,
        insn: Insn::Unsupported,
    };
    if cond == 15 {
        return unsupported;
    }
    let rn = ((word >> 16) & 0xF) as u8;
    let rd = ((word >> 12) & 0xF) as u8;
    let s = word & (1 << 20) != 0;
    let insn = match (word >> 25) & 7 {
        0b000 => {
            if word & 0x0FFF_FFF0 == 0x012F_FF10 {
                Insn::Bx {
                    rm: (word & 0xF) as u8,
                }
            } else if word & 0x0FC0_00F0 == 0x0000_0090 {
                // MUL/MLA, the destination is in the bits DP uses for Rn
                Insn::Mul(MulOp {
                    acc: word & (1 << 21) != 0,
                    s,
                    rd: rn,
                    rm: (word & 0xF) as u8,
                    rs: ((word >> 8) & 0xF) as u8,
                    rn: rd,
                })
            } else if word & 0x90 == 0x90 && word & 0x60 != 0 {
                // halfword and signed transfers
                let load = word & (1 << 20) != 0;
                let sh = (word >> 5) & 3;
                let kind = match sh {
                    1 => MemKind::Half,
                    2 => MemKind::SignedByte,
                    _ => MemKind::SignedHalf,
                };
                if !load && sh != 1 {
                    // LDRD/STRD are not ARMv4T
                    return unsupported;
                }
                let imm = word & (1 << 22) != 0;
                let off = if imm {
                    Offset::Imm(((word >> 4) & 0xF0) | (word & 0xF))
                } else {
                    Offset::Reg {
                        rm: (word & 0xF) as u8,
                        kind: Shift::Lsl,
                        amount: 0,
                    }
                };
                let pre = word & (1 << 24) != 0;
                let wb = word & (1 << 21) != 0;
                if !pre && wb {
                    return unsupported;
                }
                Insn::Mem(MemOp {
                    load,
                    kind,
                    pre,
                    up: word & (1 << 23) != 0,
                    wb: wb || !pre,
                    rd,
                    rn,
                    off,
                })
            } else if word & 0x90 == 0x90 {
                // long multiplies, swap
                return unsupported;
            } else if word & 0x0FBF_0FFF == 0x010F_0000 {
                Insn::Mrs {
                    rd,
                    spsr: word & (1 << 22) != 0,
                }
            } else if word & 0x0FB0_FFF0 == 0x0120_F000 {
                Insn::Msr {
                    spsr: word & (1 << 22) != 0,
                    flags: word & (1 << 19) != 0,
                    src: MsrSrc::Reg((word & 0xF) as u8),
                }
            } else if !s && (8..=11).contains(&((word >> 21) & 0xF)) {
                // the rest of the TST/TEQ/CMP/CMN without the S bit space
                return unsupported;
            } else {
                let op = DpOp::from_bits(word >> 21);
                let rm = (word & 0xF) as u8;
                let kind = Shift::from_bits(word >> 5);
                let op2 = if word & 0x10 == 0 {
                    Op2::Reg {
                        rm,
                        kind,
                        amount: ((word >> 7) & 0x1F) as u8,
                    }
                } else {
                    Op2::RegShift {
                        rm,
                        kind,
                        rs: ((word >> 8) & 0xF) as u8,
                    }
                };
                dp(op, s, rd, rn, op2)
            }
        }
        0b001 => {
            if word & 0x0FB0_F000 == 0x0320_F000 {
                let (value, _) = rot_imm(word);
                return Decoded {
                    cond,
                    insn: Insn::Msr {
                        spsr: word & (1 << 22) != 0,
                        flags: word & (1 << 19) != 0,
                        src: MsrSrc::Imm(value),
                    },
                };
            }
            if !s && (8..=11).contains(&((word >> 21) & 0xF)) {
                return unsupported;
            }
            let (value, rotated) = rot_imm(word);
            dp(
                DpOp::from_bits(word >> 21),
                s,
                rd,
                rn,
                Op2::Imm { value, rotated },
            )
        }
        0b010 | 0b011 => {
            let reg_off = word & (1 << 25) != 0;
            if reg_off && word & 0x10 != 0 {
                return unsupported;
            }
            let pre = word & (1 << 24) != 0;
            let wb = word & (1 << 21) != 0;
            // the T variants (post index with W set) translate user mode accesses
            if !pre && wb {
                return unsupported;
            }
            let off = if reg_off {
                Offset::Reg {
                    rm: (word & 0xF) as u8,
                    kind: Shift::from_bits(word >> 5),
                    amount: ((word >> 7) & 0x1F) as u8,
                }
            } else {
                Offset::Imm(word & 0xFFF)
            };
            Insn::Mem(MemOp {
                load: word & (1 << 20) != 0,
                kind: if word & (1 << 22) != 0 {
                    MemKind::Byte
                } else {
                    MemKind::Word
                },
                pre,
                up: word & (1 << 23) != 0,
                wb: wb || !pre,
                rd,
                rn,
                off,
            })
        }
        0b100 => {
            if word & (1 << 22) != 0 {
                // S bit, user bank or CPSR restore
                return unsupported;
            }
            Insn::Block(BlockOp {
                load: word & (1 << 20) != 0,
                pre: word & (1 << 24) != 0,
                up: word & (1 << 23) != 0,
                wb: word & (1 << 21) != 0,
                rn,
                list: word as u16,
            })
        }
        0b101 => {
            let offset = (((word & 0x00FF_FFFF) << 8) as i32 >> 6) as u32;
            Insn::Branch {
                link: word & (1 << 24) != 0,
                target: pc.wrapping_add(8).wrapping_add(offset),
            }
        }
        0b111 if word & (1 << 24) != 0 => Insn::Swi {
            num: ((word >> 16) & 0xFF) as u8,
        },
        _ => return unsupported,
    };
    Decoded { cond, insn }
}

fn dp(op: DpOp, s: bool, rd: u8, rn: u8, op2: Op2) -> Insn {
    // MOV pc, rm is a plain register jump
    if op == DpOp::Mov
        && !s
        && rd == 15
        && let Op2::Reg {
            rm,
            kind: Shift::Lsl,
            amount: 0,
        } = op2
        && rm != 15
    {
        return Insn::Bx { rm };
    }
    Insn::Dp {
        op,
        // tests always set the flags, keep that explicit for the translator
        s: s || op.is_test(),
        rd,
        rn,
        op2,
    }
}
