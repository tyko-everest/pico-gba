#![no_std]
#![no_main]
#![allow(static_mut_refs)]
mod controls;
mod gba_mem;
mod romstore;
mod sdcard;

use common::{
    bios::{self, Memory},
    jit::{DISPATCH_BYTES, Jit, JitError, MapEntry, TRAP},
    profile,
    registers::*,
    romcache::{PAGE_SIZE, RomCache, RomSource},
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
use romstore::RomStore;
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

// the GBA memory map
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
const EWRAM_LEN: u32 = KB(34);

const IWRAM_START: u32 = 0x0300_0000;
const IWRAM_LEN: u32 = KB(32);

// Cartridge save memory. Only a small window of it exists and it starts out blank.
const SRAM_LEN: u32 = KB(4);

const ROM_START: u32 = 0x0800_0000;

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

// The ROM is read from the SD card a page at a time through this cache, there is not nearly room
// for all of it. A page is read in the first time it is used, by the other core.
const ROM_CACHE_SLOTS: usize = 64;

#[repr(C, align(4))]
struct RomCacheData([u8; ROM_CACHE_SLOTS * PAGE_SIZE]);

static mut ROM_CACHE_DATA: RomCacheData = RomCacheData([0; ROM_CACHE_SLOTS * PAGE_SIZE]);
static mut ROM_CACHE_TAGS: [u32; ROM_CACHE_SLOTS] = [0; ROM_CACHE_SLOTS];
static mut ROM_CACHE_STAMPS: [u32; ROM_CACHE_SLOTS] = [0; ROM_CACHE_SLOTS];
static mut ROM_CACHE: Option<RomCache<'static>> = None;

fn rom_cache_init() {
    unsafe {
        ROM_CACHE = Some(RomCache::new(
            &mut ROM_CACHE_DATA.0,
            &mut ROM_CACHE_TAGS,
            &mut ROM_CACHE_STAMPS,
        ));
    }
}

// Pages come from the SD card, through core 1
struct SdRom;

impl RomSource for SdRom {
    fn read_page(&mut self, offset: u32, dst: &mut [u8; PAGE_SIZE]) {
        if !romstore::read(offset, dst) {
            dst.fill(0);
        }
    }
}

// Translated ARM code. The game's ARM mode code cannot run on the Cortex-M0+, the fault handler
// translates it to Thumb the first time it is reached and runs it from this cache.
const JIT_CODE_HW: usize = 16 * 1024;
const JIT_MAP_LEN: usize = 1024;

#[repr(C, align(4))]
struct JitCode([u16; JIT_CODE_HW]);

static mut JIT_CODE: JitCode = JitCode([0; JIT_CODE_HW]);
static mut JIT_MAP: [MapEntry; JIT_MAP_LEN] = [Jit::empty_map_entry(); JIT_MAP_LEN];
// the indirect jump table of the translated Thumb code
#[repr(C, align(4))]
struct JitDispatch([u32; DISPATCH_BYTES / 4]);
static mut JIT_DISPATCH: JitDispatch = JitDispatch([0; DISPATCH_BYTES / 4]);
static mut JIT: Option<Jit<'static>> = None;

// Translated loads and stores find the native address of a GBA address through this table,
// indexed by the top byte: an even entry is an offset to add, an odd one is a page table (minus
// the bit) for the regions that are paged or mirrored. 0 leaves the address alone, it faults.
static mut JIT_REGIONS: [u32; 256] = [0; 256];
// IWRAM in 512 byte pages, repeated so the table covers the 256 KiB the lookup indexes
static mut IWRAM_PAGE_TABLE: [u32; PAGE_TABLE_LEN] = [0; PAGE_TABLE_LEN];

#[allow(static_mut_refs)]
fn jit_init() {
    unsafe {
        let iwram = &raw const NEW_IWRAM as u32;
        for (i, page) in IWRAM_PAGE_TABLE.iter_mut().enumerate() {
            *page = iwram + (i as u32 * EWRAM_PAGE) % IWRAM_LEN;
        }
        let flat = |gba: u32, native: u32| native.wrapping_sub(gba);
        JIT_REGIONS[0x02] = EWRAM_PAGE_TABLE.as_ptr() as u32 | 1;
        JIT_REGIONS[0x03] = IWRAM_PAGE_TABLE.as_ptr() as u32 | 1;
        // IO and SRAM have side effects, accesses to them fault and the handler emulates them
        JIT_REGIONS[0x05] = flat(PALETTE_START, &raw const NEW_PALETTE as u32);
        JIT_REGIONS[0x06] = flat(VRAM_START, &raw const NEW_VRAM as u32);
        JIT_REGIONS[0x07] = flat(OAM_START, &raw const NEW_OAM as u32);
        // ROM is not mapped either: it is paged in from the SD card by the handler

        let base = JIT_CODE.0.as_ptr() as u32;
        let regions = JIT_REGIONS.as_ptr() as u32;
        JIT = Some(Jit::new(
            &mut JIT_CODE.0,
            base,
            &mut JIT_MAP,
            regions,
            JIT_DISPATCH.0.as_ptr() as u32,
        ));
        JIT.as_mut().unwrap().set_reserve(JIT_RESERVE_HW);
    }
}

// Reads a word of GBA memory for the translator. It also reads the region and page tables
// (native addresses) to resolve loads and stores through known constant addresses.
#[allow(static_mut_refs)]
fn gba_read_word(addr: u32) -> Option<u32> {
    let table = |start: u32, len: usize| addr >= start && addr < start + 4 * len as u32;
    let native = unsafe {
        table(JIT_REGIONS.as_ptr() as u32, JIT_REGIONS.len())
            || table(IWRAM_PAGE_TABLE.as_ptr() as u32, PAGE_TABLE_LEN)
            || table(EWRAM_PAGE_TABLE.as_ptr() as u32, PAGE_TABLE_LEN)
    };
    if native {
        Some(unsafe { core::ptr::read_volatile(addr as *const u32) })
    } else {
        Some(gba_mem::load(addr, 4))
    }
}

// The code cache is thrown away when there is little room left, outside of the interrupt handler:
// the code it interrupted is in the cache. The handler needs the room to translate its code
// again afterwards.
const JIT_MARGIN_HW: usize = 3 * 1024;
// held back from translation until a full cache cannot be thrown away
const JIT_RESERVE_HW: usize = 2 * 1024;
const JIT_MARGIN_BLOCKS: usize = 200;

fn jit_needs_flush(jit: &Jit) -> bool {
    !unsafe { IN_IRQ }
        && (jit.free_halfwords() < JIT_MARGIN_HW || jit.free_blocks() < JIT_MARGIN_BLOCKS)
}

// Does the stack hold a return address in the code cache? Translated ARM code that calls other
// ARM code keeps one in lr, and a flush would leave it pointing at nothing.
fn cache_address_on_stack(jit: &Jit, frame: u32) -> bool {
    let top = &raw const _stack_start as u32;
    let holds_cache_address =
        |at: u32| jit.contains(unsafe { core::ptr::read_volatile(at as *const u32) } & !1);
    // the registers the exception stacked: r0-r3, r12 and lr (the pc and xpsr that follow are
    // where the code stopped, which is in the cache)
    if (0..6).any(|i| holds_cache_address(frame + 4 * i)) {
        return true;
    }
    let mut at = frame + size_of::<ExceptionFrame>() as u32;
    while at < top {
        if holds_cache_address(at) {
            return true;
        }
        at += 4;
    }
    false
}

fn jit_flush(jit: &mut Jit) {
    unsafe { JIT_IRQ_HW = 0 };
    jit.reset();
    unsafe { JIT_DISPATCH.0.fill(0) };
    unsafe { JIT_FLUSHES += 1 };
    // the code the interrupt handler used goes back first
    for i in 0..unsafe { JIT_PIN_COUNT } {
        let key = unsafe { JIT_PIN_KEYS[i] };
        let _ = jit.translate(key, &mut gba_read_word);
    }
}

static mut JIT_FLUSHES: u32 = 0;
// code translated inside the interrupt handler since the last flush (halfwords), and the most of that
static mut JIT_IRQ_HW: usize = 0;
static mut JIT_IRQ_MAX: usize = 0;
// the core, pc, lr, xpsr and r0-r3 of the last fault that was not in translated code
#[unsafe(no_mangle)]
static mut NATIVE_FAULT: [u32; 8] = [0; 8];

// The blocks the interrupt handler needed. The cache cannot be flushed while it runs, so after a
// flush these are translated again at once, not inside the next interrupt where the room might
// run out.
const JIT_PINNED: usize = 160;
static mut JIT_PIN_KEYS: [u32; JIT_PINNED] = [0; JIT_PINNED];
static mut JIT_PIN_COUNT: usize = 0;

// Count what the interrupt handler translated, and remember where it started
fn note_irq_code(jit: &Jit, used_before: usize, key: u32) {
    if unsafe { IN_IRQ } && jit.used_halfwords() > used_before {
        unsafe {
            JIT_IRQ_HW += jit.used_halfwords() - used_before;
            JIT_IRQ_MAX = JIT_IRQ_MAX.max(JIT_IRQ_HW);
            if JIT_PIN_COUNT < JIT_PINNED && !JIT_PIN_KEYS[..JIT_PIN_COUNT].contains(&key) {
                JIT_PIN_KEYS[JIT_PIN_COUNT] = key;
                JIT_PIN_COUNT += 1;
            }
        }
    }
}

fn jit_translate(jit: &mut Jit, key: u32, sp: u32) -> Result<u32, JitError> {
    let used_before = jit.used_halfwords();
    let result = jit_translate_inner(jit, key, sp);
    note_irq_code(jit, used_before, key);
    result
}

// What recovered from a full cache
#[derive(PartialEq)]
enum Recovery {
    // the cache was thrown away
    Flushed,
    // the reserve was opened up
    Extended,
    None,
}

// The cache is full: throw it away if nothing can be pointing into it (not in the interrupt
// handler, which interrupted code in the cache, nor with a return address on the stack), else use
// the reserve that was held back for this.
fn jit_recover(jit: &mut Jit, sp: u32) -> Recovery {
    if !unsafe { IN_IRQ } && !cache_address_on_stack(jit, sp) {
        jit_flush(jit);
        Recovery::Flushed
    } else if !jit.in_emergency() {
        jit.use_reserve();
        Recovery::Extended
    } else {
        Recovery::None
    }
}

fn jit_translate_inner(jit: &mut Jit, key: u32, sp: u32) -> Result<u32, JitError> {
    if jit_needs_flush(jit) && !cache_address_on_stack(jit, sp) {
        jit_flush(jit);
    }
    loop {
        match jit.translate(key, &mut gba_read_word) {
            Err(JitError::CacheFull) if jit_recover(jit, sp) != Recovery::None => {}
            r => return r,
        }
    }
}

// An unresolved exit slot was reached
fn jit_resolve(jit: &mut Jit, slot: u32, sp: u32) -> Result<u32, JitError> {
    let used_before = jit.used_halfwords();
    let key = jit.halfword(slot + 4) as u32 | (jit.halfword(slot + 6) as u32) << 16;
    let result = jit_resolve_inner(jit, slot, key, sp);
    note_irq_code(jit, used_before, key);
    result
}

fn jit_resolve_inner(jit: &mut Jit, slot: u32, key: u32, sp: u32) -> Result<u32, JitError> {
    if jit_needs_flush(jit) && !cache_address_on_stack(jit, sp) {
        // the slot goes with the rest of the cache
        jit_flush(jit);
        return jit.translate(key, &mut gba_read_word);
    }
    match jit.resolve_slot(slot, &mut gba_read_word) {
        Err(JitError::CacheFull) => match jit_recover(jit, sp) {
            Recovery::Flushed => jit.translate(key, &mut gba_read_word),
            Recovery::Extended => jit.resolve_slot(slot, &mut gba_read_word),
            Recovery::None => Err(JitError::CacheFull),
        },
        r => r,
    }
}

// Is this a GBA address that code runs from
fn is_guest_code(addr: u32) -> bool {
    matches!(addr >> 24, 2 | 3 | 8..=0xD)
}

// Handles the faults caused by translated code: unresolved exits, dispatches that missed, and
// jumps to GBA code (a bx from translated ARM code to Thumb code, an ARM jump). Returns true
// when the fault was dealt with.
fn jit_fault(frame: &mut ExceptionFrame) -> bool {
    let Some(jit) = (unsafe { JIT.as_mut() }) else {
        return false;
    };
    let pc = frame.pc();
    const THUMB_BIT: u32 = 1 << 24;
    let sp = frame as *mut ExceptionFrame as u32;
    let target = if jit.contains(pc) && jit.halfword(pc) == TRAP {
        jit_resolve(jit, pc, sp)
    } else if jit.is_stub(pc) {
        // a dispatch missed, it left its target in the miss word
        let target = unsafe { core::ptr::read_volatile(jit.miss_var() as *const u32) };
        if !is_guest_code(target) {
            // native code: the cache, or something the game copied to the stack
            Ok(target)
        } else {
            let result = jit_translate(jit, target, sp);
            if let (Ok(native), true) = (result, target & 1 != 0) {
                let entry = jit.dispatch_entry(target) as *mut u32;
                unsafe {
                    core::ptr::write_volatile(entry, target);
                    core::ptr::write_volatile(entry.add(1), native);
                }
            }
            result
        }
    } else if is_guest_code(pc) {
        let key = if frame.xpsr() & THUMB_BIT != 0 {
            pc | 1
        } else {
            pc & !3
        };
        jit_translate(jit, key, sp)
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
            rprintln!("cannot translate code at 0x{:x}: {:?}", pc, e);
            panic!()
        }
    }
}

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    rprintln!("panic: {}", info);
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

