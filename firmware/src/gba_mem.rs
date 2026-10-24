// GBA memory for everything that cannot reach it by itself: the loads and stores the fault handler
// emulates, BIOS calls and DMA. Addresses are GBA addresses, a native one (the stack, or code the
// game copied to it) passes through. This is also where the hardware registers with side effects
// live: timers, DMA, the interrupt flags and the flash save chip.
use crate::romstore;
use crate::{
    BIOS_LEN, BIOS_OPEN_BUS, IWRAM_LEN, IWRAM_START, NEW_IWRAM, NEW_OAM, NEW_PALETTE, NEW_REG,
    NEW_SRAM, NEW_VRAM, OAM_LEN, PALETTE_LEN, REG_LEN, ROM_CACHE, ROM_START, SRAM_LEN, SdRom,
    VRAM_LEN, ewram_native, ewram_page_empty, pac, read_native, write_native,
};
use common::{
    bios::Memory,
    dma::Dma,
    eeprom::Eeprom,
    flash::FlashChip,
    timers::{TIMER_BASE, TIMER_END, Timers},
};

static mut FLASH: FlashChip = FlashChip::new();
static mut TIMERS: Timers = Timers::new();
// the save EEPROM some games have at 0x0D000000 (the part of the ROM mirrors a game never reads)
static mut EEPROM: Eeprom = Eeprom::new();

const IF_OFFSET: u32 = 0x202;

// The GBA clock, 16.78 MHz, from the 1 MHz timer: times 2^24 / 10^6, which is 17592186 / 2^20
fn gba_cycles() -> u64 {
    let micros = unsafe { (*pac::TIMER::ptr()).timerawl().read().bits() } as u64;
    micros * 17_592_186 >> 20
}

// Timer interrupts: alarm 3 of the 1 MHz timer is set for the soonest overflow of a timer that has
// its interrupt enabled. When it goes off `timer_alarm` raises the flag in IF, and the interrupt
// handler of core 0 injects the game's handler like it does for VBlank.
const TIMER_ALARM: u32 = 1 << 3;
// how many times the alarm was set, went off, and raised an interrupt flag, and the last deadline
pub static mut TIMER_STATS: [u32; 4] = [0; 4];
// RegisterRamReset flag bits 0x80 and 0x20 and 0x40 reset registers: the timers' state is here
pub fn reset_timers() {
    unsafe { TIMERS = Timers::new() };
    timer_arm();
}

// the last IO registers loaded and stored (offset, for a debugger), and how many of each
#[unsafe(no_mangle)]
pub static mut IO_LOADS: [u32; 8] = [0; 8];
#[unsafe(no_mangle)]
pub static mut IO_STORES: [u32; 8] = [0; 8];
#[unsafe(no_mangle)]
pub static mut IO_COUNTS: [u32; 2] = [0; 2];
// the last addresses loaded (any region) and stored
#[unsafe(no_mangle)]
pub static mut LOAD_ADDRS: [u32; 8] = [0; 8];
#[unsafe(no_mangle)]
pub static mut STORE_ADDRS: [u32; 8] = [0; 8];
#[unsafe(no_mangle)]
pub static mut ADDR_COUNTS: [u32; 2] = [0; 2];

pub fn timer_arm() {
    unsafe {
        let timer = &*pac::TIMER::ptr();
        match TIMERS.next_irq() {
            None => timer.inte().modify(|r, w| w.bits(r.bits() & !TIMER_ALARM)),
            Some(at) => {
                TIMER_STATS[0] += 1;
                // the alarm goes off when the low 32 bits of the microseconds match: aim a few
                // microseconds ahead at the least, a time that has already passed would not fire
                let delta = at.saturating_sub(gba_cycles());
                let micros = ((delta << 20) / 17_592_186).max(4) as u32;
                let now = timer.timerawl().read().bits();
                timer.alarm3().write(|w| w.bits(now.wrapping_add(micros)));
                timer.inte().modify(|r, w| w.bits(r.bits() | TIMER_ALARM));
            }
        }
    }
}

// The alarm went off: raise IF for the timers that overflowed and arm the next one
pub fn timer_alarm() {
    unsafe {
        (*pac::TIMER::ptr()).intr().write(|w| w.bits(TIMER_ALARM));
        TIMER_STATS[1] += 1;
        let due = TIMERS.take_due(gba_cycles());
        if due != 0 {
            TIMER_STATS[2] += 1;
            TIMER_STATS[3] = due as u32;
            let flags = (&raw mut NEW_REG as u32 + IF_OFFSET) as *mut u16;
            core::ptr::write_volatile(flags, core::ptr::read_volatile(flags) | (due as u16) << 3);
        }
    }
    timer_arm();
}

