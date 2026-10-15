#![no_std]
#![no_main]
#![allow(static_mut_refs)]
mod controls;

use common::{
    jit::{Jit, MapEntry, TRAP},
    profile,
    registers::*,
    video::*,
};
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

// Only the start of EWRAM exists, the real 256 KiB does not fit next to everything else. The
// game's heap grows up from the bottom of it.
const EWRAM_START: u32 = 0x0200_0000;
const EWRAM_LEN: u32 = KB(48);

const IWRAM_START: u32 = 0x0300_0000;
const IWRAM_LEN: u32 = KB(32);
const IWRAM_MIRRORS: u32 = MB(16);

// Cartridge save memory. Only a small window of it exists and it starts out blank.
const SRAM_START: u32 = 0x0E00_0000;
const SRAM_LEN: u32 = KB(4);

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
static mut NEW_SRAM: [u8; SRAM_LEN as usize] = [0xFF; SRAM_LEN as usize];
static mut NEW_EWRAM: [u32; EWRAM_LEN as usize / 4] = [0; EWRAM_LEN as usize / 4];
static mut NEW_IWRAM: [u32; IWRAM_LEN as usize / 4] = [0; IWRAM_LEN as usize / 4];

// part of the rom loaded in
// The ROM runs from where it is stored, so it has to keep the GBA's word alignment or every
// pc relative load in it reads the wrong word
#[repr(C, align(4))]
struct Aligned<T: ?Sized>(T);
static ROM_ALIGNED: &Aligned<[u8]> = &Aligned(*include_bytes!("Google-Dino-Advance.gba"));
static ROM_CODE: &[u8] = &ROM_ALIGNED.0;

// Translated ARM code. The game's ARM mode code cannot run on the Cortex-M0+, the fault handler
// translates it to Thumb the first time it is reached and runs it from this cache.
const JIT_CODE_HW: usize = 12 * 1024;
const JIT_MAP_LEN: usize = 1024;

#[repr(C, align(4))]
struct JitCode([u16; JIT_CODE_HW]);

static mut JIT_CODE: JitCode = JitCode([0; JIT_CODE_HW]);
static mut JIT_MAP: [MapEntry; JIT_MAP_LEN] = [Jit::empty_map_entry(); JIT_MAP_LEN];
static mut JIT: Option<Jit<'static>> = None;

fn jit_init() {
    unsafe {
        let base = JIT_CODE.0.as_ptr() as u32;
        JIT = Some(Jit::new(&mut JIT_CODE.0, base, &mut JIT_MAP));
    }
}

// the GBA address of something stored in the ROM or IWRAM copies, for code that runs in place
fn native_to_gba(native: u32) -> Option<u32> {
    let rom = ROM_CODE.as_ptr() as u32;
    let iwram = unsafe { &raw const NEW_IWRAM as u32 };
    if native >= rom && native < rom + ROM_CODE.len() as u32 {
        Some(native - rom + ROM_START)
    } else if native >= iwram && native < iwram + IWRAM_LEN {
        Some(native - iwram + IWRAM_START)
    } else {
        None
    }
}

// Reads a word of GBA memory for the translator
fn gba_read_word(addr: u32) -> Option<u32> {
    fix_addr(addr).map(|native| unsafe { core::ptr::read_volatile(native as *const u32) })
}

