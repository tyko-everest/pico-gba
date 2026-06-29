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

#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn test() -> ! {
    naked_asm!("b {}", sym test);
}
#[exception(trampoline = false)]
unsafe fn HardFault() -> ! {
    let mut sp_ptr: u32;
    unsafe { asm!("mov {}, sp", out(reg) sp_ptr) };
    let frame_ptr = sp_ptr - size_of::<ExceptionFrame>() as u32;

    let frame: &mut ExceptionFrame = unsafe { &mut *(frame_ptr as *mut ExceptionFrame) };
    let test_ptr = test as extern "C" fn() -> ! as u32;
    unsafe {
        frame.set_pc(test_ptr);
    }

    loop {}

    unsafe {
        asm!(
            "mov sp, r7",
            "pop {{r0, r7}}",
            "eors r0, r0, r7",
            "eors r7, r0, r7",
            "eors r0, r0, r7",
            "adds r0, #2",
            "bx r0"
        )
    }

    loop {}
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