const PAGE_TABLE_LEN: usize = (EWRAM_MIRROR / EWRAM_PAGE) as usize;

// The native address of every EWRAM page. A page that is not handed out yet holds its own GBA
// address, so translated code that reaches it through this table faults and ends up here.
static mut EWRAM_PAGE_TABLE: [u32; PAGE_TABLE_LEN] = {
    let mut t = [0; PAGE_TABLE_LEN];
    let mut i = 0;
    while i < PAGE_TABLE_LEN {
        t[i] = EWRAM_START + i as u32 * EWRAM_PAGE;
        i += 1;
    }
    t
};
static mut EWRAM_PAGES_USED: u32 = 0;
// set when a page was needed and the pool had none left: stores to it are lost
static mut EWRAM_POOL_FULL: bool = false;

// A page that has not been handed out holds its own GBA address in the table
fn ewram_page_empty(gba_addr: u32) -> bool {
    let page = ((gba_addr - EWRAM_START) & (EWRAM_MIRROR - 1)) / EWRAM_PAGE;
    unsafe { EWRAM_PAGE_TABLE[page as usize] == EWRAM_START + page * EWRAM_PAGE }
}

// The BIOS is not here. Code outside it reads the last word the BIOS fetched, the same value at
// every address, and writes do nothing. C++ code that follows a null pointer relies on this: both
// ends of an empty std::vector read the same word, the RP2040 boot rom there has different words.
const BIOS_LEN: u32 = KB(16);
const BIOS_OPEN_BUS: u32 = 0xE129_F000;