// Handles the faults caused by translated code: unresolved exits and jumps into ARM code.
// Returns true when the fault was dealt with.
fn jit_fault(frame: &mut ExceptionFrame) -> bool {
    let Some(jit) = (unsafe { JIT.as_mut() }) else {
        return false;
    };
    let pc = frame.pc();
    const THUMB_BIT: u32 = 1 << 24;
    let target = if jit.contains(pc) && jit.halfword(pc) == TRAP {
        jit.resolve_slot(pc, &mut gba_read_word)
    } else if native_to_gba(pc & !1).is_some() && unsafe { *((pc & !1) as *const u16) } == 0x4778 {
        // bx pc, the Thumb to ARM veneer. Cortex-M0+ does not allow it, the ARM code follows
        // the next instruction.
        match native_to_gba((pc + 4) & !3) {
            Some(gba) => jit.translate(gba, &mut gba_read_word),
            None => return false,
        }
    } else if frame.xpsr() & THUMB_BIT == 0 {
        // a bx to an even address, which is ARM code. ROM code runs from where it is stored, so
        // the address may be in the flash copy of the ROM.
        let gba = match native_to_gba(pc) {
            Some(gba) => gba,
            None if fix_addr(pc).is_some() => pc,
            None => return false,
        };
        jit.translate(gba, &mut gba_read_word)
    } else {
        return false;
    };
    match target {
        Ok(thumb) => unsafe {
            frame.set_pc(thumb & !1);
            // the stacked status register must be in Thumb state for the exception return
            let raw = frame as *mut ExceptionFrame as *mut u32;
            *raw.add(7) |= THUMB_BIT;
            true
        },
        Err(e) => {
            rprintln!("cannot translate ARM code at 0x{:x}: {:?}", pc, e);
            panic!()
        }
    }
}

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

// EWRAM is 256 KiB that repeats up to IWRAM, far more than there is RAM for. Games touch only part
// of it, so it is handed out in small pages the first time a page is used.
const EWRAM_PAGE: u32 = 512;
const EWRAM_MIRROR: u32 = KB(256);

static mut EWRAM_TABLE: [u8; (EWRAM_MIRROR / EWRAM_PAGE) as usize] =
    [0xFF; (EWRAM_MIRROR / EWRAM_PAGE) as usize];
static mut EWRAM_PAGES_USED: u32 = 0;

fn ewram_native(gba_addr: u32) -> Option<u32> {
    unsafe {
        let offset = (gba_addr - EWRAM_START) & (EWRAM_MIRROR - 1);
        let page = (offset / EWRAM_PAGE) as usize;
        if EWRAM_TABLE[page] == 0xFF {
            if EWRAM_PAGES_USED * EWRAM_PAGE >= EWRAM_LEN {
                return None;
            }
            EWRAM_TABLE[page] = EWRAM_PAGES_USED as u8;
            EWRAM_PAGES_USED += 1;
        }
        let pool = &raw const NEW_EWRAM as u32;
        Some(pool + EWRAM_TABLE[page] as u32 * EWRAM_PAGE + offset % EWRAM_PAGE)
    }
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
    } else if gba_addr >= SRAM_START && gba_addr < SRAM_START + SRAM_LEN {
        let new_sram_start = unsafe { &raw const NEW_SRAM as u32 };
        Some(gba_addr + new_sram_start - SRAM_START)
    } else if gba_addr >= EWRAM_START && gba_addr < IWRAM_START {
        ewram_native(gba_addr)
    } else if gba_addr >= IWRAM_START && gba_addr < IWRAM_START + IWRAM_MIRRORS {
        // the 32 KiB of IWRAM repeats all the way up to the IO registers, games use the mirror
        // at the top for the interrupt vector
        let new_iwram_start = unsafe { &raw const NEW_IWRAM as u32 };
        Some(new_iwram_start + ((gba_addr - IWRAM_START) & (IWRAM_LEN - 1)))
    } else if gba_addr >= ROM_START && gba_addr < ROM_START + ROM_LEN {
        let new_rom_start = ROM_CODE.as_ptr() as u32;
        Some(gba_addr + new_rom_start - ROM_START)
    } else {
        None
    }
}

// GBA BIOS calls are SWIs, which the RP2040 runs natively as SVC. The BIOS is not present, so
// the SVCall exception implements the calls the game uses.
#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn SVCall() {
    // r0 is the stacked exception frame. r4 is pushed only to keep the stack 8 byte aligned.
    naked_asm!(
        "mov r0, sp",
        "push {{r4, lr}}",
        "bl {func}",
        "pop {{r4, pc}}",
        func = sym svc_call
    );
}

// Registers hold GBA addresses unless a faulting load or store already swapped in the native
// address, so anything that is not a GBA address is assumed to be native already
fn native_addr(addr: u32) -> u32 {
    fix_addr(addr).unwrap_or(addr)
}

