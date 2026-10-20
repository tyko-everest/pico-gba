// Where a GBA ROM starts running its own code.
//
// The ROM begins with an ARM crt0 that sets up the CPU modes and stacks, then switches to Thumb
// with `add rX, pc, #1; bx rX`. The ARM code cannot run on the RP2040 and the stacks stay native,
// so the firmware starts at the Thumb code the crt0 jumps to.

const ADD_PC_1: u32 = 0xE28F_0001;
const BX: u32 = 0xE12F_FF10;
const B_ALWAYS: u32 = 0xEA00_0000;

// How far to look for the switch to Thumb, in instructions
const SCAN: usize = 64;

fn word(rom: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        rom.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

// The ROM offset of the first Thumb instruction after the crt0, if the crt0 is recognised
pub fn thumb_entry(rom: &[u8]) -> Option<usize> {
    let mut at = 0;
    let mut scanned = 0;
    while scanned < SCAN {
        let w = word(rom, at)?;
        if w & 0xFF00_0000 == B_ALWAYS {
            // a branch is followed, the crt0 hops over the header and the multiboot area
            let offset = (((w & 0x00FF_FFFF) << 8) as i32 >> 6) as isize;
            at = at.checked_add_signed(offset + 8)?;
        } else if w & 0xFFFF_0FFF == ADD_PC_1 {
            // the register the address goes to has to be the one bx jumps through
            let rd = (w >> 12) & 0xF;
            if word(rom, at + 4)? == BX | rd {
                return Some(at + 8);
            }
            at += 4;
        } else {
            at += 4;
        }
        scanned += 1;
    }
    None
}

// Where the crt0 of a ROM sends the game, and the interrupt handler it installs.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    // the GBA address of the first Thumb instruction to run (bit 0 set)
    pub thumb: u32,
    // the address the crt0 stores at the top of IWRAM for the BIOS to call on interrupts, 0 when
    // it stores none
    pub irq_handler: u32,
}

const INTR_VECTOR: u32 = 0x0300_7FFC;
const ROM_BASE: u32 = 0x0800_0000;
// How many instructions of the crt0 are followed
const FOLLOW: usize = 160;

// Run the ARM crt0 a ROM starts with just far enough to find where it goes to Thumb: the values it
// puts in registers (a literal, mov, or pc relative address) are followed, and a bx through one of
// them is the end. Everything else is skipped, the stacks it sets are the firmware's. read32
// returns the ROM word at an offset.
pub fn find_entry(mut read32: impl FnMut(u32) -> u32) -> Option<Entry> {
    let mut regs: [Option<u32>; 16] = [None; 16];
    let mut irq_handler = 0;
    let mut at = 0u32;
    for _ in 0..FOLLOW {
        let w = read32(at);
        let pc = ROM_BASE + at + 8;
        let rd = ((w >> 12) & 0xF) as usize;
        let rn = ((w >> 16) & 0xF) as usize;
        let imm = (w & 0xFF).rotate_right(((w >> 8) & 0xF) * 2);
        let cond_al = w >> 28 == 0xE;
        if w & 0xFF00_0000 == B_ALWAYS {
            let offset = (((w & 0x00FF_FFFF) << 8) as i32 >> 6) as i64;
            at = (at as i64 + offset + 8) as u32;
            continue;
        }
        if cond_al {
            match w & 0x0FFF_F000 {
                // mov rd, #imm
                x if x & 0x0FEF_0000 == 0x03A0_0000 => regs[rd] = Some(imm),
                // add or sub rd, pc, #imm (adr)
                x if x & 0x0FEF_0000 == 0x028F_0000 => regs[rd] = Some(pc.wrapping_add(imm)),
                x if x & 0x0FEF_0000 == 0x024F_0000 => regs[rd] = Some(pc.wrapping_sub(imm)),
                _ => {}
            }
            // ldr rd, [pc, #imm12]
            if w & 0x0F7F_0000 == 0x051F_0000 {
                let offset = w & 0xFFF;
                let from = if w & (1 << 23) != 0 {
                    at + 8 + offset
                } else {
                    at + 8 - offset
                };
                regs[rd] = Some(read32(from));
            }
            // str rd, [rn]
            if w & 0x0FF0_0FFF == 0x0580_0000 && regs[rn] == Some(INTR_VECTOR) {
                if let Some(v) = regs[rd] {
                    irq_handler = v;
                }
            }
            // bx rm
            if w & 0x0FFF_FFF0 == BX & 0x0FFF_FFF0 {
                let rm = (w & 0xF) as usize;
                return regs[rm]
                    .filter(|t| t & 1 != 0)
                    .map(|thumb| Entry { thumb, irq_handler });
            }
        }
        at += 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // crt0 of a ROM whose header branches to 0x10, then runs mov, add, bx
    #[test]
    fn follows_the_header_branch() {
        let mut rom = [0u8; 0x40];
        let put =
            |rom: &mut [u8], at: usize, w: u32| rom[at..at + 4].copy_from_slice(&w.to_le_bytes());
        put(&mut rom, 0, B_ALWAYS | 2); // b 0x10
        put(&mut rom, 0x10, 0xE3A0_0301); // mov r0, #0x4000000
        put(&mut rom, 0x14, ADD_PC_1); // add r0, pc, #1
        put(&mut rom, 0x18, BX); // bx r0
        assert_eq!(thumb_entry(&rom), Some(0x1C));
    }

    #[test]
    fn needs_the_bx_through_the_same_register() {
        let mut rom = [0u8; 0x40];
        rom[0..4].copy_from_slice(&(B_ALWAYS | 2).to_le_bytes());
        rom[0x14..0x18].copy_from_slice(&(ADD_PC_1 | 1 << 12).to_le_bytes());
        rom[0x18..0x1C].copy_from_slice(&BX.to_le_bytes());
        assert_eq!(thumb_entry(&rom), None);
    }
}