const EWRAM_SLOTS: usize = (EWRAM_LEN / EWRAM_PAGE) as usize;
// the page table index each slot of the pool holds, EWRAM_FREE for one that is free
const EWRAM_FREE: u16 = u16::MAX;
static mut EWRAM_SLOT_PAGE: [u16; EWRAM_SLOTS] = [EWRAM_FREE; EWRAM_SLOTS];

// Free the pages that hold nothing but zeros, which are the same as pages that were never used.
// The translator does not build the native address of an EWRAM page into code, so a page can go.
// Returns whether any page was freed.
fn ewram_reclaim() -> bool {
    unsafe {
        let pool = &raw const NEW_EWRAM as u32;
        let mut freed = false;
        for slot in 0..EWRAM_SLOTS {
            let page = EWRAM_SLOT_PAGE[slot];
            if page == EWRAM_FREE {
                continue;
            }
            let words = (pool + slot as u32 * EWRAM_PAGE) as *const u32;
            if (0..EWRAM_PAGE as usize / 4).all(|i| core::ptr::read_volatile(words.add(i)) == 0) {
                EWRAM_PAGE_TABLE[page as usize] = EWRAM_START + page as u32 * EWRAM_PAGE;
                EWRAM_SLOT_PAGE[slot] = EWRAM_FREE;
                EWRAM_PAGES_USED -= 1;
                freed = true;
            }
        }
        freed
    }
}