// SWI 0x0B CpuSet and SWI 0x0C CpuFastSet, a memcpy/memset in 16 or 32 bit units.
// r0 = source, r1 = destination, r2 = bits 0-20 unit count, bit 24 fill, bit 26 32 bit (CpuSet).
// CpuFastSet is always 32 bit and rounds the count up to a multiple of 8.
fn cpu_set(frame: &ExceptionFrame, fast: bool) {
    let control = frame.r2();
    let mut count = (control & 0x1F_FFFF) as usize;
    let fill = control & (1 << 24) != 0;
    let word = fast || control & (1 << 26) != 0;
    if fast {
        count = (count + 7) & !7;
    }
    if count == 0 {
        return;
    }

    let align = if word { !3 } else { !1 };
    let (src, dst) = (frame.r0() & align, frame.r1() & align);
    let unit = if word { 4 } else { 2 };

    // Every unit is translated on its own, the memory behind an address range is not contiguous
    // (EWRAM is paged). Volatile so the loop is not turned into a memcpy call.
    unsafe {
        let first = native_addr(src);
        let value_at = |i: usize| {
            let from = native_addr(src + if fill { 0 } else { i as u32 * unit });
            let from = if fill && i == 0 { first } else { from };
            if word {
                (from as *const u32).read_volatile()
            } else {
                (from as *const u16).read_volatile() as u32
            }
        };
        for i in 0..count {
            let value = value_at(i);
            let to = native_addr(dst + i as u32 * unit);
            if word {
                (to as *mut u32).write_volatile(value);
            } else {
                (to as *mut u16).write_volatile(value as u16);
            }
        }
    }
}

// pc, call number and r0-r2 of the last BIOS call, for debugging
static mut LAST_SVC: [u32; 5] = [0; 5];
static mut SVC_COUNTS: [u32; 16] = [0; 16];
static mut FAULT_COUNT: u32 = 0;

#[unsafe(no_mangle)]
extern "C" fn svc_call(frame: &mut ExceptionFrame) {
    // the stacked pc is the instruction after the svc, whose low byte is the BIOS call number
    let instr = unsafe { *((frame.pc() - 2) as *const u16) };
    unsafe {
        SVC_COUNTS[(instr & 0xF) as usize] += 1;
        LAST_SVC = [
            frame.pc(),
            instr as u32 & 0xFF,
            frame.r0(),
            frame.r1(),
            frame.r2(),
        ];
    }
    match instr & 0xFF {
        // Div: r0 = quotient, r1 = remainder, r3 = abs(quotient)
        0x06 => {
            let (num, den) = (frame.r0() as i32, frame.r1() as i32);
            // the real BIOS hangs on a zero divisor, here the registers are left alone
            if den != 0 {
                let quotient = num.wrapping_div(den);
                let remainder = num.wrapping_rem(den);
                unsafe {
                    frame.set_r0(quotient as u32);
                    frame.set_r1(remainder as u32);
                    frame.set_r3(quotient.unsigned_abs());
                }
            }
        }
        0x0B => cpu_set(frame, false),
        0x0C => cpu_set(frame, true),
        // Halt: nothing to save power on
        0x02 => {}
        // IntrWait(discard in r0, flags in r1)
        0x04 => intr_wait(frame, frame.r0() != 0, frame.r1() as u16),
        // VBlankIntrWait
        0x05 => intr_wait(frame, true, 1),
        // nothing else handled currently
        n => {
            rprintln!("unhandled BIOS call 0x{:x} at 0x{:x}", n, frame.pc());
            panic!()
        }
    }
    // this is a good place to let an interrupt in, a game waiting in a loop of calls would
    // otherwise keep the exception tail-chaining
    try_inject(frame);
}

