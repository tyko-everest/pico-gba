#![no_std]
#![no_main]
#![allow(static_mut_refs)]
mod controls;

use common::{profile, registers::*, video::*};
use controls::{
    BUTTONS, DPAD_CENTRE, GLYPH_HEIGHT, GLYPH_WIDTH, Glyph, KEYS_RELEASED, PANEL, Rect, Touch,
};
use core::{
    arch::{asm, naked_asm},
    convert::Infallible,
    panic::PanicInfo,
};
use cortex_m::peripheral::syst::SystClkSource;
use cortex_m_rt::{ExceptionFrame, entry};
use embedded_graphics::prelude::*;
use embedded_hal::{
    digital::{InputPin, OutputPin},
    spi::SpiDevice,
};
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

// All 1K of the GBA's IO registers. Only the display registers are emulated, the rest is plain
// memory for the game apart from KEYINPUT, which the touch controls write.
#[repr(C, align(4))]
struct IoRegisters {
    display: DisplayRegisters,
    rest: [u8; REG_LEN as usize - size_of::<DisplayRegisters>()],
}

const KEYINPUT_OFFSET: usize = 0x130;

static mut NEW_REG: IoRegisters = IoRegisters {
    display: DisplayRegisters::zeroed(),
    rest: [0; REG_LEN as usize - size_of::<DisplayRegisters>()],
};
fn keyinput_register() -> *mut u16 {
    unsafe { (&raw mut NEW_REG as *mut u8).add(KEYINPUT_OFFSET) as *mut u16 }
}

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
        let new_reg_start = unsafe { &NEW_REG as *const IoRegisters as u32 };
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

// GBA display timing: the clock is 2^24 Hz, every line is 1232 cycles of which the first 1006
// draw pixels and the rest are HBlank, and a frame is 160 visible lines then 68 lines of VBlank
const LINE_CYCLES: u32 = 1232;
const HBLANK_START_CYCLES: u32 = 1006;
const LINES_PER_FRAME: usize = 228;

// GBA cycles to microseconds, exact to within a quarter of a microsecond for anything up to
// a whole frame without overflowing a u32
const fn gba_cycles_to_us(cycles: u32) -> u32 {
    ((cycles >> 2) * 15_625) >> 16
}

const FRAME_US: u32 = gba_cycles_to_us(LINE_CYCLES * LINES_PER_FRAME as u32);

// DISPSTAT flags, which only the hardware writes
const DISPSTAT_VBLANK: u16 = 1 << 0;
const DISPSTAT_HBLANK: u16 = 1 << 1;
const DISPSTAT_VCOUNT: u16 = 1 << 2;

// Update the DISPSTAT flags for this point in the frame. The game may write the other bits
// (interrupt enables and the V-count setting) at any time so only the flags are replaced.
fn set_display_status(disp_status: *mut u16, line: usize, hblank: bool) {
    unsafe {
        let status = core::ptr::read_volatile(disp_status);
        // the VBlank flag is set on lines 160-226, but not on the last line
        let mut flags = 0;
        if line >= SCREEN_HEIGHT && line < LINES_PER_FRAME - 1 {
            flags |= DISPSTAT_VBLANK;
        }
        if hblank {
            flags |= DISPSTAT_HBLANK;
        }
        if line as u16 == status >> 8 {
            flags |= DISPSTAT_VCOUNT;
        }
        core::ptr::write_volatile(disp_status, (status & !0b111) | flags);
    }
}

// Start sending bytes to the display with DMA. Only one transfer can run at a time, the channel
// comes back when it is waited on.
fn dma_send<CH: SingleChannel>(
    idle: &mut Option<(CH, Spi1Tx)>,
    bytes: &'static [u8],
) -> single_buffer::Transfer<CH, &'static [u8], Spi1Tx> {
    let (channel, tx) = idle.take().unwrap();
    single_buffer::Config::new(channel, bytes, tx).start()
}

fn dma_wait<CH: SingleChannel>(
    idle: &mut Option<(CH, Spi1Tx)>,
    transfer: single_buffer::Transfer<CH, &'static [u8], Spi1Tx>,
) {
    let (channel, _, tx) = transfer.wait();
    *idle = Some((channel, tx));
}

