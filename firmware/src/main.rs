#![no_std]
#![no_main]
use core::{
    arch::{asm, naked_asm},
    panic::PanicInfo,
};
use cortex_m_rt::{ExceptionFrame, entry, exception};

#[unsafe(link_section = ".boot_loader")]
#[used]
pub static BOOT_LOADER: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn HardFault() {
    naked_asm!("mov r0, sp", "push {{lr}}", "bl {func}", "pop {{pc}}", func = sym hard_fault);
}

#[unsafe(no_mangle)]
fn hard_fault(frame: &mut ExceptionFrame) {
    unsafe {
        frame.set_pc(frame.pc() + 2);
    }
}

#[entry]
fn main() -> ! {
    let ptr = 0x0800_0000 as *mut usize;
    unsafe {
        *ptr = 123;
    }
    let hi = true;
    loop {}
}
