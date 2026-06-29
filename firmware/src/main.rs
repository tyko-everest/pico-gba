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

const OLD_START: u32 = 0x0800_0000;
const NEW_START: u32 = 0x2000_0000;
const LENGTH: u32 = 256 << 10;

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

#[unsafe(no_mangle)]
extern "C" fn hard_fault(frame: &mut ExceptionFrame) {
    let pc = frame.pc() as *const u16;
    let instr = unsafe { *pc };

    if instr >> 11 & 0b01100 == 0b01100 {
        // STR (immediate)
        let imm5 = (instr >> 6) & 0b11111;
        let base = (instr >> 3) & 0b111;
        let source = instr & 0b111;
        let mut addr: u32;
        if source <= 3 && base <= 3 {
            match base {
                0 => addr = frame.r0(),
                1 => addr = frame.r1(),
                2 => addr = frame.r2(),
                3 => addr = frame.r3(),
                _ => panic!(),
            }
        } else {
            todo!()
        }
        addr += imm5 as u32;
        if addr >= OLD_START && addr < OLD_START + LENGTH {
            addr += NEW_START - OLD_START;
        }
        unsafe {
            match base {
                0 => frame.set_r0(addr),
                1 => frame.set_r1(addr),
                2 => frame.set_r2(addr),
                3 => frame.set_r3(addr),
                _ => panic!(),
            }
        }
    } else {
        panic!()
    }
}

#[entry]
fn main() -> ! {
    let invalid_ptr = OLD_START as *mut usize;
    let valid_ptr = NEW_START as *mut usize;
    unsafe {
        *invalid_ptr = 123;
    }
    // valid_ptr now has the value we attempted to save at invalid_ptr
    let val = unsafe { *valid_ptr };
    assert!(val == 123);
    // assert passes and we arrive here
    loop {}
}