fn set_window<SPI: SpiDevice, DC: OutputPin>(spi: &mut SPI, dc: &mut DC, rect: Rect) {
    let last_x = rect.x + rect.width - 1;
    let last_y = rect.y + rect.height - 1;
    let [x0, x1, y0, y1] = [rect.x, last_x, rect.y, last_y].map(u16::to_be_bytes);
    send_command(spi, dc, CASET, &[x0[0], x0[1], x1[0], x1[1]]);
    send_command(spi, dc, RASET, &[y0[0], y0[1], y1[0], y1[1]]);
    send_command(spi, dc, RAMWR, &[]);
}

// Fill a rectangle of the display with one colour. SPI1 and the DMA channel must be idle.
fn fill_rect<SPI: SpiDevice, DC: OutputPin, CH: SingleChannel>(
    spi: &mut SPI,
    dc: &mut DC,
    idle: &mut Option<(CH, Spi1Tx)>,
    rect: Rect,
    colour: u16,
) {
    set_window(spi, dc, rect);
    let row = unsafe { &mut LINE_BUFFERS[0][..rect.width as usize] };
    row.fill(colour);
    let bytes: &'static [u8] =
        unsafe { core::slice::from_raw_parts(row.as_ptr() as *const u8, row.len() * 2) };
    for _ in 0..rect.height {
        let transfer = dma_send(idle, bytes);
        dma_wait(idle, transfer);
    }
    spi1_flush();
}

// colours of the touch controls, as GBA colours
const PANEL_COLOUR: u16 = 0x0842;
const BUTTON_COLOUR: u16 = 0x318C;
const PRESSED_COLOUR: u16 = 0x6B5A;
const LABEL_COLOUR: u16 = 0x7FFF;
// pressed buttons are highlighted inside a border of this width
const BUTTON_BORDER: u16 = 4;
// each pixel of a label's letter is drawn as a square this many pixels wide
const LABEL_SCALE: u16 = 4;

// Draw a letter scaled up, with its top left corner at x, y. SPI1 and the DMA channel must be idle.
fn draw_glyph<SPI: SpiDevice, DC: OutputPin, CH: SingleChannel>(
    spi: &mut SPI,
    dc: &mut DC,
    idle: &mut Option<(CH, Spi1Tx)>,
    x: u16,
    y: u16,
    glyph: &Glyph,
    foreground: u16,
    background: u16,
) {
    let width = GLYPH_WIDTH * LABEL_SCALE;
    set_window(spi, dc, Rect::new(x, y, width, GLYPH_HEIGHT * LABEL_SCALE));
    let row = unsafe { &mut LINE_BUFFERS[0][..width as usize] };
    let bytes: &'static [u8] =
        unsafe { core::slice::from_raw_parts(row.as_ptr() as *const u8, row.len() * 2) };
    for &bits in glyph {
        for (i, pixel) in row.iter_mut().enumerate() {
            let column = i as u16 / LABEL_SCALE;
            let set = bits & (1 << (GLYPH_WIDTH - 1 - column)) != 0;
            *pixel = if set { foreground } else { background };
        }
        // every row of the letter is repeated to scale it up vertically
        for _ in 0..LABEL_SCALE {
            let transfer = dma_send(idle, bytes);
            dma_wait(idle, transfer);
        }
    }
    spi1_flush();
}

fn draw_button<SPI: SpiDevice, DC: OutputPin, CH: SingleChannel>(
    spi: &mut SPI,
    dc: &mut DC,
    idle: &mut Option<(CH, Spi1Tx)>,
    rect: Rect,
    key: u16,
    pressed: bool,
) {
    let colour = gba_to_display(if pressed { PRESSED_COLOUR } else { BUTTON_COLOUR });
    fill_rect(spi, dc, idle, rect.inset(BUTTON_BORDER), colour);
    if let Some(glyph) = controls::label(key) {
        // centred on the button
        let x = rect.x + (rect.width - GLYPH_WIDTH * LABEL_SCALE) / 2;
        let y = rect.y + (rect.height - GLYPH_HEIGHT * LABEL_SCALE) / 2;
        draw_glyph(spi, dc, idle, x, y, glyph, gba_to_display(LABEL_COLOUR), colour);
    }
}

