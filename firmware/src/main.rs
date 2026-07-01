#![no_std]
#![no_main]
use core::{
    arch::{asm, naked_asm},
    panic::PanicInfo,
};
use cortex_m_rt::{ExceptionFrame, entry};

#[unsafe(link_section = ".boot_loader")]
#[used]
pub static BOOT_LOADER: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

const fn KB(num: u32) -> u32 {
    num << 10
}

// the GBA memory map
const REG_START: u32 = 0x0400_0000;
const REG_LEN: u32 = KB(1);

const PALETTE_START: u32 = 0x0500_0000;
const PALETTE_LEN: u32 = KB(1);

const VRAM_START: u32 = 0x0600_0000;
const VRAM_LEN: u32 = KB(96);

const OAM_START: u32 = 0x0700_0000;
const OAM_LEN: u32 = KB(1);

static mut NEW_REG: [u8; REG_LEN as usize] = [0; REG_LEN as usize];
static mut NEW_PALETTE: [u8; PALETTE_LEN as usize] = [0; PALETTE_LEN as usize];
static mut NEW_VRAM: [u8; VRAM_LEN as usize] = [0; VRAM_LEN as usize];
static mut NEW_OAM: [u8; OAM_LEN as usize] = [0; OAM_LEN as usize];

// part of the rom loaded in
const ROM_CODE: &[u8] = include_bytes!("rom_partial_10K.gba");

#[panic_handler]
fn panic_handler(_info: &PanicInfo) -> ! {
    loop {}
}

// Override the HardFault provided by the cortex-m-rt crate
// Can't use the macro since the function signature does not return and fighting that causes issues
#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn HardFault() {
    // Moving the value of the stack pointer into r0 makes it the arg for the rust function below
    // Also need to save the lr so it does not get clobbered when we call the next function
    // Finally popping the saved value in lr directly to pc is a return
    naked_asm!("mov r0, sp", "push {{lr}}", "bl {func}", "pop {{pc}}", func = sym hard_fault);
}

#[allow(static_mut_refs)]
fn fix_addr(gba_addr: u32) -> Option<u32> {
    if gba_addr >= REG_START && gba_addr < REG_START + REG_LEN {
        let new_reg_start = unsafe { NEW_REG.as_ptr() as u32 };
        Some(gba_addr + new_reg_start - REG_START)
    } else if gba_addr >= PALETTE_START && gba_addr < PALETTE_START + PALETTE_LEN {
        let new_palette_start = unsafe { NEW_PALETTE.as_ptr() as u32 };
        Some(gba_addr + new_palette_start - PALETTE_START)
    } else if gba_addr >= VRAM_START && gba_addr < VRAM_START + VRAM_LEN {
        let new_vram_start = unsafe { NEW_VRAM.as_ptr() as u32 };
        Some(gba_addr + new_vram_start - VRAM_START)
    } else if gba_addr >= OAM_START && gba_addr < OAM_START + OAM_LEN {
        let new_oam_start = unsafe { NEW_OAM.as_ptr() as u32 };
        Some(gba_addr + new_oam_start - OAM_START)
    } else {
        None
    }
}

static mut HF_COUNT: usize = 0;

#[unsafe(no_mangle)]
extern "C" fn hard_fault(frame: &mut ExceptionFrame) {
    // get the instruction we were trying to execute
    let instr = unsafe { *(frame.pc() as *const u16) };

    // for debug purposes
    let rom_start = ROM_CODE.as_ptr() as u32;

    // ensure this is a str* instruction
    if !(instr >> 11 == 0b01100
        || instr >> 9 == 0b0101000
        || instr >> 11 == 0b01110
        || instr >> 9 == 0b0101010
        || instr >> 11 == 0b10000
        || instr >> 9 == 0b0101001)
    {
        // nothing else handled currently
        panic!()
    }

    // this register is always encoded here
    let base_reg = (instr >> 3) & 0b111;

    let base_addr: u32;
    match base_reg {
        0 => base_addr = frame.r0(),
        1 => base_addr = frame.r1(),
        2 => base_addr = frame.r2(),
        3 => base_addr = frame.r3(),
        // figure out how to handle r4-r7 cleanly
        _ => todo!(),
    };

    let Some(new_base_addr) = fix_addr(base_addr) else {
        // we did not know how to translate this address, not recoverable
        panic!()
    };

    unsafe {
        match base_reg {
            0 => frame.set_r0(new_base_addr),
            1 => frame.set_r1(new_base_addr),
            2 => frame.set_r2(new_base_addr),
            3 => frame.set_r3(new_base_addr),
            // figure out how to handle r4-r7 cleanly
            _ => todo!(),
        }
    }

    // unsafe {
    //     HF_COUNT += 1;
    //     if HF_COUNT == 1 {
    //         panic!()
    //     }
    // }
}

#[entry]
fn main() -> ! {
    let rom_addr = ROM_CODE.as_ptr() as u32 + 1; // last bit needs to be 1 for thumb mode

    unsafe { asm!("bx {}", in(reg) rom_addr) };

    loop {}
}
