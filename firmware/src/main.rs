#![no_std]
#![no_main]
#![allow(static_mut_refs)]
use common::{profile, registers::*, video::*};
use core::{
    arch::{asm, naked_asm},
    convert::Infallible,
    panic::PanicInfo,
};
use cortex_m::peripheral::syst::SystClkSource;
use cortex_m_rt::{ExceptionFrame, entry};
use embedded_graphics::prelude::*;
use embedded_hal::{digital::OutputPin, spi::SpiDevice};
use embedded_hal_bus::spi::ExclusiveDevice;
use mipidsi::{Builder, interface::SpiInterface, models, options::ColorInversion};
use rp2040_hal::{
    Clock, Spi, Timer, Watchdog,
    dma::{DMAExt, SingleChannel, WriteTarget, single_buffer},
    fugit::HertzU32,
    multicore::{self, Stack},
    pac::{self, SYST},
    spi::SpiDevice as HalSpiDevice,
};
use rtt_target::{rprintln, rtt_init_print};

#[unsafe(link_section = ".boot_loader")]
#[used]
pub static BOOT_LOADER: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

const fn KB(num: u32) -> u32 {
    num << 10
}

const fn MB(num: u32) -> u32 {
    num << 20
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

const ROM_START: u32 = 0x0800_0000;
const ROM_LEN: u32 = MB(20);

static mut NEW_REG: DisplayRegisters = DisplayRegisters::zeroed();
static mut NEW_PALETTE: Palette = Palette::zeroed();
static mut NEW_VRAM: VRAM = VRAM::zeroed();
static mut NEW_OAM: OAM = OAM::zeroed();

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
        let new_reg_start = unsafe { &NEW_REG as *const DisplayRegisters as u32 };
        Some(gba_addr + new_reg_start - REG_START)
    } else if gba_addr >= PALETTE_START && gba_addr < PALETTE_START + PALETTE_LEN {
        let new_palette_start = unsafe { &NEW_PALETTE as *const Palette as u32 };
        Some(gba_addr + new_palette_start - PALETTE_START)
    } else if gba_addr >= VRAM_START && gba_addr < VRAM_START + VRAM_LEN {
        let new_vram_start = unsafe { &NEW_VRAM as *const VRAM as u32 };
        Some(gba_addr + new_vram_start - VRAM_START)
    } else if gba_addr >= OAM_START && gba_addr < OAM_START + OAM_LEN {
        let new_oam_start = unsafe { &NEW_OAM as *const OAM as u32 };
        Some(gba_addr + new_oam_start - OAM_START)
    } else if gba_addr >= ROM_START && gba_addr < ROM_START + ROM_LEN {
        let new_rom_start = ROM_CODE.as_ptr() as u32;
        Some(gba_addr + new_rom_start - ROM_START)
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

    if let Some(pc) = fix_addr(frame.pc()) {
        // if we get an invalid PC assume from BX or BLX
        // bx and blx can use all 16 regisers, r8-r15 are not handled yet
        unsafe {
            frame.set_pc(pc);
        }
        return;
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
            4 => other_regs.r4 = new_base_addr,
            5 => other_regs.r5 = new_base_addr,
            6 => other_regs.r6 = new_base_addr,
            7 => other_regs.r7 = new_base_addr,
            _ => unreachable!(),
        }
    }
}

// The display is the only device on the SPI bus, so its CS pin is held low for good and the
// SPI device used by mipidsi gets this pin that does nothing instead
struct NoCs;

impl embedded_hal::digital::ErrorType for NoCs {
    type Error = Infallible;
}

impl OutputPin for NoCs {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

// The SPI1 TX FIFO as a DMA destination. The Spi itself stays inside the SPI device, which is
// still used for the occasional blocking command while no DMA is running.
struct Spi1Tx;

// Safety: DMA only ever writes to the SPI1 TX FIFO through this
unsafe impl WriteTarget for Spi1Tx {
    type TransmittedWord = u8;

    fn tx_treq() -> Option<u8> {
        Some(<pac::SPI1 as HalSpiDevice>::tx_dreq())
    }

    fn tx_address_count(&mut self) -> (u32, u32) {
        let spi = unsafe { &*pac::SPI1::PTR };
        (spi.sspdr().as_ptr() as u32, u32::MAX)
    }