fn draw_controls<SPI: SpiDevice, DC: OutputPin, CH: SingleChannel>(
    spi: &mut SPI,
    dc: &mut DC,
    idle: &mut Option<(CH, Spi1Tx)>,
) {
    fill_rect(spi, dc, idle, PANEL, gba_to_display(PANEL_COLOUR));
    fill_rect(spi, dc, idle, DPAD_CENTRE, gba_to_display(BUTTON_COLOUR));
    for (rect, key) in BUTTONS {
        fill_rect(spi, dc, idle, rect, gba_to_display(BUTTON_COLOUR));
        draw_button(spi, dc, idle, rect, key, false);
    }
}

fn video_loop<SPI, DC, CH, TpCs, TpIrq, LcdCs>(
    mut spi: SPI,
    mut dc: DC,
    dma_channel: CH,
    mut video: Video,
    mut systick: SYST,
    mut touch: Touch<TpCs, TpIrq, LcdCs>,
    keyinput: *mut u16,
) -> !
where
    SPI: SpiDevice,
    DC: OutputPin,
    CH: SingleChannel,
    TpCs: OutputPin,
    TpIrq: InputPin,
    LcdCs: OutputPin,
{
    video.set_colour_format(gba_to_display);

    // the game polls these from the other core, so they are written with single 16 bit stores
    let disp_status = core::ptr::addr_of_mut!(video.registers.disp_status) as *mut u16;
    let vert_counter = core::ptr::addr_of_mut!(video.registers.vert_counter) as *mut u16;
    assert!(disp_status as usize % 2 == 0 && vert_counter as usize % 2 == 0);

    rtt_init_print!();
    // this is using the 1MHz reference clock right now
    systick.enable_counter();
    // SysTick is a 24-bit down counter; invert it to count up and shift so it wraps at 2^32
    profile::set_timer(|| (!SYST::get_current()) << 8);
    let elapsed = |start: u32| start.wrapping_sub(SYST::get_current()) & 0x00FF_FFFF;

    let mut idle = Some((dma_channel, Spi1Tx));
    let mut sending = None;

    draw_controls(&mut spi, &mut dc, &mut idle);
    let mut keys_held: u16 = 0;
    let mut touch_frames: u32 = 0;
    // longest time spent reading touch and redrawing buttons in one VBlank
    let mut input_max: u32 = 0;

    // stats printed once a second
    const REPORT_FRAMES: u32 = 60;
    let mut frames: u32 = 0;
    let mut render_total: u32 = 0;
    let mut render_max: u32 = 0;
    // how late the last visible line finished sending, compared to when VBlank should start
    let mut vblank_late_max: u32 = 0;
    let mut late_frames: u32 = 0;
    let mut report_start = SYST::get_current();

    let mut frame_start = SYST::get_current();
    loop {
        let mut render_time: u32 = 0;

        for line in 0..LINES_PER_FRAME {
            // wait for the line to start, a line that is already late starts right away
            let line_start = gba_cycles_to_us(line as u32 * LINE_CYCLES);
            while elapsed(frame_start) < line_start {}

            unsafe { core::ptr::write_volatile(vert_counter, line as u16) };
            set_display_status(disp_status, line, false);

            if line == 0 {
                let prep_start = SYST::get_current();
                video.begin_frame();
                render_time += elapsed(prep_start);
                // the frame goes in the top half of the display, this also puts the display's
                // write position back at the top left
                let screen = Rect::new(0, 0, SCREEN_WIDTH as u16, SCREEN_HEIGHT as u16);
                set_window(&mut spi, &mut dc, screen);
            }

            if line < SCREEN_HEIGHT {
                // the other buffer is the one DMA may still be reading
                let buffer = unsafe { &mut LINE_BUFFERS[line % 2] };
                let render_start = SYST::get_current();
                video.render_scanline_into(line, buffer);
                render_time += elapsed(render_start);

                let (channel, tx) = match sending.take() {
                    Some(transfer) => {
                        let (channel, _, tx) = single_buffer::Transfer::wait(transfer);
                        (channel, tx)
                    }
                    None => idle.take().unwrap(),
                };
                let bytes: &'static [u8] = unsafe {
                    core::slice::from_raw_parts(buffer.as_ptr() as *const u8, SCREEN_WIDTH * 2)
                };
                sending = Some(single_buffer::Config::new(channel, bytes, tx).start());
            } else if line == SCREEN_HEIGHT {
                // the whole frame has to be on the display before the next RAMWR
                if let Some(transfer) = sending.take() {
                    let (channel, _, tx) = single_buffer::Transfer::wait(transfer);
                    idle = Some((channel, tx));
                }
                spi1_flush();

                // SPI1 is idle until the next frame, so this is when touch is read
                let input_start = SYST::get_current();
                let mut keys = 0;
                if let Some((raw_x, raw_y)) = touch.read_raw() {
                    let (x, y) = controls::raw_to_screen(raw_x, raw_y);
                    keys = controls::keys_at(x, y);
                    // print readings a few times a second to calibrate against
                    if touch_frames % 15 == 0 {
                        rprintln!("touch raw {} {} -> screen {} {}, keys {:#x}", raw_x, raw_y, x, y, keys);
                    }
                    touch_frames += 1;
                } else {
                    touch_frames = 0;
                }
                unsafe { core::ptr::write_volatile(keyinput, KEYS_RELEASED & !keys) };
                if keys != keys_held {
                    for (rect, key) in BUTTONS {
                        if (keys ^ keys_held) & key != 0 {
                            draw_button(&mut spi, &mut dc, &mut idle, rect, key, keys & key != 0);
                        }
                    }
                    keys_held = keys;
                }
                input_max = input_max.max(elapsed(input_start));

                vblank_late_max = vblank_late_max.max(elapsed(frame_start).saturating_sub(line_start));

                frames += 1;
                render_total += render_time;
                render_max = render_max.max(render_time);
                if frames == REPORT_FRAMES {
                    let report_time = elapsed(report_start);
                    report_start = SYST::get_current();
                    rprintln!(
                        "{} frames in {} us ({} us/frame, target {}), {} late",
                        frames,
                        report_time,
                        report_time / frames,
                        FRAME_US,
                        late_frames
                    );
                    rprintln!(
                        "  render avg {} us, max {} us, last line sent up to {} us into VBlank",
                        render_total / frames,
                        render_max,
                        vblank_late_max
                    );
                    rprintln!("  touch and button drawing took up to {} us", input_max);
                    input_max = 0;
                    let phases = profile::take();
                    for (name, time) in profile::PHASE_NAMES.iter().zip(phases) {
                        rprintln!("  {:>12}: {:>6} us/frame", name, (time >> 8) / frames);
                    }
                    frames = 0;
                    render_total = 0;
                    render_max = 0;
                    vblank_late_max = 0;
                    late_frames = 0;
                }
            }

            let hblank_start = gba_cycles_to_us(line as u32 * LINE_CYCLES + HBLANK_START_CYCLES);
            while elapsed(frame_start) < hblank_start {}
            set_display_status(disp_status, line, true);
        }

        // the next frame starts a whole frame after this one did, so lateness does not add up
        let frame_time = elapsed(frame_start);
        if frame_time > FRAME_US {
            late_frames += 1;
        }
        while elapsed(frame_start) < FRAME_US {}
        if frame_time > 2 * FRAME_US {
            // more than a frame behind, give up on catching up
            frame_start = SYST::get_current();
        } else {
            frame_start = frame_start.wrapping_sub(FRAME_US) & 0x00FF_FFFF;
        }
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
    let mut tp_cs = pins.gpio16.into_push_pull_output();
    tp_cs.set_high().unwrap();
    let tp_irq = pins.gpio17.into_pull_up_input();

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
        let mut disp_status = NEW_REG.display.disp_status;
        disp_status.set_vblank_flag(true);
        NEW_REG.display.disp_status = disp_status;
    }

    // no keys are held until the touch controls say otherwise
    unsafe { core::ptr::write_volatile(keyinput_register(), KEYS_RELEASED) };

    let video = unsafe {
        Video::new(&mut NEW_REG.display, &mut NEW_PALETTE, &mut NEW_VRAM, &mut NEW_OAM)
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
                let touch = Touch { tp_cs, tp_irq, lcd_cs: spi_cs };
                video_loop(spi_device, dc_pin, dma.ch0, video, systick, touch, keyinput_register());
            })
            .unwrap();
    };

    let rom_addr = ROM_CODE.as_ptr() as u32;
    let thumb_start = rom_addr + 0xdc | 1; // last bit needs to be 1 for thumb mode
    unsafe { asm!("bx {}", in(reg) thumb_start) };

    loop {}
}