fn open_bus(addr: u32, size: u32) -> u32 {
    let doubled = BIOS_OPEN_BUS as u64 | (BIOS_OPEN_BUS as u64) << 32;
    let word = (doubled >> (8 * (addr & 3))) as u32;
    match size {
        1 => word & 0xFF,
        2 => word & 0xFFFF,
        _ => word,
    }
}

fn ewram_load(addr: u32, size: u32) -> u32 {
    unsafe {
        if ewram_page_empty(addr) {
            // nothing but zero has been stored there
            return 0;
        }
        match ewram_native(addr, false) {
            Some(native) => read_native(native, size),
            None => 0,
        }
    }
}

fn ewram_store(addr: u32, size: u32, value: u32) {
    let mask = u32::MAX >> (32 - 8 * size);
    // storing zero to a page that is not there changes nothing
    if ewram_page_empty(addr) && value & mask == 0 {
        return;
    }
    if let Some(native) = ewram_native(addr, true) {
        unsafe { write_native(native, size, value) };
    }
}

fn io_load(addr: u32, size: u32) -> u32 {
    let offset = addr & 0x00FF_FFFF;
    if offset >= REG_LEN {
        return open_bus(addr, size);
    }
    unsafe {
        IO_LOADS[IO_COUNTS[0] as usize % 8] = offset;
        IO_COUNTS[0] += 1;
        let value = read_native((&raw const NEW_REG as u32) + offset, size);
        if (TIMER_BASE..TIMER_END).contains(&offset) {
            let v = TIMERS.read(offset, size, value, gba_cycles());
            return v;
        }
        value
    }
}

fn io_store(addr: u32, size: u32, value: u32) {
    let offset = addr & 0x00FF_FFFF;
    if offset >= REG_LEN {
        return;
    }
    unsafe {
        IO_STORES[IO_COUNTS[1] as usize % 8] = offset | (value & 0xFFFF) << 16;
        IO_COUNTS[1] += 1;
        let native = (&raw mut NEW_REG as u32) + offset;
        // IF is cleared by writing ones to it
        let flags = (&raw mut NEW_REG as u32 + IF_OFFSET) as *mut u16;
        let old_flags = core::ptr::read_volatile(flags);
        write_native(native, size, value);
        if offset <= IF_OFFSET && offset + size > IF_OFFSET {
            let written = (value >> (8 * (IF_OFFSET - offset))) as u16;
            core::ptr::write_volatile(flags, old_flags & !written);
        }
        if (TIMER_BASE..TIMER_END).contains(&offset) {
            TIMERS.write(offset, size, value, gba_cycles());
            timer_arm();
        } else if let Some(channel) = Dma::started_by(offset, size) {
            run_dma(channel);
        }
    }
}

// Run a DMA channel that starts at once. The others (VBlank, HBlank, sound) are not emulated.
fn run_dma(channel: usize) {
    unsafe {
        let regs = ((&raw const NEW_REG as u32) + 0xB0 + 12 * channel as u32) as *const u32;
        let dma = Dma::from_registers(
            channel,
            core::ptr::read_volatile(regs),
            core::ptr::read_volatile(regs.add(1)),
            core::ptr::read_volatile(regs.add(2)),
        );
        if !dma.enabled() || !dma.immediate() {
            return;
        }
        dma.run(&mut Gba);
        // the transfer is done, so the channel is off
        let control = regs.add(2) as *mut u16;
        let control = control.add(1);
        core::ptr::write_volatile(control, core::ptr::read_volatile(control) & 0x7FFF);
    }
}

fn rom_load(addr: u32, size: u32) -> u32 {
    let offset = (addr - ROM_START) & 0x01FF_FFFF;
    // past the end of the ROM there is nothing to read
    match romstore::rom_size() {
        Some(Some(len)) if offset + size > len => return open_bus(addr, size),
        _ => {}
    }
    let Some(cache) = (unsafe { ROM_CACHE.as_mut() }) else {
        return open_bus(addr, size);
    };
    match size {
        1 => cache.read8(&mut SdRom, offset) as u32,
        2 => cache.read16(&mut SdRom, offset) as u32,
        _ => cache.read32(&mut SdRom, offset),
    }
}