    fn tx_increment(&self) -> bool {
        false
    }
}

// Wait until everything queued on SPI1 has been sent, then throw away what was received
fn spi1_flush() {
    let spi = unsafe { &*pac::SPI1::PTR };
    while spi.sspsr().read().bsy().bit_is_set() {}
    while spi.sspsr().read().rne().bit_is_set() {
        let _ = spi.sspdr().read();
    }
}

// ST7789 commands
const CASET: u8 = 0x2A;
const RASET: u8 = 0x2B;
const RAMWR: u8 = 0x2C;

fn send_command<SPI: SpiDevice, DC: OutputPin>(spi: &mut SPI, dc: &mut DC, command: u8, args: &[u8]) {
    dc.set_low().ok();
    spi.write(&[command]).ok();
    dc.set_high().ok();
    if !args.is_empty() {
        spi.write(args).ok();
    }
}

// GBA BGR555 to the display's RGB565, byte swapped since SPI sends each byte of the line buffer in
// memory order and the display expects the high byte first
fn gba_to_display(colour: u16) -> u16 {
    let red = colour & 0x1F;
    let green = (colour >> 5) & 0x1F;
    let blue = (colour >> 10) & 0x1F;
    ((red << 11) | (green << 6) | blue).swap_bytes()
}

// Scanlines are rendered into one buffer while DMA sends the other to the display
static mut LINE_BUFFERS: [[u16; SCREEN_WIDTH]; 2] = [[0; SCREEN_WIDTH]; 2];

fn video_loop<SPI, DC, CH>(
    mut spi: SPI,
    mut dc: DC,
    dma_channel: CH,
    mut video: Video,
    mut systick: SYST,
) -> !
where
    SPI: SpiDevice,
    DC: OutputPin,
    CH: SingleChannel,
{
    let mut stat = video.registers.disp_status;
    video.set_colour_format(gba_to_display);

    rtt_init_print!();
    // this is using the 1MHz reference clock right now
    systick.enable_counter();
    // SysTick is a 24-bit down counter; invert it to count up and shift so it wraps at 2^32
    profile::set_timer(|| (!SYST::get_current()) << 8);
    const CYCLES_PER_FRAME: u32 = 1_000_000 / 60;
    let elapsed = |start: u32| start.wrapping_sub(SYST::get_current()) & 0x00FF_FFFF;

    // every frame covers the whole screen, so the window only needs setting once
    let last_x = (SCREEN_WIDTH - 1) as u16;
    let last_y = (SCREEN_HEIGHT - 1) as u16;
    send_command(&mut spi, &mut dc, CASET, &[0, 0, (last_x >> 8) as u8, last_x as u8]);
    send_command(&mut spi, &mut dc, RASET, &[0, 0, (last_y >> 8) as u8, last_y as u8]);

    let mut idle = Some((dma_channel, Spi1Tx));

    loop {
        let render_start = SYST::get_current();
        // time spent only rendering, excluding the display transfer
        let mut scanline_total: u32 = 0;
        let mut scanline_max: u32 = 0;
        // time spent waiting for DMA to finish sending the previous line
        let mut dma_wait: u32 = 0;

        let prep_start = SYST::get_current();
        video.begin_frame();
        scanline_total += elapsed(prep_start);

        // the display's write position goes back to the top left of the window
        send_command(&mut spi, &mut dc, RAMWR, &[]);

        let mut sending = None;
        for y in 0..SCREEN_HEIGHT {
            // the other buffer is the one DMA may still be reading
            let buffer = unsafe { &mut LINE_BUFFERS[y % 2] };

            let scanline_start = SYST::get_current();
            video.render_scanline_into(y, buffer);
            let scanline_time = elapsed(scanline_start);
            scanline_total += scanline_time;
            scanline_max = scanline_max.max(scanline_time);

            let wait_start = SYST::get_current();
            let (channel, tx) = match sending.take() {
                Some(transfer) => {
                    let (channel, _, tx) = single_buffer::Transfer::wait(transfer);
                    (channel, tx)
                }
                None => idle.take().unwrap(),
            };
            dma_wait += elapsed(wait_start);

            let bytes: &'static [u8] = unsafe {
                core::slice::from_raw_parts(buffer.as_ptr() as *const u8, SCREEN_WIDTH * 2)
            };
            sending = Some(single_buffer::Config::new(channel, bytes, tx).start());
        }

        let wait_start = SYST::get_current();
        let (channel, _, tx) = sending.take().unwrap().wait();
        idle = Some((channel, tx));
        spi1_flush();
        dma_wait += elapsed(wait_start);

        stat.set_vblank_flag(true);
        video.registers.disp_status = stat;

        let render_end = SYST::get_current();
        let render_time = render_start.wrapping_sub(render_end) & 0x00FF_FFFF;
        rprintln!("frame took {} us, {} us of it waiting for DMA", render_time, dma_wait);
        rprintln!(
            "render only (begin_frame + scanlines): {} us total, {} us avg/line, {} us max line",
            scanline_total,
            scanline_total / SCREEN_HEIGHT as u32,
            scanline_max
        );
        let phases = profile::take();
        for (name, time) in profile::PHASE_NAMES.iter().zip(phases) {
            let us = time >> 8;
            rprintln!("  {:>10}: {:>6} us ({}%)", name, us, us * 100 / scanline_total.max(1));
        }

        if render_time >= CYCLES_PER_FRAME {
            rprintln!("rendering took too long! cannot delay")
        } else {
            let vblank_end = render_start - CYCLES_PER_FRAME;
            let delay = render_end - vblank_end;
            rprintln!("delaying for {}", delay);
        }

        stat.set_vblank_flag(false);
        video.registers.disp_status = stat;
    }
}