fn ewram_native(gba_addr: u32) -> Option<u32> {
    unsafe {
        let offset = (gba_addr - EWRAM_START) & (EWRAM_MIRROR - 1);
        let page = (offset / EWRAM_PAGE) as usize;
        if ewram_page_empty(gba_addr) {
            let mut slot = EWRAM_SLOT_PAGE.iter().position(|&p| p == EWRAM_FREE);
            if slot.is_none() && ewram_reclaim() {
                slot = EWRAM_SLOT_PAGE.iter().position(|&p| p == EWRAM_FREE);
            }
            let Some(slot) = slot else {
                EWRAM_POOL_FULL = true;
                return None;
            };
            let pool = &raw const NEW_EWRAM as u32;
            EWRAM_PAGE_TABLE[page] = pool + slot as u32 * EWRAM_PAGE;
            EWRAM_SLOT_PAGE[slot] = page as u16;
            EWRAM_PAGES_USED += 1;
        }
        Some(EWRAM_PAGE_TABLE[page] + offset % EWRAM_PAGE)
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

    // Every unit is accessed on its own, the memory behind an address range is not contiguous
    // (EWRAM is paged, ROM is cached).
    let mut mem = gba_mem::Gba;
    let first = if fill { mem.read32(src) } else { 0 };
    for i in 0..count {
        let from = src + if fill { 0 } else { i as u32 * unit };
        let to = dst + i as u32 * unit;
        if word {
            let value = if fill { first } else { mem.read32(from) };
            mem.write32(to, value);
        } else {
            let value = if fill { first as u16 } else { mem.read16(from) };
            mem.write16(to, value);
        }
    }
}

// pc, call number and r0-r2 of the last BIOS call, for debugging
static mut LAST_SVC: [u32; 5] = [0; 5];
static mut SVC_COUNTS: [u32; 16] = [0; 16];
static mut FAULT_COUNT: u32 = 0;
#[unsafe(no_mangle)]
static mut FAULT_PCS: [u32; 8] = [0; 8];

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
        // DivArm: the same with the operands swapped
        0x07 => {
            let (num, den) = (frame.r1() as i32, frame.r0() as i32);
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
        0x08 => unsafe { frame.set_r0(bios::sqrt(frame.r0())) },
        0x09 => unsafe { frame.set_r0(bios::arctan(frame.r0() as i32) as u32) },
        0x0A => unsafe { frame.set_r0(bios::arctan2(frame.r0() as i32, frame.r1() as i32)) },
        0x0E => bios::bg_affine_set(&mut gba_mem::Gba, frame.r0(), frame.r1(), frame.r2()),
        0x0F => bios::obj_affine_set(
            &mut gba_mem::Gba,
            frame.r0(),
            frame.r1(),
            frame.r2(),
            frame.r3(),
        ),
        0x11 | 0x12 => bios::lz77(
            &mut gba_mem::Gba,
            frame.r0(),
            frame.r1(),
            instr & 0xFF == 0x12,
        ),
        0x13 => bios::huffman(&mut gba_mem::Gba, frame.r0(), frame.r1()),
        0x14 | 0x15 => bios::run_length(
            &mut gba_mem::Gba,
            frame.r0(),
            frame.r1(),
            instr & 0xFF == 0x15,
        ),
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

// total time spent in the fault handler in microseconds and how many faults it was over
static mut FAULT_TICKS: u32 = 0;

#[unsafe(no_mangle)]
extern "C" fn hard_fault(frame: &mut ExceptionFrame, other_regs: &mut Reg4_7) {
    // the 1 MHz timer, individual readings are coarse but the sum over many faults is not
    let timer = unsafe { &*pac::TIMER::ptr() };
    let start = timer.timerawl().read().bits();
    hard_fault_inner(frame, other_regs);
    unsafe {
        FAULT_TICKS = FAULT_TICKS.wrapping_add(timer.timerawl().read().bits().wrapping_sub(start))
    };
}

fn hard_fault_inner(frame: &mut ExceptionFrame, other_regs: &mut Reg4_7) {
    unsafe {
        FAULT_PCS[FAULT_COUNT as usize % 8] = frame.pc();
        FAULT_COUNT += 1;
    }

    if jit_fault(frame) {
        return;
    }
    // a fault in the firmware's own code, which is a bug: keep what is known where a debugger
    // finds it, printing could deadlock on the other core's lock
    let pc = frame.pc();
    if !unsafe { JIT.as_ref().is_some_and(|j| j.contains(pc)) } {
        unsafe {
            NATIVE_FAULT = [
                (*pac::SIO::ptr()).cpuid().read().bits(),
                pc,
                frame.lr(),
                frame.xpsr(),
                frame.r0(),
                frame.r1(),
                frame.r2(),
                frame.r3(),
            ];
        }
    }

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
        let jit = unsafe { JIT.as_ref().unwrap() };
        rprintln!(
            "cannot handle opcode 0x{:x} at pc 0x{:x}, lr 0x{:x}, in the code cache {}, cache used {} bytes, {} flushes",
            instr,
            frame.pc(),
            frame.lr(),
            jit.contains(frame.pc()),
            jit.code_used_bytes(),
            unsafe { JIT_FLUSHES }
        );
        rprintln!(
            "r0-r7 {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x}, in irq {}",
            frame.r0(),
            frame.r1(),
            frame.r2(),
            frame.r3(),
            { other_regs.r4 },
            { other_regs.r5 },
            { other_regs.r6 },
            { other_regs.r7 },
            unsafe { IN_IRQ }
        );
        rprintln!(
            "last svc {:x?}, irq pcs {:x?}",
            unsafe { LAST_SVC },
            unsafe { PC_SAMPLES }
        );
        let around = (frame.pc() & !1).wrapping_sub(32);
        for i in 0..24 {
            let at = around + 2 * i;
            let raw = unsafe { core::ptr::read_volatile(at as *const u16) };
            rprintln!("  {:x}: {:04x}", at, raw);
        }
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
            if load {
                let value = gba_mem::load(address, 4);
                set(frame, other_regs, r, value);
            } else {
                gba_mem::store(address, 4, get(frame, other_regs, r));
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
        if load {
            let mut value = gba_mem::load(address, size);
            if signed {
                value = match size {
                    1 => value as u8 as i8 as i32 as u32,
                    _ => value as u16 as i16 as i32 as u32,
                };
            }
            set(frame, other_regs, rt, value);
        } else {
            gba_mem::store(address, size, get(frame, other_regs, rt));
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
static mut SAVED_MISS: u32 = 0;
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
        // A dispatch that missed leaves its target in a word, and the trap that follows reads it:
        // an interrupt in between would replace it with its own.
        if let Some(jit) = JIT.as_ref() {
            SAVED_MISS = core::ptr::read_volatile(jit.miss_var() as *const u32);
        }
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
        if let Some(jit) = JIT.as_ref() {
            core::ptr::write_volatile(jit.miss_var() as *mut u32, SAVED_MISS);
        }
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

// the longest a single ROM read request is expected to take, requests are not started after
// this long before the end of VBlank
const SD_MARGIN_US: u32 = 2000;

fn video_loop<SPI, DC, CH, TpCs, TpIrq, LcdCs, SdCs>(
    mut spi: SPI,
    mut dc: DC,
    dma_channel: CH,
    video: &mut Video,
    mut systick: SYST,
    mut touch: Touch<TpCs, TpIrq, LcdCs>,
    keyinput: *mut u16,
    mut rom_store: Option<&mut RomStore<SdCs>>,
) -> !
where
    SPI: SpiDevice,
    DC: OutputPin,
    CH: SingleChannel,
    TpCs: OutputPin,
    TpIrq: InputPin,
    LcdCs: OutputPin,
    SdCs: OutputPin,
{
    video.set_colour_format(gba_to_display);

    // the game polls these from the other core, so they are written with single 16 bit stores
    let disp_status = core::ptr::addr_of_mut!(video.registers.disp_status) as *mut u16;
    let vert_counter = core::ptr::addr_of_mut!(video.registers.vert_counter) as *mut u16;
    assert!(disp_status as usize % 2 == 0 && vert_counter as usize % 2 == 0);

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
                // The bus is idle until the next line goes out: a ROM read the other core is
                // waiting for is done now, not at VBlank
                if romstore::request_pending() {
                    if let Some(store) = rom_store.as_deref_mut() {
                        spi1_flush();
                        store.poll(&mut touch.lcd_cs);
                    }
                }
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
                if cfg!(feature = "autopress") {
                    // a test without hands: A held for 6 frames out of every 90
                    static mut AUTO_FRAME: u32 = 0;
                    unsafe {
                        AUTO_FRAME += 1;
                        if AUTO_FRAME % 90 < 6 {
                            keys |= controls::KEY_A;
                        }
                    }
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

                // SPI1 stays idle for the rest of VBlank, so this is when the SD card is used.
                // Stop in time to not make the first line of the next frame late.
                if let Some(store) = rom_store.as_deref_mut() {
                    let deadline = gba_cycles_to_us((LINES_PER_FRAME as u32 - 1) * LINE_CYCLES)
                        .saturating_sub(SD_MARGIN_US);
                    // one request is always served, or a frame that is late would starve the other
                    // core, then more while there is time
                    let mut served = store.poll(&mut touch.lcd_cs);
                    while served && elapsed(frame_start) < deadline {
                        served = store.poll(&mut touch.lcd_cs);
                    }
                }

                vblank_late_max =
                    vblank_late_max.max(elapsed(frame_start).saturating_sub(line_start));

                frames += 1;
                render_total += render_time;
                render_max = render_max.max(render_time);
                if frames == REPORT_FRAMES {
                    unsafe {
                        rprintln!(
                            "fault us {}, ewram pages {}, faults {}, svc counts {:?}, jit blocks {} ({} bytes), last svc {:x?}, game pc samples {:x?} (count {}), dispcnt {:x} dispstat {:x} ie {:x} if {:x} ime {:x}",
                            FAULT_TICKS,
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
                        rprintln!(
                            "stacks used: core 0 {} of {}, core 1 {} of {}; jit flushes {}, most code translated in irq {} bytes; rom cache hits {} misses {}; ewram pool full {}; sd read retries {} (last error {:#x})",
                            stack_used(&raw const __sheap as u32, &raw const _stack_start as u32),
                            &raw const _stack_start as u32 - &raw const __sheap as u32,
                            stack_used(
                                &raw const CORE1_STACK as u32,
                                &raw const CORE1_STACK as u32 + size_of::<Stack<2560>>() as u32
                            ),
                            size_of::<Stack<2560>>(),
                            JIT_FLUSHES,
                            JIT_IRQ_MAX * 2,
                            ROM_CACHE.as_ref().map_or(0, |c| c.hits),
                            ROM_CACHE.as_ref().map_or(0, |c| c.misses),
                            EWRAM_POOL_FULL,
                            sdcard::RETRIES.load(core::sync::atomic::Ordering::Relaxed),
                            sdcard::LAST_ERROR.load(core::sync::atomic::Ordering::Relaxed)
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

const STACK_PAINT: u32 = 0xC0DE_C0DE;

unsafe extern "C" {
    static __sheap: u32;
    static _stack_start: u32;
}

// Fill the unused part of a stack with a pattern, so stack_used can tell how deep it went
unsafe fn paint_stack(low: u32, high: u32) {
    let mut at = low & !3;
    while at < high {
        unsafe { core::ptr::write_volatile(at as *mut u32, STACK_PAINT) };
        at += 4;
    }
}

// The most of the stack between low and high that has been used
fn stack_used(low: u32, high: u32) -> u32 {
    let mut at = low & !3;
    while at < high && unsafe { core::ptr::read_volatile(at as *const u32) } == STACK_PAINT {
        at += 4;
    }
    high - at
}

// The video state is big and stays in a static. Building it in a function of its own gives the
// room it takes on the stack back when it is done, setup keeps it for good otherwise.
#[inline(never)]
fn init_video() -> &'static mut Video<'static> {
    unsafe {
        VIDEO = Some(Video::new(
            &mut NEW_REG.display,
            &mut NEW_PALETTE,
            &mut NEW_VRAM,
            &mut NEW_OAM,
        ));
        VIDEO.as_mut().unwrap()
    }
}

// Wait until core 1 has mounted the SD card and looked for a ROM, its size when there is one
fn wait_for_rom(timer: &Timer) -> Option<u32> {
    let start = timer.get_counter().ticks();
    loop {
        if let Some(found) = romstore::rom_size() {
            return found;
        }
        if timer.get_counter().ticks() - start > 5_000_000 {
            rprintln!("core 1 did not look at the SD card");
            return None;
        }
    }
}

// The pin the SD card's chip select is on
type SdCsPin = rp2040_hal::gpio::Pin<
    rp2040_hal::gpio::bank0::Gpio22,
    rp2040_hal::gpio::FunctionSioOutput,
    rp2040_hal::gpio::PullDown,
>;

// The ROM store is big, and core 1's stack is small
static mut VIDEO: Option<Video<'static>> = None;

static mut ROM_STORE: Option<RomStore<SdCsPin>> = None;

static mut CORE1_STACK: Stack<2560> = Stack::new();

#[entry]
fn main() -> ! {
    // all the setup is in a function that returns, its stack frame is big and the game does not
    // need that room
    let thumb_start = setup();
    unsafe {
        // measure the stack from here on, not the setup's
        let here: u32;
        asm!("mov {}, sp", out(reg) here);
        paint_stack(&raw const __sheap as u32, here - 256);
        asm!("bx {}", in(reg) thumb_start);
    }
    loop {}
}

// Set everything up and return the GBA address to start the game at
#[inline(never)]
fn setup() -> u32 {
    rtt_init_print!();
    let mut pac = pac::Peripherals::take().unwrap();
    let core = cortex_m::Peripherals::take().unwrap();
    unsafe {
        let here: u32;
        asm!("mov {}, sp", out(reg) here);
        paint_stack(&raw const __sheap as u32, here - 256);
    }
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

    // Probe the SD card before the display is set up, they share SPI1 and the card needs it slow
    let mut sd_cs = pins.gpio22.into_push_pull_output();
    sd_cs.set_high().unwrap();
    spi_cs.set_high().unwrap();
    let sd_card = match sdcard::SdCard::new(sd_cs) {
        Ok(mut card) => {
            let blocks = card.block_count();
            rprintln!("sd card {:?}, {:?} blocks", card.card_type, blocks);
            Some(card)
        }
        Err(e) => {
            rprintln!("sd card init failed: {:?}", e);
            None
        }
    };
    controls::spi1_configure(controls::DISPLAY_SCR, true, true);
    spi_cs.set_low().unwrap();

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
    // the BIOS leaves the display in forced blank, games clear it when they are ready. Some of
    // them (the ones that buffer their register writes) rely on seeing it.
    unsafe { core::ptr::write_volatile(io_reg(0), 0x0080) };

    let video = init_video();

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
        paint_stack(
            &raw const CORE1_STACK as u32,
            &raw const CORE1_STACK as u32 + size_of::<Stack<2560>>() as u32 - 64,
        );
        core1
            .spawn(CORE1_STACK.take().unwrap(), move || {
                let mut touch = Touch {
                    tp_cs,
                    tp_irq,
                    lcd_cs: spi_cs,
                };
                // the ROM store mounts the card now, before the display's SPI1 traffic starts
                let rom_store = unsafe {
                    ROM_STORE = sd_card.map(|card| RomStore::new(card, &mut touch.lcd_cs));
                    ROM_STORE.as_mut()
                };
                video_loop(
                    spi_device,
                    dc_pin,
                    dma.ch0,
                    video,
                    systick,
                    touch,
                    keyinput_register(),
                    rom_store,
                );
            })
            .unwrap();
    };

    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::SIO_IRQ_PROC0) };

    rom_cache_init();
    rprintln!("rom cache ready");
    let Some(rom_len) = wait_for_rom(&timer) else {
        rprintln!("no ROM on the SD card");
        loop {}
    };
    rprintln!("ROM of {} bytes", rom_len);

    // The game's idea of the BIOS is address 0, which on the RP2040 is its boot rom. Reads of it
    // do not fault, so make it no access on this core and let the fault handler answer for it.
    // The handler itself is exempt (HFNMIENA is off).
    unsafe {
        let mpu = &*cortex_m::peripheral::MPU::PTR;
        const SIZE_16K: u32 = 13 << 1;
        const NO_ACCESS: u32 = 0 << 24;
        const EXECUTE_NEVER: u32 = 1 << 28;
        mpu.rnr.write(0);
        mpu.rbar.write(0);
        mpu.rasr.write(EXECUTE_NEVER | NO_ACCESS | SIZE_16K | 1);
        // enable, and keep the default memory map for everything else
        mpu.ctrl.write(1 | 1 << 2);
        asm!("dsb", "isb");
    }

    // The crt0 of the ROM is ARM code that sets up stacks and the interrupt handler, all that is
    // left to do of it is to start the game where it goes to Thumb
    let Some(entry) = common::rom::find_entry(|at| gba_mem::load(ROM_START + at, 4)) else {
        rprintln!("cannot find where the ROM switches to Thumb");
        panic!()
    };
    if entry.irq_handler != 0 {
        unsafe {
            let vector = (&raw mut NEW_IWRAM as *mut u8).add(IRQ_VECTOR_OFFSET) as *mut u32;
            core::ptr::write_volatile(vector, entry.irq_handler);
        }
    }
    // the GBA address, the translator runs the code from the code cache
    let thumb_start = entry.thumb;
    unsafe {
        rprintln!(
            "stack used by setup: {}",
            stack_used(&raw const __sheap as u32, &raw const _stack_start as u32)
        );
    }
    thumb_start
}
