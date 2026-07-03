#![no_std]
#![no_main]
use core::{
    arch::{asm, naked_asm},
    panic::PanicInfo,
};
use cortex_m_rt::{ExceptionFrame, entry};
use embedded_graphics::{pixelcolor, prelude::*};
use embedded_hal::digital::OutputPin;
use embedded_hal_bus::spi::ExclusiveDevice;
use mipidsi::{Builder, interface::SpiInterface, models, options::ColorInversion};
use rp2040_hal::{Clock, Spi, Timer, Watchdog, fugit::HertzU32, pac};

#[unsafe(link_section = ".boot_loader")]
#[used]
pub static BOOT_LOADER: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

const SCREEN_WIDTH: usize = 240;
const SCREEN_HEIGHT: usize = 160;

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
const ROM_CODE: &[u8] = include_bytes!("pong.gba");

#[panic_handler]
fn panic_handler(_info: &PanicInfo) -> ! {
    loop {}
}

#[repr(C, packed)]
struct Reg4_7 {
    r4: u32,
    r5: u32,
    r6: u32,
    r7: u32,
    lr: u32,
}

// Override the HardFault provided by the cortex-m-rt crate
// Can't use the macro since the function signature does not return and fighting that causes issues
#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn HardFault() {
    // Moving the value of the stack pointer into r0 makes it the arg for the rust function below
    // Also need to save the lr so it does not get clobbered when we call the next function
    // Finally popping the saved value in lr directdly to pc is a return
    naked_asm!(
        "mov r0, sp",
        "push {{r4, r5, r6, r7, lr}}",
        "mov r1, sp",
        "bl {func}",
        "pop {{r4, r5, r6, r7, pc}}",
        func = sym hard_fault
    );
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

#[unsafe(no_mangle)]
extern "C" fn hard_fault(frame: &mut ExceptionFrame, other_regs: &mut Reg4_7) {
    static mut HF_COUNT: usize = 0;
    let hf_count: usize;
    unsafe {
        HF_COUNT += 1;
        hf_count = HF_COUNT;
    }

    // for debug purposes
    let rom_start = ROM_CODE.as_ptr() as u32;
    let diff = frame.pc() - rom_start;

    // get the instruction we were trying to execute
    let instr = unsafe { *((frame.pc() & !1) as *const u16) };

    // ensure this is a str* or ldr* instruction that could be problematic
    if !(
        instr >> 11 == 0b01100      // STR imm5
        || instr >> 9 == 0b0101000  // STR reg
        || instr >> 11 == 0b01110   // STRB imm
        || instr >> 9 == 0b0101010  // STRB reg
        || instr >> 11 == 0b10000   // STRH imm
        || instr >> 9 == 0b0101001  // STRH reg
        || instr >> 11 == 0b01101   // LDR imm5
        || instr >> 9 == 0b0101100  // LDR reg
        || instr >> 11 == 0b01111   // LDRB imm
        || instr >> 9 == 0b0101110  // LDRB reg
        || instr >> 11 == 0b10001   // LDRH imm
        || instr >> 9 == 0b0101101  // LDRH reg
        || instr >> 9 == 0b0101011  // LDRSB reg
        || instr >> 9 == 0b0101111
        // LDRSH reg
    ) {
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
        4 => base_addr = other_regs.r4,
        5 => base_addr = other_regs.r5,
        6 => base_addr = other_regs.r6,
        7 => base_addr = other_regs.r7,
        _ => unreachable!(),
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
            4 => other_regs.r4 = base_addr,
            5 => other_regs.r5 = base_addr,
            6 => other_regs.r6 = base_addr,
            7 => other_regs.r7 = base_addr,
            _ => unreachable!(),
        }
    }
}

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let mut core = pac::CorePeripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);

    let clocks = rp2040_hal::clocks::init_clocks_and_plls(
        12_000_000,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let mut timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);

    let sio = rp2040_hal::Sio::new(pac.SIO);
    let pins = rp2040_hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let spi_sclk = pins.gpio10.into_function::<rp2040_hal::gpio::FunctionSpi>();
    let spi_mosi = pins.gpio11.into_function::<rp2040_hal::gpio::FunctionSpi>();
    let spi_miso = pins.gpio12.into_function::<rp2040_hal::gpio::FunctionSpi>();
    let spi_cs = pins.gpio9.into_push_pull_output();
    let dc_pin = pins.gpio8.into_push_pull_output();
    let reset_pin = pins.gpio15.into_push_pull_output();
    let mut bk_light_pin = pins.gpio13.into_push_pull_output();

    bk_light_pin.set_high();

    let spi_bus = Spi::<_, _, _, 8>::new(pac.SPI1, (spi_mosi, spi_miso, spi_sclk)).init(
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
        HertzU32::Hz(60_000_000),  // Set SPI bus speed (e.g., 1 MHz)
        embedded_hal::spi::MODE_0, // SPI Mode (0, 1, 2, or 3)
    );

    let spi_device = ExclusiveDevice::new(spi_bus, spi_cs, timer).unwrap();

    let mut buffer = [0u8; 512];
    let di = SpiInterface::new(spi_device, dc_pin, &mut buffer);
    let mut display = Builder::new(models::ST7789, di)
        .reset_pin(reset_pin)
        .display_size(SCREEN_WIDTH as u16, SCREEN_HEIGHT as u16)
        .invert_colors(ColorInversion::Inverted)
        .init(&mut timer)
        .unwrap();

    // display.set_pixel(50, 50, pixelcolor::Rgb565::WHITE);
    display.clear(pixelcolor::Rgb565::GREEN);

    loop {}

    let rom_addr = ROM_CODE.as_ptr() as u32;
    let thumb_start = rom_addr + 0xdc + 1; // last bit needs to be 1 for thumb mode
    unsafe { asm!("bx {}", in(reg) thumb_start) };

    loop {}
}