#[unsafe(no_mangle)]
extern "C" fn hard_fault(frame: &mut ExceptionFrame, other_regs: &mut Reg4_7) {
    static mut HF_COUNT: usize = 0;
    unsafe { FAULT_COUNT += 1 };
    let hf_count: usize;
    unsafe {
        HF_COUNT += 1;
        hf_count = HF_COUNT;
    }

    if jit_fault(frame) {
        return;
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
        || instr >> 9 == 0b0101111  // LDRSH reg
        || instr >> 11 == 0b11000   // STMIA
        || instr >> 11 == 0b11001
        // LDMIA
    ) {
        // nothing else handled currently
        rprintln!("cannot handle opcode 0x{:x}", instr);
        panic!()
    }

    let get = |frame: &ExceptionFrame, other: &Reg4_7, reg: u16| -> u32 {
        match reg {
            0 => frame.r0(),
            1 => frame.r1(),
            2 => frame.r2(),
            3 => frame.r3(),
            4 => other.r4,
            5 => other.r5,
            6 => other.r6,
            _ => other.r7,
        }
    };
    let set = |frame: &mut ExceptionFrame, other: &mut Reg4_7, reg: u16, value: u32| unsafe {
        match reg {
            0 => frame.set_r0(value),
            1 => frame.set_r1(value),
            2 => frame.set_r2(value),
            3 => frame.set_r3(value),
            4 => other.r4 = value,
            5 => other.r5 = value,
            6 => other.r6 = value,
            _ => other.r7 = value,
        }
    };
    let fail = |frame: &ExceptionFrame, other: &Reg4_7, address: u32| -> ! {
        rprintln!("last svc {:x?}", unsafe { LAST_SVC });
        rprintln!(
            "r0-r7 {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x}",
            frame.r0(),
            frame.r1(),
            frame.r2(),
            frame.r3(),
            { other.r4 },
            { other.r5 },
            { other.r6 },
            { other.r7 }
        );
        rprintln!(
            "cannot handle address 0x{:x} (instr 0x{:x}, pc 0x{:x}, lr 0x{:x}, rom offset 0x{:x}, rn 0x{:x}, rm 0x{:x})",
            address,
            instr,
            frame.pc(),
            frame.lr(),
            diff,
            get(frame, other, (instr >> 3) & 7),
            get(frame, other, (instr >> 6) & 7)
        );
        panic!()
    };

    // The access is done here and the registers keep the GBA addresses: code compares pointers
    // it loaded from memory against ones it computed, a loop that copies up to an end pointer
    // never ends when the two are different kinds. That goes for ROM pointers too.
    let block = instr >> 12 == 0b1100;
    let reg_offset = instr >> 12 == 0b0101;
    let rn = if block {
        (instr >> 8) & 7
    } else {
        (instr >> 3) & 7
    };
    let rm = (instr >> 6) & 7;
    let base_value = get(frame, other_regs, rn);

    if block {
        let load = instr & (1 << 11) != 0;
        let list = instr & 0xFF;
        let mut address = base_value;
        for r in 0..8u16 {
            if list & (1 << r) == 0 {
                continue;
            }
            let Some(native) = fix_addr(address) else {
                fail(frame, other_regs, address)
            };
            if load {
                let value = unsafe { read_native(native, 4) };
                set(frame, other_regs, r, value);
            } else {
                let value = get(frame, other_regs, r);
                unsafe { write_native(native, 4, value) };
            }
            address = address.wrapping_add(4);
        }
        // a loaded base register keeps the loaded value
        if !(load && list & (1 << rn) != 0) {
            set(frame, other_regs, rn, address);
        }
    } else {
        let rt = instr & 7;
        // size in bytes, whether it loads, whether it sign extends
        let (size, load, signed, address) = if reg_offset {
            let address = get(frame, other_regs, rn).wrapping_add(get(frame, other_regs, rm));
            match (instr >> 9) & 7 {
                0b000 => (4, false, false, address),
                0b001 => (2, false, false, address),
                0b010 => (1, false, false, address),
                0b011 => (1, true, true, address),
                0b100 => (4, true, false, address),
                0b101 => (2, true, false, address),
                0b110 => (1, true, false, address),
                _ => (2, true, true, address),
            }
        } else {
            let imm = ((instr >> 6) & 0x1F) as u32;
            let base = get(frame, other_regs, rn);
            let load = instr & (1 << 11) != 0;
            match instr >> 12 {
                0b0110 => (4, load, false, base.wrapping_add(imm * 4)),
                0b0111 => (1, load, false, base.wrapping_add(imm)),
                _ => (2, load, false, base.wrapping_add(imm * 2)),
            }
        };
        let Some(native) = fix_addr(address) else {
            fail(frame, other_regs, address)
        };
        if load {
            let mut value = unsafe { read_native(native, size) };
            if signed {
                value = match size {
                    1 => value as u8 as i8 as i32 as u32,
                    _ => value as u16 as i16 as i32 as u32,
                };
            }
            set(frame, other_regs, rt, value);
        } else {
            let value = get(frame, other_regs, rt);
            unsafe { write_native(native, size, value) };
        }
    }
    // the instruction is done, continue after it
    unsafe { frame.set_pc(frame.pc() + 2) };
}

