// Encoders for the ARMv6-M (Cortex-M0+) Thumb instructions the translator emits. Low registers
// are r0-r7, anything above is a high register that only a few instructions can reach.
use super::decode::{DpOp, Shift};

pub const NOP: u16 = 0xBF00;
// UDF #0xFE, raises a HardFault the handler recognises as an unresolved exit
pub const TRAP: u16 = 0xDEFE;

pub const SP: u8 = 13;
pub const LR: u8 = 14;
pub const PC: u8 = 15;

pub fn is_low(r: u8) -> bool {
    r < 8
}

// ALU register opcodes, 0x4000 | op << 6 | rm << 3 | rdn
pub const AND: u16 = 0x0;
pub const EOR: u16 = 0x1;
pub const LSL: u16 = 0x2;
pub const LSR: u16 = 0x3;
pub const ASR: u16 = 0x4;
pub const ADC: u16 = 0x5;
pub const SBC: u16 = 0x6;
pub const ROR: u16 = 0x7;
pub const TST: u16 = 0x8;
pub const NEG: u16 = 0x9;
pub const CMP: u16 = 0xA;
pub const CMN: u16 = 0xB;
pub const ORR: u16 = 0xC;
pub const MUL: u16 = 0xD;
pub const BIC: u16 = 0xE;
pub const MVN: u16 = 0xF;

pub fn alu(op: u16, rdn: u8, rm: u8) -> u16 {
    debug_assert!(rdn < 8 && rm < 8);
    0x4000 | op << 6 | (rm as u16) << 3 | rdn as u16
}

pub fn neg(rd: u8, rm: u8) -> u16 {
    alu(NEG, rd, rm)
}

pub fn mvn(rd: u8, rm: u8) -> u16 {
    alu(MVN, rd, rm)
}

// the ALU opcode that shifts by a register
pub fn shift_op(kind: Shift) -> u16 {
    match kind {
        Shift::Lsl => LSL,
        Shift::Lsr => LSR,
        Shift::Asr => ASR,
        Shift::Ror => ROR,
    }
}

// The ALU opcode that does a data processing op as rdn = rdn op rm, for the ones that have one
// (TEQ is an EOR whose result is thrown away).
pub fn dp_op(op: DpOp) -> Option<u16> {
    Some(match op {
        DpOp::And => AND,
        DpOp::Eor | DpOp::Teq => EOR,
        DpOp::Adc => ADC,
        DpOp::Sbc => SBC,
        DpOp::Tst => TST,
        DpOp::Cmp => CMP,
        DpOp::Cmn => CMN,
        DpOp::Orr => ORR,
        DpOp::Bic => BIC,
        _ => return None,
    })
}

// MOV rd, rm between any registers, does not touch the flags
pub fn mov(rd: u8, rm: u8) -> u16 {
    0x4600 | ((rd as u16 >> 3) << 7) | (rm as u16) << 3 | (rd as u16 & 7)
}

// ADD rdn, rm between any registers, does not touch the flags
pub fn add_hi(rdn: u8, rm: u8) -> u16 {
    0x4400 | ((rdn as u16 >> 3) << 7) | (rm as u16) << 3 | (rdn as u16 & 7)
}

pub fn bx(rm: u8) -> u16 {
    0x4700 | (rm as u16) << 3
}

// LDR rt, [pc, #words * 4] relative to the word aligned pc
pub fn ldr_pc(rt: u8, words: u16) -> u16 {
    0x4800 | (rt as u16) << 8 | words
}

pub fn movs_imm(rd: u8, imm: u16) -> u16 {
    0x2000 | (rd as u16) << 8 | imm
}

pub fn cmp_imm(rn: u8, imm: u16) -> u16 {
    0x2800 | (rn as u16) << 8 | imm
}

pub fn adds_imm8(rdn: u8, imm: u16) -> u16 {
    0x3000 | (rdn as u16) << 8 | imm
}

pub fn subs_imm8(rdn: u8, imm: u16) -> u16 {
    0x3800 | (rdn as u16) << 8 | imm
}

pub fn adds_imm3(rd: u8, rn: u8, imm: u16) -> u16 {
    0x1C00 | imm << 6 | (rn as u16) << 3 | rd as u16
}

pub fn subs_imm3(rd: u8, rn: u8, imm: u16) -> u16 {
    0x1E00 | imm << 6 | (rn as u16) << 3 | rd as u16
}

pub fn adds_reg(rd: u8, rn: u8, rm: u8) -> u16 {
    0x1800 | (rm as u16) << 6 | (rn as u16) << 3 | rd as u16
}

pub fn subs_reg(rd: u8, rn: u8, rm: u8) -> u16 {
    0x1A00 | (rm as u16) << 6 | (rn as u16) << 3 | rd as u16
}