static mut CORE1_STACK: Stack<4096> = Stack::new();

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let core = cortex_m::Peripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);

    let mut systick = core.SYST;
    systick.disable_counter(); // Ensure it is stopped during config
    systick.set_reload(0x00FF_FFFF); // Set to max 24-bit value (16,777,215)
    systick.clear_current(); // Clear current value to force a reload
    systick.set_clock_source(SystClkSource::Core); // Use the RP2040 CPU clock (typically 125MHz)

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

    let mut sio = rp2040_hal::Sio::new(pac.SIO);
    let pins = rp2040_hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let spi_sclk = pins.gpio10.into_function::<rp2040_hal::gpio::FunctionSpi>();
    let spi_mosi = pins.gpio11.into_function::<rp2040_hal::gpio::FunctionSpi>();
    let spi_miso = pins.gpio12.into_function::<rp2040_hal::gpio::FunctionSpi>();
    let mut spi_cs = pins.gpio9.into_push_pull_output();
    spi_cs.set_low().unwrap();
    let dc_pin = pins.gpio8.into_push_pull_output();
    let reset_pin = pins.gpio15.into_push_pull_output();
    let mut bk_light_pin = pins.gpio13.into_push_pull_output();

    bk_light_pin.set_high();

    let spi_bus = Spi::<_, _, _, 8>::new(pac.SPI1, (spi_mosi, spi_miso, spi_sclk)).init(
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
        HertzU32::Hz(62_500_000),  // clk_peri / 2, the fastest the SPI divider allows
        // mode 3 since in mode 0 the RP2040 SPI leaves a gap between every byte
        embedded_hal::spi::MODE_3,
    );

    let spi_device = ExclusiveDevice::new(spi_bus, NoCs, timer).unwrap();

    static mut buffer: [u8; 512] = [0; 512];
    let di = unsafe { SpiInterface::new(spi_device, dc_pin, &mut buffer) };
    let mut display = Builder::new(models::ST7789, di)
        .reset_pin(reset_pin)
        .display_size(SCREEN_WIDTH as u16, SCREEN_HEIGHT as u16)
        .invert_colors(ColorInversion::Inverted)
        .init(&mut timer)
        .unwrap();

    // force the vblank to set for now
    unsafe {
        let mut disp_status = NEW_REG.disp_status;
        disp_status.set_vblank_flag(true);
        NEW_REG.disp_status = disp_status;
    }

    // let disp_status = unsafe { NEW_REG.disp_status };

    let video = unsafe {
        Video::new(&mut NEW_REG, &mut NEW_PALETTE, &mut NEW_VRAM, &mut NEW_OAM)
    };

    display.clear(RgbColor::BLACK);

    // mipidsi is only used to set the display up, after that scanlines go out through DMA
    let (di, _, _) = display.release();
    let (spi_device, dc_pin) = di.release();
    let dma = pac.DMA.split(&mut pac.RESETS);

    let mut mc = multicore::Multicore::new(&mut pac.PSM, &mut pac.PPB, &mut sio.fifo);
    let cores = mc.cores();
    let core1 = &mut cores[1];

    unsafe {
        core1
            .spawn(CORE1_STACK.take().unwrap(), move || {
                video_loop(spi_device, dc_pin, dma.ch0, video, systick);
            })
            .unwrap();
    };

    let rom_addr = ROM_CODE.as_ptr() as u32;
    let thumb_start = rom_addr + 0xdc | 1; // last bit needs to be 1 for thumb mode
    unsafe { asm!("bx {}", in(reg) thumb_start) };

    loop {}
}