fn sram_load(addr: u32, size: u32) -> u32 {
    let offset = addr & 0xFFFF;
    let byte = unsafe {
        match FLASH.read(offset) {
            Some(id) => id,
            None if offset < SRAM_LEN => {
                core::ptr::read_volatile(((&raw const NEW_SRAM as u32) + offset) as *const u8)
            }
            None => 0xFF,
        }
    };
    // the cartridge has an 8 bit bus: wider reads see the byte repeated
    (byte as u32).wrapping_mul(match size {
        1 => 1,
        2 => 0x0101,
        _ => 0x0101_0101,
    })
}

fn sram_store(addr: u32, size: u32, value: u32) {
    let offset = addr & 0xFFFF;
    let _ = size;
    unsafe {
        FLASH.write(offset, value as u8);
        if offset < SRAM_LEN {
            core::ptr::write_volatile(
                ((&raw mut NEW_SRAM as u32) + offset) as *mut u8,
                value as u8,
            );
        }
    }
}

pub fn load(addr: u32, size: u32) -> u32 {
    unsafe {
        LOAD_ADDRS[ADDR_COUNTS[0] as usize % 8] = addr;
        ADDR_COUNTS[0] += 1;
        match addr >> 24 {
            0x00 if addr < BIOS_LEN => open_bus(addr, size),
            0x02 => ewram_load(addr, size),
            0x03 => read_native(
                (&raw const NEW_IWRAM as u32) + ((addr - IWRAM_START) & (IWRAM_LEN - 1)),
                size,
            ),
            0x04 => io_load(addr, size),
            0x05 => read_native(
                (&raw const NEW_PALETTE as u32) + (addr & (PALETTE_LEN - 1)),
                size,
            ),
            0x06 if addr & 0x00FF_FFFF < VRAM_LEN => {
                read_native((&raw const NEW_VRAM as u32) + (addr & 0x00FF_FFFF), size)
            }
            0x07 => read_native((&raw const NEW_OAM as u32) + (addr & (OAM_LEN - 1)), size),
            0x0D => EEPROM.read_bit(),
            0x08..=0x0D => rom_load(addr, size),
            0x0E | 0x0F => sram_load(addr, size),
            // native memory: the stack, or code the game copied there
            0x20 => read_native(addr, size),
            _ => open_bus(addr, size),
        }
    }
}

pub fn store(addr: u32, size: u32, value: u32) {
    unsafe {
        STORE_ADDRS[ADDR_COUNTS[1] as usize % 8] = addr;
        ADDR_COUNTS[1] += 1;
        match addr >> 24 {
            0x02 => ewram_store(addr, size, value),
            0x03 => write_native(
                (&raw mut NEW_IWRAM as u32) + ((addr - IWRAM_START) & (IWRAM_LEN - 1)),
                size,
                value,
            ),
            0x04 => io_store(addr, size, value),
            0x05 => write_native(
                (&raw mut NEW_PALETTE as u32) + (addr & (PALETTE_LEN - 1)),
                size,
                value,
            ),
            0x06 if addr & 0x00FF_FFFF < VRAM_LEN => write_native(
                (&raw mut NEW_VRAM as u32) + (addr & 0x00FF_FFFF),
                size,
                value,
            ),
            0x07 => write_native(
                (&raw mut NEW_OAM as u32) + (addr & (OAM_LEN - 1)),
                size,
                value,
            ),
            0x0D => EEPROM.write_bit(value),
            0x0E | 0x0F => sram_store(addr, size, value),
            0x20 => write_native(addr, size, value),
            // the BIOS, the ROM and the holes ignore writes
            _ => {}
        }
    }
}

// The memory the BIOS calls and DMA work in
pub struct Gba;

impl Memory for Gba {
    fn read8(&mut self, addr: u32) -> u8 {
        load(addr, 1) as u8
    }
    fn read16(&mut self, addr: u32) -> u16 {
        load(addr & !1, 2) as u16
    }
    fn read32(&mut self, addr: u32) -> u32 {
        load(addr & !3, 4)
    }
    fn write8(&mut self, addr: u32, value: u8) {
        store(addr, 1, value as u32)
    }
    fn write16(&mut self, addr: u32, value: u16) {
        store(addr & !1, 2, value as u32)
    }
    fn write32(&mut self, addr: u32, value: u32) {
        store(addr & !3, 4, value)
    }
}