// MOVS rd, rm, only N and Z change
pub fn movs_reg(rd: u8, rm: u8) -> u16 {
    lsls_imm(rd, rm, 0)
}

pub fn lsls_imm(rd: u8, rm: u8, imm5: u16) -> u16 {
    imm5 << 6 | (rm as u16) << 3 | rd as u16
}

pub fn lsrs_imm(rd: u8, rm: u8, imm5: u16) -> u16 {
    0x0800 | imm5 << 6 | (rm as u16) << 3 | rd as u16
}

pub fn asrs_imm(rd: u8, rm: u8, imm5: u16) -> u16 {
    0x1000 | imm5 << 6 | (rm as u16) << 3 | rd as u16
}

pub fn uxtb(rd: u8, rm: u8) -> u16 {
    0xB2C0 | (rm as u16) << 3 | rd as u16
}

pub fn uxth(rd: u8, rm: u8) -> u16 {
    0xB280 | (rm as u16) << 3 | rd as u16
}

pub fn sxtb(rd: u8, rm: u8) -> u16 {
    0xB240 | (rm as u16) << 3 | rd as u16
}

pub fn sxth(rd: u8, rm: u8) -> u16 {
    0xB200 | (rm as u16) << 3 | rd as u16
}

// load and store with an immediate offset, the immediate is already divided by the access size
pub fn str_imm(rt: u8, rn: u8, imm5: u16) -> u16 {
    0x6000 | imm5 << 6 | (rn as u16) << 3 | rt as u16
}

pub fn ldr_imm(rt: u8, rn: u8, imm5: u16) -> u16 {
    0x6800 | imm5 << 6 | (rn as u16) << 3 | rt as u16
}

pub fn strb_imm(rt: u8, rn: u8, imm5: u16) -> u16 {
    0x7000 | imm5 << 6 | (rn as u16) << 3 | rt as u16
}

pub fn ldrb_imm(rt: u8, rn: u8, imm5: u16) -> u16 {
    0x7800 | imm5 << 6 | (rn as u16) << 3 | rt as u16
}

pub fn strh_imm(rt: u8, rn: u8, imm5: u16) -> u16 {
    0x8000 | imm5 << 6 | (rn as u16) << 3 | rt as u16
}

pub fn ldrh_imm(rt: u8, rn: u8, imm5: u16) -> u16 {
    0x8800 | imm5 << 6 | (rn as u16) << 3 | rt as u16
}

pub fn ldr_reg(rt: u8, rn: u8, rm: u8) -> u16 {
    0x5800 | (rm as u16) << 6 | (rn as u16) << 3 | rt as u16
}

pub fn str_sp(rt: u8, words: u16) -> u16 {
    0x9000 | (rt as u16) << 8 | words
}

pub fn ldr_sp(rt: u8, words: u16) -> u16 {
    0x9800 | (rt as u16) << 8 | words
}

// ADD rd, sp, #words * 4, does not touch the flags
pub fn add_rd_sp(rd: u8, words: u16) -> u16 {
    0xA800 | (rd as u16) << 8 | words
}

pub fn add_sp(words: u16) -> u16 {
    0xB000 | words
}

pub fn sub_sp(words: u16) -> u16 {
    0xB080 | words
}

pub fn push(list: u8, lr: bool) -> u16 {
    0xB400 | (lr as u16) << 8 | list as u16
}

pub fn pop(list: u8, pc: bool) -> u16 {
    0xBC00 | (pc as u16) << 8 | list as u16
}

pub fn stmia(rn: u8, list: u8) -> u16 {
    0xC000 | (rn as u16) << 8 | list as u16
}

pub fn ldmia(rn: u8, list: u8) -> u16 {
    0xC800 | (rn as u16) << 8 | list as u16
}

// conditional branch, the offset is in halfwords from the instruction after this one
pub fn b_cond(cond: u8, imm8: i32) -> u16 {
    0xD000 | (cond as u16) << 8 | (imm8 as u16 & 0xFF)
}

// unconditional branch, the offset is in halfwords from the instruction after this one
pub fn b(imm11: i32) -> u16 {
    0xE000 | (imm11 as u16 & 0x7FF)
}

pub fn svc(num: u8) -> u16 {
    0xDF00 | num as u16
}

// MRS rd, APSR as two halfwords
pub fn mrs(rd: u8) -> [u16; 2] {
    [0xF3EF, 0x8000 | (rd as u16) << 8]
}

// MSR APSR_nzcvq, rn as two halfwords
pub fn msr(rn: u8) -> [u16; 2] {
    [0xF380 | rn as u16, 0x8800]
}