// Memory accesses on behalf of the faulting code. They go a byte at a time when unaligned, a
// misaligned access would fault inside the fault handler.
unsafe fn read_native(native: u32, size: u32) -> u32 {
    unsafe {
        if native % size == 0 {
            match size {
                1 => core::ptr::read_volatile(native as *const u8) as u32,
                2 => core::ptr::read_volatile(native as *const u16) as u32,
                _ => core::ptr::read_volatile(native as *const u32),
            }
        } else {
            let mut value = 0;
            for i in 0..size {
                value |= (core::ptr::read_volatile((native + i) as *const u8) as u32) << (8 * i);
            }
            value
        }
    }
}

unsafe fn write_native(native: u32, size: u32, value: u32) {
    unsafe {
        if native % size == 0 {
            match size {
                1 => core::ptr::write_volatile(native as *mut u8, value as u8),
                2 => core::ptr::write_volatile(native as *mut u16, value as u16),
                _ => core::ptr::write_volatile(native as *mut u32, value),
            }
        } else {
            for i in 0..size {
                core::ptr::write_volatile((native + i) as *mut u8, (value >> (8 * i)) as u8);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Interrupts
//
// The display loop on core 1 raises VBlank, which reaches core 0 as a SIO FIFO interrupt. The
// handler there does not run the game's interrupt handler itself, it makes the interrupted code
// call a trampoline instead: the game's handler can be translated ARM code and may fault, and the
// game expects it to run like any other code. The trampoline saves every register, calls the
// handler whose address the game stored at the top of IWRAM, and resumes where it was.

const IME_OFFSET: usize = 0x208;
const IE_OFFSET: usize = 0x200;
const IF_OFFSET: usize = 0x202;
const DISPSTAT_OFFSET: usize = 0x004;
const IRQ_VECTOR_OFFSET: usize = 0x7FFC;

// where the game was at the last few interrupts, for debugging
static mut PC_SAMPLES: [u32; 8] = [0; 8];
static mut PC_SAMPLE_COUNT: usize = 0;
static mut IN_IRQ: bool = false;
static mut IRQ_RETURN_PC: u32 = 0;

fn io_reg(offset: usize) -> *mut u16 {
    unsafe { (&raw mut NEW_REG as *mut u8).add(offset) as *mut u16 }
}

// Called by core 1 when VBlank starts
fn raise_vblank_irq() {
    unsafe {
        if core::ptr::read_volatile(io_reg(DISPSTAT_OFFSET)) & (1 << 3) == 0 {
            return;
        }
        let flags = io_reg(IF_OFFSET);
        core::ptr::write_volatile(flags, core::ptr::read_volatile(flags) | 1);
        (*pac::SIO::ptr()).fifo_wr().write(|w| w.bits(1));
    }
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn SIO_IRQ_PROC0() {
    // r0 is the frame the hardware stacked, the interrupted code runs in thread mode
    naked_asm!(
        "mov r0, sp",
        "push {{r4, lr}}",
        "bl {func}",
        "pop {{r4, pc}}",
        func = sym irq_inject
    );
}

#[unsafe(no_mangle)]
extern "C" fn irq_inject(frame: &mut ExceptionFrame) {
    unsafe {
        let sio = &*pac::SIO::ptr();
        while sio.fifo_st().read().vld().bit() {
            let _ = sio.fifo_rd().read();
        }
        PC_SAMPLES[PC_SAMPLE_COUNT % 8] = frame.pc();
        PC_SAMPLE_COUNT += 1;
    }
    try_inject(frame);
}

// If an enabled interrupt is pending, make the code that frame returns to run the game's handler
// first
fn try_inject(frame: &mut ExceptionFrame) {
    unsafe {
        if IN_IRQ {
            return;
        }
        let enabled = core::ptr::read_volatile(io_reg(IME_OFFSET)) & 1 != 0;
        let pending = core::ptr::read_volatile(io_reg(IE_OFFSET))
            & core::ptr::read_volatile(io_reg(IF_OFFSET));
        if !enabled || pending == 0 || irq_vector() == 0 {
            return;
        }
        IN_IRQ = true;
        // the return has to be a Thumb address
        IRQ_RETURN_PC = frame.pc() | 1;
        frame.set_pc(irq_trampoline as usize as u32 & !1);
    }
}

// The BIOS keeps the flags of the interrupts that happened in the last word but one of IWRAM,
// the game's handler sets them
fn bios_if() -> *mut u16 {
    unsafe { (&raw mut NEW_IWRAM as *mut u8).add(0x7FF8) as *mut u16 }
}

static mut INTR_WAITING: bool = false;

// IntrWait and VBlankIntrWait. The BIOS halts until one of the interrupts in mask has been
// handled. Waiting inside this exception would keep the interrupt away from the game, so the
// call is run again until it is done, the interrupt happens in between.
fn intr_wait(frame: &mut ExceptionFrame, discard: bool, mask: u16) {
    unsafe {
        let flags = bios_if();
        if !INTR_WAITING {
            INTR_WAITING = true;
            core::ptr::write_volatile(io_reg(IME_OFFSET), 1);
            if discard {
                core::ptr::write_volatile(flags, core::ptr::read_volatile(flags) & !mask);
            }
        }
        if core::ptr::read_volatile(flags) & mask != 0 {
            core::ptr::write_volatile(flags, core::ptr::read_volatile(flags) & !mask);
            INTR_WAITING = false;
        } else {
            frame.set_pc(frame.pc() - 2);
        }
    }
}

// the game's interrupt handler, stored in the last word of IWRAM
extern "C" fn irq_vector() -> u32 {
    unsafe {
        core::ptr::read_volatile(
            (&raw const NEW_IWRAM as *const u8).add(IRQ_VECTOR_OFFSET) as *const u32
        )
    }
}

extern "C" fn irq_return_pc() -> u32 {
    unsafe { IRQ_RETURN_PC }
}

// The game's handler has acknowledged what it handled, the flag register is plain memory here so
// the VBlank flag is cleared for it.
extern "C" fn irq_finish() {
    unsafe {
        let flags = io_reg(IF_OFFSET);
        core::ptr::write_volatile(flags, core::ptr::read_volatile(flags) & !1);
        IN_IRQ = false;
    }
}

#[unsafe(naked)]
extern "C" fn irq_trampoline() {
    // Runs in thread mode with every register as the interrupted code left it, only the pc is
    // different. The stack ends up as: r12, flags, lr, r8-r11, r0-r7, return pc.
    naked_asm!(
        "sub sp, #4",
        "push {{r0-r7}}",
        "mov r0, r8",
        "mov r1, r9",
        "mov r2, r10",
        "mov r3, r11",
        "push {{r0-r3}}",
        "mov r0, r12",
        "mrs r1, apsr",
        "mov r2, lr",
        "push {{r0-r2}}",
        "bl {ret_pc}",
        "str r0, [sp, #60]",
        "bl {vector}",
        "mov r1, r0",
        "movs r0, #1",
        "lsls r0, r0, #26",
        "blx r1",
        "bl {finish}",
        "pop {{r0-r2}}",
        "mov r12, r0",
        "msr apsr_nzcvq, r1",
        "mov lr, r2",
        "pop {{r4-r7}}",
        "mov r8, r4",
        "mov r9, r5",
        "mov r10, r6",
        "mov r11, r7",
        "pop {{r0-r7}}",
        "pop {{pc}}",
        ret_pc = sym irq_return_pc,
        vector = sym irq_vector,
        finish = sym irq_finish,
    );
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

fn send_command<SPI: SpiDevice, DC: OutputPin>(
    spi: &mut SPI,
    dc: &mut DC,
    command: u8,
    args: &[u8],
) {
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
    DisplayColour::from(colour).to_rgb565_format().swap_bytes()
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
    let colour = gba_to_display(if pressed {
        PRESSED_COLOUR
    } else {
        BUTTON_COLOUR
    });
    fill_rect(spi, dc, idle, rect.inset(BUTTON_BORDER), colour);
    if let Some(glyph) = controls::label(key) {
        // centred on the button
        let x = rect.x + (rect.width - GLYPH_WIDTH * LABEL_SCALE) / 2;
        let y = rect.y + (rect.height - GLYPH_HEIGHT * LABEL_SCALE) / 2;
        draw_glyph(
            spi,
            dc,
            idle,
            x,
            y,
            glyph,
            gba_to_display(LABEL_COLOUR),
            colour,
        );
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
            if line == SCREEN_HEIGHT {
                raise_vblank_irq();
            }

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
                        rprintln!(
                            "touch raw {} {} -> screen {} {}, keys {:#x}",
                            raw_x,
                            raw_y,
                            x,
                            y,
                            keys
                        );
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

                vblank_late_max =
                    vblank_late_max.max(elapsed(frame_start).saturating_sub(line_start));

                frames += 1;
                render_total += render_time;
                render_max = render_max.max(render_time);
                if frames == REPORT_FRAMES {
                    unsafe {
                        rprintln!(
                            "ewram pages {}, faults {}, svc counts {:?}, jit blocks {} ({} bytes), last svc {:x?}, game pc samples {:x?} (count {}), dispcnt {:x} dispstat {:x} ie {:x} if {:x} ime {:x}",
                            EWRAM_PAGES_USED,
                            FAULT_COUNT,
                            SVC_COUNTS,
                            JIT.as_ref().map(|j| j.block_count()).unwrap_or(0),
                            JIT.as_ref().map(|j| j.code_used_bytes()).unwrap_or(0),
                            LAST_SVC,
                            PC_SAMPLES,
                            PC_SAMPLE_COUNT,
                            core::ptr::read_volatile(io_reg(0)),
                            core::ptr::read_volatile(io_reg(DISPSTAT_OFFSET)),
                            core::ptr::read_volatile(io_reg(IE_OFFSET)),
                            core::ptr::read_volatile(io_reg(IF_OFFSET)),
                            core::ptr::read_volatile(io_reg(IME_OFFSET))
                        );
                    }
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
        HertzU32::Hz(62_500_000), // clk_peri / 2, the fastest the SPI divider allows
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
        Video::new(
            &mut NEW_REG.display,
            &mut NEW_PALETTE,
            &mut NEW_VRAM,
            &mut NEW_OAM,
        )
    };

    display.clear(RgbColor::BLACK);

    // mipidsi is only used to set the display up, after that scanlines go out through DMA
    let (di, _, _) = display.release();
    let (spi_device, dc_pin) = di.release();
    let dma = pac.DMA.split(&mut pac.RESETS);

    jit_init();
    let mut mc = multicore::Multicore::new(&mut pac.PSM, &mut pac.PPB, &mut sio.fifo);
    let cores = mc.cores();
    let core1 = &mut cores[1];

    unsafe {
        core1
            .spawn(CORE1_STACK.take().unwrap(), move || {
                let touch = Touch {
                    tp_cs,
                    tp_irq,
                    lcd_cs: spi_cs,
                };
                video_loop(
                    spi_device,
                    dc_pin,
                    dma.ch0,
                    video,
                    systick,
                    touch,
                    keyinput_register(),
                );
            })
            .unwrap();
    };

    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::SIO_IRQ_PROC0) };

    let rom_addr = ROM_CODE.as_ptr() as u32;
    let thumb_start = rom_addr + 0xdc | 1; // last bit needs to be 1 for thumb mode
    unsafe { asm!("bx {}", in(reg) thumb_start) };

    loop {}
}
