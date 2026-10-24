// The pico-gba runtime on the host: GBA memory, the code cache, the fault model, BIOS calls and
// interrupts, mirroring firmware/src/main.rs. Time is counted in Cortex-M0+ cycles and only moves
// when the interpreter runs, so the GBA state does not depend on how fast the JIT code is.
use crate::cpu::{Access, Bus, Cpu, Event};
use common::jit::{Jit, MapEntry, STUB_TRAP, TRAP};
use common::romcache::{PAGE_SIZE, RomCache, RomSource};
use std::collections::HashMap;
use std::time::Instant;

// native RAM: the code cache is at its start, the stack at its end
pub const NATIVE_BASE: u32 = 0x2000_0000;
pub const NATIVE_LEN: u32 = 0x42000;
const BIOS_SIZE: usize = 0x4000;
// what the last BIOS fetch leaves on the bus after startup
const BIOS_OPEN_BUS: u32 = 0xE129_F000;
pub const CACHE_BASE: u32 = NATIVE_BASE;
// as in the firmware: 24 KiB of code and a 1024 entry block map
pub const CACHE_HW: usize = 64 * 1024;
pub const MAP_LEN: usize = 4096;
// the cycles a ROM page cache miss stalls for: an SD block read at 20 MHz and the command and
// wait for the other core, about 0.6 ms
const ROM_MISS_CYCLES: u64 = 75_000;
// free code cache (in halfwords) below which a translation outside the interrupt handler
// flushes the cache first
const FLUSH_MARGIN_HW: usize = 4 * 1024;
const FLUSH_MARGIN_BLOCKS: usize = 100;

// The region offset table the translated code reads, after the code cache. A GBA address a that
// is flat in native memory is reached at a + table[a >> 24], in a window the interpreter treats
// as a non-faulting view of the GBA region. EWRAM and the BIOS have no entry, they still fault.
pub const REGIONS_ADDR: u32 = NATIVE_BASE + 0x20000;
// the page tables of EWRAM and IWRAM (512 entries each) follow the region table
// the indirect jump dispatch table of the translated Thumb code
pub const DISPATCH_ADDR: u32 = REGIONS_ADDR + 0x1400;
const EWRAM_PT: u32 = REGIONS_ADDR + 0x400;
const IWRAM_PT: u32 = REGIONS_ADDR + 0xC00;
const ALIAS: u32 = 0x4000_0000;

pub const ROM_BASE: u32 = 0x0800_0000;
const ROM_LEN: usize = 20 << 20;
const IWRAM_SIZE: usize = 32 << 10;
const EWRAM_SIZE: usize = 256 << 10;
const VRAM_SIZE: usize = 96 << 10;
const SRAM_SIZE: usize = 64 << 10;

// Extra cycles for a data access the fault handler emulates (3.5 us at 125 MHz plus exception
// entry and exit) and for a trap into the translator
pub const FAULT_CYCLES: u64 = 465;
pub const TRAP_CYCLES: u64 = 465;
// the SVC exception entry/exit and handler, and what a CpuSet/CpuFastSet unit costs on top (the
// handler copies natively, it does not fault per unit)
pub const SVC_CYCLES: u64 = 100;
pub const CPU_SET_UNIT_CYCLES: u64 = 20;
// the irq trampoline saving and restoring everything
pub const TRAMPOLINE_CYCLES: u64 = 60;
// a frame longer than this is cut off and counted as a warning
pub const FRAME_CAP: u64 = 20_000_000;

const IE: usize = 0x200;
const IF: usize = 0x202;
const IME: usize = 0x208;
const DISPSTAT: usize = 0x004;
const VCOUNT: usize = 0x006;
const KEYINPUT: usize = 0x130;
const IRQ_VECTOR: usize = 0x7FFC;
const BIOS_IF: usize = 0x7FF8;
// the return address of the game's irq handler, it is never executed
const MAGIC: u32 = 0xFFFF_FF00;

// Backing store aligned to 4 bytes, the video code reads it through structs
pub struct Region(Vec<u32>);

impl Region {
    fn new(bytes: usize) -> Self {
        Region(vec![0; bytes.div_ceil(4)])
    }
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0.as_ptr() as *const u8, self.0.len() * 4) }
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, self.0.len() * 4) }
    }
}

#[derive(Clone, Copy)]
enum Which {
    Bios,
    Ewram,
    Iwram,
    Io,
    Palette,
    Vram,
    Oam,
    Rom,
    Sram,
}

pub struct Mem {
    // The BIOS is not there. Code outside it reads the last word the BIOS fetched, which is the
    // same value for every address, and games that follow a null pointer rely on that: both ends
    // of an empty std::vector read the same word.
    bios: Vec<u8>,
    pub ewram: Region,
    pub iwram: Region,
    pub io: Region,
    pub palette: Region,
    pub vram: Region,
    pub oam: Region,
    pub sram: Region,
    rom: Vec<u8>,
    native: Vec<u8>,
}

impl Mem {
    fn new(rom: &[u8]) -> Self {
        let mut m = Mem {
            ewram: Region::new(EWRAM_SIZE),
            bios: BIOS_OPEN_BUS.to_le_bytes().repeat(BIOS_SIZE / 4),
            iwram: Region::new(IWRAM_SIZE),
            io: Region::new(0x400),
            palette: Region::new(0x400),
            vram: Region::new(VRAM_SIZE),
            oam: Region::new(0x400),
            sram: Region::new(SRAM_SIZE),
            rom: vec![0; ROM_LEN],
            native: vec![0; NATIVE_LEN as usize],
        };
        m.rom[..rom.len()].copy_from_slice(rom);
        m.sram.bytes_mut().fill(0xFF);
        let mut put = |at: u32, v: u32| {
            let at = (at - NATIVE_BASE) as usize;
            m.native[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        for (top, offset) in region_table().into_iter().enumerate() {
            put(REGIONS_ADDR + 4 * top as u32, offset);
        }
        for i in 0..512u32 {
            // EWRAM pages are not there yet: they point at their own GBA address and fault
            put(EWRAM_PT + 4 * i, 0x0200_0000 + i * 512);
            // the 32 KiB of IWRAM repeat through the window
            put(IWRAM_PT + 4 * i, ALIAS + 0x0300_0000 + (i % 64) * 512);
        }
        m
    }

    // Where a GBA address lives, with the same mirroring and holes as fix_addr in the firmware
    fn locate(addr: u32) -> Option<(Which, usize)> {
        let off = (addr & 0x00FF_FFFF) as usize;
        match addr >> 24 {
            0 if off < BIOS_SIZE => Some((Which::Bios, off)),
            2 => Some((Which::Ewram, off & (EWRAM_SIZE - 1))),
            3 => Some((Which::Iwram, off & (IWRAM_SIZE - 1))),
            4 if off < 0x400 => Some((Which::Io, off)),
            5 if off < 0x400 => Some((Which::Palette, off)),
            6 if off < VRAM_SIZE => Some((Which::Vram, off)),
            7 if off < 0x400 => Some((Which::Oam, off)),
            8 | 9 => {
                let off = (addr - ROM_BASE) as usize;
                (off < ROM_LEN).then_some((Which::Rom, off))
            }
            0xE if off < SRAM_SIZE => Some((Which::Sram, off)),
            _ => None,
        }
    }

    fn bytes(&self, w: Which) -> &[u8] {
        match w {
            Which::Bios => &self.bios,
            Which::Ewram => self.ewram.bytes(),
            Which::Iwram => self.iwram.bytes(),
            Which::Io => self.io.bytes(),
            Which::Palette => self.palette.bytes(),
            Which::Vram => self.vram.bytes(),
            Which::Oam => self.oam.bytes(),
            Which::Rom => &self.rom,
            Which::Sram => self.sram.bytes(),
        }
    }

    // None for ROM, which ignores writes
    fn bytes_mut(&mut self, w: Which) -> Option<&mut [u8]> {
        Some(match w {
            Which::Ewram => self.ewram.bytes_mut(),
            Which::Iwram => self.iwram.bytes_mut(),
            Which::Io => self.io.bytes_mut(),
            Which::Palette => self.palette.bytes_mut(),
            Which::Vram => self.vram.bytes_mut(),
            Which::Oam => self.oam.bytes_mut(),
            Which::Rom | Which::Bios => return None,
            Which::Sram => self.sram.bytes_mut(),
        })
    }

    // Little endian and byte exact, so unaligned accesses work like the firmware's emulation
    pub fn read(&self, addr: u32, size: u32) -> Option<u32> {
        let (w, off) = Self::locate(addr)?;
        let b = self.bytes(w).get(off..off + size as usize)?;
        Some(match size {
            1 => b[0] as u32,
            2 => u16::from_le_bytes([b[0], b[1]]) as u32,
            _ => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        })
    }

    // false when the address is not mapped
    pub fn write(&mut self, addr: u32, size: u32, value: u32) -> bool {
        let Some((w, off)) = Self::locate(addr) else {
            return false;
        };
        let Some(b) = self.bytes_mut(w) else {
            return true;
        };
        match b.get_mut(off..off + size as usize) {
            Some(dst) => {
                dst.copy_from_slice(&value.to_le_bytes()[..size as usize]);
                true
            }
            None => false,
        }
    }

    // VCOUNT, with the DISPSTAT VBlank flag following it (lines 160-226) as in the firmware
    pub fn set_vcount(&mut self, line: u16) {
        self.set_io16(VCOUNT, line);
        let d = self.io16(DISPSTAT) & !1;
        self.set_io16(DISPSTAT, d | (160..227).contains(&line) as u16);
    }

    pub fn io16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.io.bytes()[off], self.io.bytes()[off + 1]])
    }

    pub fn set_io16(&mut self, off: usize, v: u16) {
        self.io.bytes_mut()[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn iwram_vector(&self) -> u32 {
        self.iwram32(IRQ_VECTOR)
    }

    // what a crt0 that is skipped would have stored for the interrupt handler
    pub fn set_irq_vector(&mut self, handler: u32) {
        self.iwram.bytes_mut()[IRQ_VECTOR..IRQ_VECTOR + 4].copy_from_slice(&handler.to_le_bytes());
    }

    fn iwram16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.iwram.bytes()[off], self.iwram.bytes()[off + 1]])
    }

    fn set_iwram16(&mut self, off: usize, v: u16) {
        self.iwram.bytes_mut()[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn iwram32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.iwram.bytes()[off..off + 4].try_into().unwrap())
    }
}

// The entries of the table at REGIONS_ADDR, indexed by the top byte of the address: offsets, or
// page tables (odd) for the mirrored EWRAM and IWRAM. The ROM mirrors land on the ROM window.
fn region_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for top in [4u32, 5, 6, 7, 8, 9, 0xE] {
        t[top as usize] = ALIAS;
    }
    t[2] = EWRAM_PT | 1;
    t[3] = IWRAM_PT | 1;
    for (top, rom) in [(0xAu32, 0x8u32), (0xB, 0x9), (0xC, 0x8), (0xD, 0x9)] {
        t[top as usize] = ALIAS.wrapping_add((rom - top) << 24);
    }
    t
}

// the region names used in the fault report, indexed by region_of
pub const REGION_NAMES: [&str; 9] = [
    "ewram", "iwram", "io", "palette", "vram", "oam", "rom", "sram", "other",
];

fn region_of(addr: u32) -> usize {
    match addr >> 24 {
        2 => 0,
        3 => 1,
        4 => 2,
        5 => 3,
        6 => 4,
        7 => 5,
        8..=0xD => 6,
        0xE => 7,
        _ => 8,
    }
}

// An LRU cache of ROM pages, to see how many misses a given amount of RAM would have
pub struct LruSim {
    pub pages: usize,
    pub shift: u32,
    // page -> last use stamp
    map: HashMap<u32, u64>,
    stamp: u64,
    pub misses_by_frame: Vec<u32>,
}

impl LruSim {
    pub fn new(kb: usize, shift: u32) -> Self {
        LruSim {
            pages: kb * 1024 >> shift,
            shift,
            map: HashMap::new(),
            stamp: 0,
            misses_by_frame: Vec::new(),
        }
    }

    pub fn access(&mut self, addr: u32, frame: u64) {
        let page = addr >> self.shift;
        self.stamp += 1;
        if self.map.insert(page, self.stamp).is_none() {
            while self.misses_by_frame.len() <= frame as usize {
                self.misses_by_frame.push(0);
            }
            self.misses_by_frame[frame as usize] += 1;
            if self.map.len() > self.pages {
                let (&victim, _) = self.map.iter().min_by_key(|(_, t)| **t).unwrap();
                self.map.remove(&victim);
            }
        }
    }
}

// An EWRAM page pool of `slots` pages of 1 << shift bytes with LRU eviction, for judging paging
// to the SD card: pages that were never written non-zero are free (they read as zero), the others
// need a slot, and one that was evicted is read back from the card when it is used again.
pub struct EwramSim {
    pub shift: u32,
    pub slots: usize,
    backed: Vec<bool>,
    in_pool: Vec<bool>,
    stamp: Vec<u64>,
    dirty: Vec<bool>,
    // the page has a copy on the card
    swapped: Vec<bool>,
    clock: u64,
    resident: usize,
    pub reads_by_frame: Vec<u32>,
    pub writebacks_by_frame: Vec<u32>,
}

impl EwramSim {
    pub fn new(shift: u32, kb: usize) -> Self {
        let pages = EWRAM_SIZE >> shift;
        EwramSim {
            shift,
            slots: kb * 1024 >> shift,
            backed: vec![false; pages],
            in_pool: vec![false; pages],
            stamp: vec![0; pages],
            dirty: vec![false; pages],
            swapped: vec![false; pages],
            clock: 0,
            resident: 0,
            reads_by_frame: Vec::new(),
            writebacks_by_frame: Vec::new(),
        }
    }

    fn access(&mut self, off: usize, size: u32, write: bool, value: u32, frame: usize) {
        let first = off >> self.shift;
        let last = ((off + size as usize - 1) & (EWRAM_SIZE - 1)) >> self.shift;
        for page in [first, last].into_iter().take(1 + (first != last) as usize) {
            if write && value != 0 {
                self.backed[page] = true;
            }
            if self.backed[page] {
                self.touch(page, write, frame);
            }
        }
    }

    fn touch(&mut self, page: usize, write: bool, frame: usize) {
        self.clock += 1;
        if self.in_pool[page] {
            self.stamp[page] = self.clock;
            self.dirty[page] |= write;
            return;
        }
        while self.reads_by_frame.len() <= frame {
            self.reads_by_frame.push(0);
            self.writebacks_by_frame.push(0);
        }
        if self.swapped[page] {
            self.reads_by_frame[frame] += 1;
        }
        if self.resident == self.slots {
            let victim = (0..self.in_pool.len())
                .filter(|&i| self.in_pool[i])
                .min_by_key(|&i| self.stamp[i])
                .unwrap();
            self.in_pool[victim] = false;
            self.resident -= 1;
            if self.dirty[victim] {
                self.writebacks_by_frame[frame] += 1;
                self.swapped[victim] = true;
            }
        }
        self.in_pool[page] = true;
        self.resident += 1;
        self.stamp[page] = self.clock;
        // a page read back is clean, a new one only exists in the pool
        self.dirty[page] = write || !self.swapped[page];
    }
}

pub const EWRAM_PAGES: usize = EWRAM_SIZE / 512;

// What the game does with its EWRAM, page by page
pub struct EwramStats {
    // a page that has been written a non-zero value, so it needs a slot
    pub backed: Vec<bool>,
    pub reads: Vec<u64>,
    pub writes: Vec<u64>,
    // the frame of the last access and of the last write
    pub last_access: Vec<u64>,
    pub last_write: Vec<u64>,
    // distinct backed pages touched in each frame
    pub touched_by_frame: Vec<u32>,
    last_frame: Vec<i64>,
    pub sims: Vec<EwramSim>,
}

impl Default for EwramStats {
    fn default() -> Self {
        Self::new()
    }
}

impl EwramStats {
    pub fn new() -> Self {
        EwramStats {
            backed: vec![false; EWRAM_PAGES],
            reads: vec![0; EWRAM_PAGES],
            writes: vec![0; EWRAM_PAGES],
            last_access: vec![0; EWRAM_PAGES],
            last_write: vec![0; EWRAM_PAGES],
            touched_by_frame: Vec::new(),
            last_frame: vec![-1; EWRAM_PAGES],
            sims: [7, 8, 9, 10, 11]
                .into_iter()
                .flat_map(|shift| [8, 12, 16, 24].map(|kb| EwramSim::new(shift, kb)))
                .collect(),
        }
    }

    // an access of size bytes at the EWRAM offset (already mirrored); value is what is stored
    pub fn access(&mut self, off: usize, size: u32, write: bool, value: u32, frame: u64) {
        // an access can straddle two pages, count both
        let first = off / 512;
        let last = ((off + size as usize - 1) & (EWRAM_SIZE - 1)) / 512;
        for page in [first, last].into_iter().take(1 + (first != last) as usize) {
            if write {
                self.writes[page] += 1;
                self.last_write[page] = frame;
                if value != 0 {
                    self.backed[page] = true;
                }
            } else {
                self.reads[page] += 1;
            }
            if !self.backed[page] {
                continue;
            }
            self.last_access[page] = frame;
            if self.last_frame[page] != frame as i64 {
                self.last_frame[page] = frame as i64;
                while self.touched_by_frame.len() <= frame as usize {
                    self.touched_by_frame.push(0);
                }
                self.touched_by_frame[frame as usize] += 1;
            }
        }
        for sim in &mut self.sims {
            sim.access(off, size, write, value, frame as usize);
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct FaultPc {
    pub count: u64,
    pub regions: [u64; 9],
}

// one translated block, in code cache halfwords from the start of the cache
#[derive(Clone, Copy)]
pub struct BlockInfo {
    pub arm: u32,
    pub arm_end: u32,
    pub hw_start: usize,
    pub hw_end: usize,
}

#[derive(Default)]
pub struct Stats {
    // instruction cycles by where the pc was
    pub native_cycles: u64,
    pub jit_cycles: u64,
    // faults by where the pc was: 0 native code, 1 code cache
    pub fault_count: [u64; 2],
    pub fault_cycles: [u64; 2],
    pub fault_pcs: HashMap<u32, FaultPc>,
    // loads and stores translated inline by the jit, by region_of (an ldm/stm counts once)
    pub inline_count: [u64; 9],
    // EWRAM pages the fault handler paged in
    pub ewram_pages: u64,
    // traps into the translator: 0 unresolved exit slot, 1 jump to ARM code, 2 bx pc veneer
    pub trap_count: [u64; 3],
    pub trap_cycles: u64,
    pub svc_count: u64,
    pub svc_cycles: u64,
    pub tramp_count: u64,
    pub tramp_cycles: u64,
    // cycles of each frame
    pub frames: Vec<u64>,
    pub capped_frames: u64,
    // per code cache halfword: instruction cycles, and fault + trap cycles
    pub cache_instr: Vec<u64>,
    // the frame each halfword of the cache last ran in, to measure the working set
    pub last_run: Vec<u32>,
    pub cache_other: Vec<u64>,
    pub blocks: Vec<BlockInfo>,
    pub translate_ns: u64,
    pub flushes: u64,
    pub rom_miss_cycles: u64,
    // ROM page cache misses in each frame
    pub rom_misses_by_frame: Vec<u32>,
    // bytes translated inside the interrupt handler since the last flush, and the most of those
    pub irq_bytes: usize,
    pub irq_bytes_max: usize,
    // ROM pages (of 512 bytes) fetched as code and read as data, with the frame of the last use
    pub code_pages: HashMap<u32, u64>,
    pub caches: Vec<LruSim>,
    pub data_pages: HashMap<u32, u64>,
    pub arm_instrs: u64,
    pub ewram: EwramStats,
}

#[derive(Clone, Copy)]
pub struct InputScript {
    pub start: u64,
    pub every: u64,
    pub len: u64,
}

impl InputScript {
    // is A held during this frame
    fn pressed(&self, frame: u64) -> bool {
        self.every != 0 && frame >= self.start && (frame - self.start) % self.every < self.len
    }
}

// everything the interpreter can reach
pub struct Sys {
    pub mem: Mem,
    pub jit: Jit<'static>,
    pub stats: Stats,
    cur_pc: u32,
    trace: bool,
    vblank_polled: bool,
    frame_cycles: u64,
    frame_no: u64,
    input: InputScript,
    in_irq: bool,
    // regions of the code cache are thrown away one at a time
    ring: bool,
    cur_regs: [u32; 16],
    irq_ret: u32,
    paging: Option<Paging>,
    game_code: [u8; 4],
    // cycles since the start, the timers count from it
    total_cycles: u64,
    timers: common::timers::Timers,
    flash: common::flash::FlashChip,
}

// The ROM as the SD card holds it, pages are copied out of it on a miss
struct VecSource(Vec<u8>);

impl RomSource for VecSource {
    fn read_page(&mut self, offset: u32, dst: &mut [u8; PAGE_SIZE]) {
        let at = offset as usize;
        let n = self.0.len().saturating_sub(at).min(PAGE_SIZE);
        dst[..n].copy_from_slice(&self.0[at..at + n]);
        dst[n..].fill(0);
    }
}

// ROM reads go through a cache of pages of RAM, like the firmware will have
struct Paging {
    cache: RomCache<'static>,
    src: VecSource,
}

// adapter for the BIOS calls and DMA: GBA addresses in, bytes of the model out
struct GbaMem<'a>(&'a mut Sys);

impl common::bios::Memory for GbaMem<'_> {
    fn read8(&mut self, a: u32) -> u8 {
        self.0.raw_read(a, 1) as u8
    }
    fn read16(&mut self, a: u32) -> u16 {
        self.0.raw_read(a & !1, 2) as u16
    }
    fn read32(&mut self, a: u32) -> u32 {
        self.0.raw_read(a & !3, 4)
    }
    fn write8(&mut self, a: u32, v: u8) {
        self.0.raw_write(a, 1, v as u32)
    }
    fn write16(&mut self, a: u32, v: u16) {
        self.0.raw_write(a & !1, 2, v as u32)
    }
    fn write32(&mut self, a: u32, v: u32) {
        self.0.raw_write(a & !3, 4, v)
    }
}

impl Sys {
    fn in_cache(pc: u32) -> bool {
        pc.wrapping_sub(CACHE_BASE) < 2 * CACHE_HW as u32
    }

    fn die(&self, msg: &str) -> ! {
        let mut at = String::new();
        if Self::in_cache(self.cur_pc) {
            let hw = ((self.cur_pc - CACHE_BASE) / 2) as usize;
            if let Some(b) = self
                .stats
                .blocks
                .iter()
                .find(|b| (b.hw_start..b.hw_end).contains(&hw))
            {
                at = format!(", in the block for ARM {:#x}-{:#x}", b.arm, b.arm_end);
            }
        }
        // JITBENCH_DUMP_DIR gets ewram, iwram and native (the stack) as they were when the run died
        if let Some(dir) = std::env::var_os("JITBENCH_DUMP_DIR") {
            let dir = std::path::Path::new(&dir);
            let _ = std::fs::write(dir.join("ewram"), self.mem.ewram.bytes());
            let _ = std::fs::write(dir.join("iwram"), self.mem.iwram.bytes());
            let _ = std::fs::write(dir.join("native"), &self.mem.native);
        }
        panic!("{msg} (pc {:#x}{at}, frame {})", self.cur_pc, self.frame_no);
    }

    fn charge_instr(&mut self, pc: u32, cycles: u32) {
        let c = cycles as u64;
        self.frame_cycles += c;
        self.total_cycles += c;
        if Self::in_cache(pc) {
            self.stats.jit_cycles += c;
            self.stats.cache_instr[((pc - CACHE_BASE) / 2) as usize] += c;
            self.stats.last_run[((pc - CACHE_BASE) / 2) as usize] = self.frame_no as u32;
        } else {
            self.stats.native_cycles += c;
        }
    }

    // cycles that belong to the instruction at pc but are not the instruction itself
    fn charge_other(&mut self, pc: u32, c: u64) {
        self.frame_cycles += c;
        self.total_cycles += c;
        if Self::in_cache(pc) {
            self.stats.cache_other[((pc - CACHE_BASE) / 2) as usize] += c;
        }
    }

    // EWRAM accesses by page, whoever makes them (the cpu, dma, a BIOS call)
    fn note_ewram(&mut self, addr: u32, size: u32, write: bool, value: u32) {
        if addr >> 24 == 2 {
            let off = (addr & 0x00FF_FFFF) as usize & (EWRAM_SIZE - 1);
            let frame = self.frame_no;
            self.stats.ewram.access(off, size, write, value, frame);
        }
    }

    fn note_fault(&mut self, addr: u32) {
        let pc = self.cur_pc;
        let side = Self::in_cache(pc) as usize;
        self.stats.fault_count[side] += 1;
        self.stats.fault_cycles[side] += FAULT_CYCLES;
        let e = self.stats.fault_pcs.entry(pc).or_default();
        e.count += 1;
        e.regions[region_of(addr)] += 1;
        self.charge_other(pc, FAULT_CYCLES);
        // the handler pages the EWRAM page in, later accesses through the table do not fault
        if addr >> 24 == 2 {
            let at = (EWRAM_PT - NATIVE_BASE + 4 * ((addr >> 9) & 0x1FF)) as usize;
            let page = ALIAS + 0x0200_0000 + (addr & 0x00FF_FE00 & (EWRAM_SIZE as u32 - 1));
            if self.mem.native[at..at + 4] != page.to_le_bytes() {
                self.mem.native[at..at + 4].copy_from_slice(&page.to_le_bytes());
                self.stats.ewram_pages += 1;
            }
        }
    }

    // The GBA address behind an access through an alias window, and whether it was one
    fn unalias(&mut self, addr: u32, size: u32, kind: Access) -> (u32, bool) {
        if addr >> 28 != 4 {
            return (addr, false);
        }
        let gba = addr - ALIAS;
        // IWRAM and EWRAM go through page tables, a flat access that leaves its window means
        // the table was built wrong
        let (top, off) = (gba >> 24, (gba & 0x00FF_FFFF) + size);
        if (top == 3 && off > IWRAM_SIZE as u32) || (top == 2 && off > EWRAM_SIZE as u32) {
            self.die(&format!("inline access left its region at {gba:#x}"));
        }
        if kind == Access::Data {
            self.stats.inline_count[region_of(gba)] += 1;
        }
        (gba, true)
    }

    // Memory as the BIOS and DMA see it: GBA addresses, or native ones for the stack
    fn raw_read(&mut self, addr: u32, size: u32) -> u32 {
        self.note_ewram(addr, size, false, 0);
        if let Some(v) = self.mem_read(addr, size) {
            return v;
        }
        match Self::native_off(addr, size) {
            Some(off) => self.read_native(off, size),
            // open bus
            None => 0,
        }
    }

    fn raw_write(&mut self, addr: u32, size: u32, value: u32) {
        self.note_ewram(addr, size, true, value);
        if !self.mem.write(addr, size, value) {
            if let Some(off) = Self::native_off(addr, size) {
                for i in 0..size {
                    self.mem.native[(off + i) as usize] = (value >> (8 * i)) as u8;
                }
            }
        }
    }

    // writes to IO registers with side effects: timers and DMA
    fn io_write(&mut self, io: u32, size: u32, value: u32) {
        if std::env::var_os("JITBENCH_LOG_IO").is_some() && self.frame_no < 3 {
            eprintln!(
                "io write {io:#x} size {size} value {value:#x} at pc {:#x}",
                self.cur_pc
            );
        }
        match io {
            0x100..=0x10F => {
                let now = self.total_cycles;
                self.timers.write(io, size, value, now);
            }
            0xB0..=0xDF => {
                if let Some(ch) = common::dma::Dma::started_by(io, size) {
                    self.dma_start(ch);
                }
            }
            _ => {}
        }
    }

    fn io_read(&self, io: u32, size: u32, value: u32) -> u32 {
        if (0x100..=0x10F).contains(&io) {
            return self.timers.read(io, size, value, self.total_cycles);
        }
        value
    }

    // Immediate DMA runs right away. Other start times (VBlank, HBlank, sound FIFO) are left.
    fn dma_start(&mut self, ch: usize) {
        let base = 0xB0 + 12 * ch;
        let io = |m: &Mem, off: usize| {
            u32::from_le_bytes(m.io.bytes()[off..off + 4].try_into().unwrap())
        };
        let dma = common::dma::Dma::from_registers(
            ch,
            io(&self.mem, base),
            io(&self.mem, base + 4),
            io(&self.mem, base + 8),
        );
        if !dma.enabled() || !dma.immediate() {
            return;
        }
        let count = dma.run(&mut GbaMem(self));
        self.charge_other(self.cur_pc, count as u64 * 2);
        // done, the enable bit clears
        let off = base + 10;
        let b = self.mem.io.bytes_mut();
        let c = u16::from_le_bytes([b[off], b[off + 1]]) & 0x7FFF;
        b[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }

    fn native_off(addr: u32, size: u32) -> Option<u32> {
        let off = addr.wrapping_sub(NATIVE_BASE);
        (off <= NATIVE_LEN - size).then_some(off)
    }

    fn read_native(&self, off: u32, size: u32) -> u32 {
        let mut v = 0;
        for i in 0..size {
            let a = off + i;
            let byte = if a < 2 * CACHE_HW as u32 {
                (self.jit.halfword(CACHE_BASE + (a & !1)) >> (8 * (a & 1))) as u8
            } else {
                self.mem.native[a as usize]
            };
            v |= (byte as u32) << (8 * i);
        }
        v
    }

    // Read GBA memory for the translator, like gba_read_word
    // The translator also reads the region table and the page tables (native memory) here, to
    // find the address behind a constant base register.
    fn gba_word(
        mem: &Mem,
        paging: &mut Option<Paging>,
        game_code: [u8; 4],
        addr: u32,
    ) -> Option<u32> {
        Self::gba_word_raw(mem, paging, addr).map(|w| common::patches::word(game_code, addr, w))
    }

    fn gba_word_raw(mem: &Mem, paging: &mut Option<Paging>, addr: u32) -> Option<u32> {
        if addr >= REGIONS_ADDR && addr < REGIONS_ADDR + 0x1400 {
            let at = (addr - NATIVE_BASE) as usize;
            return Some(u32::from_le_bytes(
                mem.native[at..at + 4].try_into().unwrap(),
            ));
        }
        if let (Some(p), 8..=0xD) = (paging.as_mut(), addr >> 24) {
            return Some(p.cache.read32(&mut p.src, addr & 0x01FF_FFFC));
        }
        mem.read(addr, 4)
    }

    // A read of GBA memory, ROM through the page cache when there is one. The cycles a miss
    // stalls for are charged.
    fn mem_read(&mut self, addr: u32, size: u32) -> Option<u32> {
        if let (Some(p), 8..=0xD) = (self.paging.as_mut(), addr >> 24) {
            let off = addr & 0x01FF_FFFF;
            let before = p.cache.misses;
            let v = match size {
                1 => p.cache.read8(&mut p.src, off) as u32,
                2 => p.cache.read16(&mut p.src, off) as u32,
                _ => p.cache.read32(&mut p.src, off),
            };
            let missed = (p.cache.misses - before) as u64;
            if missed != 0 {
                let c = missed * ROM_MISS_CYCLES;
                self.frame_cycles += c;
                self.total_cycles += c;
                self.stats.rom_miss_cycles += c;
            }
            return Some(v);
        }
        self.mem.read(addr, size)
    }

    // Translate the block at an ARM address, charging nothing. Returns the thumb address.
    // The code cache is full: throw all of it away. Nothing may hold addresses in it, which is
    // true outside the interrupt handler (the code it interrupted is in the cache).
    fn needs_flush(&self) -> bool {
        let margin = std::env::var("JITBENCH_MARGIN_HW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(FLUSH_MARGIN_HW);
        self.jit.free_halfwords() < margin || self.jit.free_blocks() < FLUSH_MARGIN_BLOCKS
    }

    fn flush(&mut self) {
        self.jit.reset();
        let at = (DISPATCH_ADDR - NATIVE_BASE) as usize;
        self.mem.native[at..at + common::jit::DISPATCH_BYTES].fill(0);
        self.stats.blocks.clear();
        self.stats.flushes += 1;
        self.stats.irq_bytes = 0;
    }

    // Throw away a region of the code cache that nothing running is in or about to return to
    fn evict_for_room(&mut self) {
        let mut keep = 0u32;
        let mut mark = |a: u32| {
            if let Some(r) = self.jit.region_of(a & !1) {
                keep |= 1 << r;
            }
        };
        mark(self.cur_pc);
        for r in self.cur_regs {
            mark(r);
        }
        if self.in_irq {
            mark(self.irq_ret);
        }
        // anything on the stack that is an address in the cache
        let top = NATIVE_BASE + NATIVE_LEN;
        let mut at = self.cur_regs[13] & !3;
        while at < top {
            if let Some(off) = Self::native_off(at, 4) {
                mark(self.read_native(off, 4));
            }
            at += 4;
        }
        match self.jit.evict_region(keep) {
            Some((lo, hi)) => {
                let table = (DISPATCH_ADDR - NATIVE_BASE) as usize;
                for e in 0..common::jit::DISPATCH_ENTRIES {
                    let at = table + 8 * e + 4;
                    let native =
                        u32::from_le_bytes(self.mem.native[at..at + 4].try_into().unwrap());
                    if native & !1 >= lo && native & !1 < hi {
                        self.mem.native[at - 4..at + 4].fill(0);
                    }
                }
                self.stats.flushes += 1;
            }
            None => self.die("every region of the code cache is in use"),
        }
    }

    fn translate(&mut self, arm: u32) -> u32 {
        if self.ring {
            return self.translate_ring(arm);
        }
        // the interrupt handler needs room to translate its code after a flush, outside of it
        // the cache is thrown away before that room is used
        if !self.in_irq && self.needs_flush() {
            self.flush();
        }
        let mut used0 = self.jit.used_halfwords();
        let t0 = Instant::now();
        let game_code = self.game_code;
        let mem = &self.mem;
        let paging = &mut self.paging;
        let mut r = self
            .jit
            .translate(arm, &mut |a| Self::gba_word(mem, paging, game_code, a));
        if r == Err(common::jit::JitError::CacheFull) && self.in_irq && !self.jit.in_emergency() {
            // the interrupt handler needs more than the margin: use the reserve
            self.jit.use_reserve();
            let game_code = self.game_code;
            let mem = &self.mem;
            let paging = &mut self.paging;
            r = self
                .jit
                .translate(arm, &mut |a| Self::gba_word(mem, paging, game_code, a));
        }
        if r == Err(common::jit::JitError::CacheFull) && !self.in_irq {
            self.flush();
            used0 = self.jit.used_halfwords();
            let game_code = self.game_code;
            let mem = &self.mem;
            let paging = &mut self.paging;
            r = self
                .jit
                .translate(arm, &mut |a| Self::gba_word(mem, paging, game_code, a));
        }
        self.stats.translate_ns += t0.elapsed().as_nanos() as u64;
        let thumb = match r {
            Ok(t) => t,
            Err(e) => self.die(&format!("cannot translate ARM code at {arm:#x}: {e:?}")),
        };
        if self.in_irq {
            self.stats.irq_bytes += 2 * (self.jit.used_halfwords() - used0);
            self.stats.irq_bytes_max = self.stats.irq_bytes_max.max(self.stats.irq_bytes);
        }
        self.record_block(used0, arm);
        thumb
    }

    fn translate_ring(&mut self, arm: u32) -> u32 {
        loop {
            let used0 = self.jit.used_halfwords();
            let t0 = Instant::now();
            let game_code = self.game_code;
            let mem = &self.mem;
            let paging = &mut self.paging;
            let r = self
                .jit
                .translate(arm, &mut |a| Self::gba_word(mem, paging, game_code, a));
            self.stats.translate_ns += t0.elapsed().as_nanos() as u64;
            match r {
                Ok(t) => {
                    self.record_block(used0, arm);
                    return t;
                }
                Err(common::jit::JitError::CacheFull) => self.evict_for_room(),
                Err(e) => self.die(&format!("cannot translate code at {arm:#x}: {e:?}")),
            }
        }
    }

    fn resolve(&mut self, slot: u32) -> u32 {
        if self.ring {
            loop {
                let used0 = self.jit.used_halfwords();
                let arm =
                    self.jit.halfword(slot + 4) as u32 | (self.jit.halfword(slot + 6) as u32) << 16;
                let game_code = self.game_code;
                let mem = &self.mem;
                let paging = &mut self.paging;
                let r = self
                    .jit
                    .resolve_slot(slot, &mut |a| Self::gba_word(mem, paging, game_code, a));
                match r {
                    Ok(t) => {
                        self.record_block(used0, arm);
                        return t;
                    }
                    Err(common::jit::JitError::CacheFull) => self.evict_for_room(),
                    Err(e) => self.die(&format!("cannot resolve slot at {slot:#x}: {e:?}")),
                }
            }
        }
        if !self.in_irq && self.needs_flush() {
            // the slot is gone with the rest of the cache
            let arm =
                self.jit.halfword(slot + 4) as u32 | (self.jit.halfword(slot + 6) as u32) << 16;
            return self.translate(arm);
        }
        let used0 = self.jit.used_halfwords();
        let arm = self.jit.halfword(slot + 4) as u32 | (self.jit.halfword(slot + 6) as u32) << 16;
        let t0 = Instant::now();
        let game_code = self.game_code;
        let mem = &self.mem;
        let paging = &mut self.paging;
        let r = self
            .jit
            .resolve_slot(slot, &mut |a| Self::gba_word(mem, paging, game_code, a));
        self.stats.translate_ns += t0.elapsed().as_nanos() as u64;
        if r == Err(common::jit::JitError::CacheFull) {
            // the slot is gone with the rest of the cache, or the reserve is opened up
            return self.translate(arm);
        }
        let thumb = match r {
            Ok(t) => t,
            Err(e) => self.die(&format!("cannot resolve slot at {slot:#x}: {e:?}")),
        };
        self.record_block(used0, arm);
        thumb
    }

    // a translation that grew the cache made a new block
    fn record_block(&mut self, used0: usize, arm: u32) {
        let used1 = self.jit.used_halfwords();
        if used1 != used0 {
            let arm_end = self.jit.last_arm_end;
            self.stats.arm_instrs += arm_end.wrapping_sub(arm) as u64 / 4;
            self.stats.blocks.push(BlockInfo {
                arm,
                arm_end,
                hw_start: used0 + (used0 & 1),
                hw_end: used1,
            });
        }
    }

    fn trap(&mut self, pc: u32, kind: usize) {
        self.stats.trap_count[kind] += 1;
        self.stats.trap_cycles += TRAP_CYCLES;
        self.charge_other(pc, TRAP_CYCLES);
    }
}

impl Bus for Sys {
    fn fetch(&mut self, addr: u32) -> u16 {
        match addr >> 24 {
            0x20 if Self::in_cache(addr) => self.jit.halfword(addr),
            0x08 => {
                let o = (addr - ROM_BASE) as usize;
                let f = self.frame_no;
                self.stats.code_pages.insert((o >> 9) as u32, f);
                for c in &mut self.stats.caches {
                    c.access(o as u32, f);
                }
                self.mem_read(addr, 2).unwrap() as u16
            }
            _ => match (self.mem_read(addr, 2), Self::native_off(addr, 2)) {
                (Some(v), _) => v as u16,
                // code copied onto the stack
                (None, Some(off)) => self.read_native(off, 2) as u16,
                _ => self.die(&format!("fetch from {addr:#x}")),
            },
        }
    }

    fn read(&mut self, addr: u32, size: u32, kind: Access) -> u32 {
        if let Some(off) = Self::native_off(addr, size) {
            let v = self.read_native(off, size);
            if self.trace {
                eprintln!("      read{size} {addr:08x} -> {v:x}");
            }
            return v;
        }
        let (addr, inline) = self.unalias(addr, size, kind);
        if kind == Access::Stack {
            self.die(&format!("stack access to the GBA address {addr:#x}"));
        }
        if kind == Access::Data && !inline {
            self.note_fault(addr);
        }
        self.note_ewram(addr, size, false, 0);
        // unmapped addresses read the open bus, which the BIOS leaves as a constant here
        let v = self.mem_read(addr, size).unwrap_or_else(|| match size {
            1 => BIOS_OPEN_BUS & 0xFF,
            2 => BIOS_OPEN_BUS & 0xFFFF,
            _ => BIOS_OPEN_BUS,
        });
        if matches!(addr >> 24, 8..=0xD) {
            let page = (addr & 0x01FF_FFFF) >> 9;
            let f = self.frame_no;
            let byte = addr & 0x01FF_FFFF;
            self.stats.data_pages.insert(page, f);
            for c in &mut self.stats.caches {
                c.access(byte, f);
            }
        }
        let v = if addr >> 24 == 4 {
            self.io_read(addr & 0xFFFFFF, size, v)
        } else if addr >> 24 == 0xE {
            self.flash.read(addr & 0xFFFF).map_or(v, |b| b as u32)
        } else {
            v
        };
        if std::env::var_os("JITBENCH_LOG_IO").is_some() && self.frame_no < 3 && addr == 0x0400_0004
        {
            eprintln!(
                "io read DISPSTAT size {size} -> {v:#x} at pc {:#x}",
                self.cur_pc
            );
        }
        // every read of VCOUNT moves it on, so a game polling it makes progress without the
        // result depending on how many cycles the polling loop takes
        let io = addr.wrapping_sub(0x0400_0000);
        if io < 0x400 && io <= 7 && io + size > 6 {
            let next = (self.mem.io16(VCOUNT) + 1) % 228;
            self.mem.set_vcount(next);
            // polling VCOUNT into line 160 is a VBlank, like on the device
            self.vblank_polled |= next == 160;
        }
        if self.trace {
            eprintln!("      read{size} {addr:08x} -> {v:x}");
        }
        v
    }

    fn write(&mut self, addr: u32, size: u32, value: u32, kind: Access) {
        if self.trace {
            eprintln!("      write{size} {addr:08x} <- {value:x}");
        }
        if let Some(off) = Self::native_off(addr, size) {
            if off < 2 * CACHE_HW as u32 {
                self.die(&format!("write into the code cache at {addr:#x}"));
            }
            for i in 0..size {
                self.mem.native[(off + i) as usize] = (value >> (8 * i)) as u8;
            }
            return;
        }
        let (addr, inline) = self.unalias(addr, size, kind);
        if kind == Access::Stack {
            self.die(&format!("stack access to the GBA address {addr:#x}"));
        }
        if kind == Access::Data && !inline {
            self.note_fault(addr);
        }
        self.note_ewram(addr, size, true, value);
        let old_if = self.mem.io16(IF);
        // a write to an unmapped address is ignored
        self.mem.write(addr, size, value);
        if addr >> 24 == 4 && addr & 0xFFFFFF < 0x400 {
            // IF is write one to clear
            if addr & 0xFFFFFE == 0x202 {
                self.mem.set_io16(IF, old_if & !(value as u16));
            }
            self.io_write(addr & 0xFFFFFF, size, value);
        } else if addr >> 24 == 0xE {
            self.flash.write(addr & 0xFFFF, value as u8);
        }
    }
}

// registers and flags saved by the irq trampoline
struct Saved {
    r: [u32; 16],
    flags: u32,
    ret_pc: u32,
    // the word a dispatch that missed leaves its target in: the interrupt may come between that
    // and the trap that reads it
    miss: u32,
}

pub struct Machine {
    pub cpu: Cpu,
    pub sys: Sys,
    in_irq: bool,
    saved: Option<Saved>,
    intr_waiting: bool,
    // frames end after one frame of cycles, with VBlank, for games that poll flags
    pub timed: bool,
    // translate Thumb code as well, so it runs from the code cache
    pub thumb_jit: bool,
    // print every instruction executed, for debugging
    pub trace: bool,
    pub trace_from: Option<u64>,
}

impl Machine {
    pub fn new(rom: &[u8], input: InputScript, thumb_jit: bool) -> Self {
        let code: &'static mut [u16] = Box::leak(vec![0u16; CACHE_HW].into_boxed_slice());
        let map: &'static mut [MapEntry] =
            Box::leak(vec![Jit::empty_map_entry(); MAP_LEN].into_boxed_slice());
        // JITBENCH_CACHE_KB limits how much of the cache the translator may use
        let kb: usize = std::env::var("JITBENCH_CACHE_KB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(128);
        let code = &mut code[..(kb * 512).min(CACHE_HW)];
        let mut jit = Jit::new(
            code,
            CACHE_BASE,
            map,
            REGIONS_ADDR,
            if thumb_jit { DISPATCH_ADDR } else { 0 },
        );
        // the last 6 KB are for when the cache is full inside the interrupt handler
        jit.set_reserve(3 * 1024);
        let ring = std::env::var("JITBENCH_REGIONS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if ring != 0 && thumb_jit {
            jit.set_regions(ring);
        }
        let mut mem = Mem::new(rom);
        mem.set_io16(KEYINPUT, 0x03FF);
        // the BIOS leaves the display in forced blank
        mem.set_io16(0, 0x0080);
        let stats = Stats {
            cache_instr: vec![0; CACHE_HW],
            last_run: vec![u32::MAX; CACHE_HW],
            cache_other: vec![0; CACHE_HW],
            ..Default::default()
        };
        let mut cpu = Cpu::new();
        // main starts the game in Thumb past the crt0 (a ROM with an unrecognised one needs --entry)
        let entry = common::rom::find_entry(|at| {
            let at = at as usize;
            rom.get(at..at + 4)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        });
        cpu.r[15] = entry.as_ref().map_or(ROM_BASE, |e| e.thumb & !1);
        cpu.r[13] = NATIVE_BASE + NATIVE_LEN;
        if let Some(e) = entry.as_ref().filter(|e| e.irq_handler != 0) {
            mem.set_irq_vector(e.irq_handler);
        }
        Machine {
            cpu,
            sys: Sys {
                mem,
                jit,
                stats,
                cur_pc: 0,
                trace: false,
                vblank_polled: false,
                frame_cycles: 0,
                frame_no: 0,
                input,
                in_irq: false,
                ring: ring != 0 && thumb_jit,
                cur_regs: [0; 16],
                irq_ret: 0,
                paging: None,
                game_code: rom
                    .get(0xAC..0xB0)
                    .map_or([0; 4], |b| b.try_into().unwrap()),
                total_cycles: 0,
                timers: common::timers::Timers::new(),
                flash: common::flash::FlashChip::new(),
            },
            in_irq: false,
            saved: None,
            intr_waiting: false,
            timed: false,
            trace_from: None,
            thumb_jit,
            trace: false,
        }
    }

    // Send ROM reads through a cache of kb kilobytes of 256 byte pages. The translated code stops
    // reading ROM inline, those loads fault and the handler reads through the cache.
    pub fn enable_rom_cache(&mut self, kb: usize) {
        let slots = kb * 1024 / PAGE_SIZE;
        let data: &'static mut [u8] = Box::leak(vec![0u8; slots * PAGE_SIZE].into_boxed_slice());
        let tags: &'static mut [u32] = Box::leak(vec![0u32; slots].into_boxed_slice());
        let stamps: &'static mut [u32] = Box::leak(vec![0u32; slots].into_boxed_slice());
        let run: &'static mut [u8; common::romcache::RUN_BYTES] =
            Box::leak(Box::new([0u8; common::romcache::RUN_BYTES]));
        let rom = self.sys.mem.rom.clone();
        self.sys.paging = Some(Paging {
            cache: RomCache::new(data, tags, stamps, run),
            src: VecSource(rom),
        });
        for top in 8..=0xDu32 {
            let at = (REGIONS_ADDR - NATIVE_BASE + 4 * top) as usize;
            self.sys.mem.native[at..at + 4].fill(0);
        }
    }

    pub fn frames_done(&self) -> u64 {
        self.sys.frame_no
    }

    pub fn run(&mut self, frames: u64) {
        while self.sys.frame_no < frames {
            let pc = self.cpu.r[15];
            if pc == MAGIC {
                self.finish_irq();
                continue;
            }
            if self.sys.ring {
                self.sys.cur_regs = self.cpu.r;
                self.sys.cur_pc = pc;
            }
            if self.thumb_jit && !Sys::in_cache(pc) && matches!(pc >> 24, 2 | 3 | 8..=0xD) {
                // Thumb code at a GBA address: the fault handler translates it and continues
                // in the code cache
                self.sys.trap(pc, 1);
                let t = self.sys.translate(pc | 1);
                self.cpu.r[15] = t & !1;
                continue;
            }
            self.sys.cur_pc = pc;
            if let Some(f) = self.trace_from {
                self.trace = self.sys.frame_no >= f;
            }
            self.sys.trace = self.trace;
            if self.trace {
                let hw = self.sys.fetch(pc);
                eprintln!(
                    "{pc:08x} {hw:04x} nzcv {:04b} r {:x?}",
                    self.cpu.flags() >> 28,
                    &self.cpu.r[..15]
                );
            }
            let step = self.cpu.step(&mut self.sys);
            self.sys.charge_instr(pc, step.cycles);
            if let Some(ev) = step.event {
                self.event(pc, ev);
            }
            if self.sys.vblank_polled {
                self.sys.vblank_polled = false;
                self.end_frame(true);
                self.try_inject(pc);
            }
            if self.timed && self.sys.frame_cycles > 2_083_333 {
                self.end_frame(true);
                self.try_inject(pc);
            }
            if self.sys.frame_cycles > FRAME_CAP {
                self.sys.stats.capped_frames += 1;
                self.end_frame(false);
            }
        }
    }

    fn event(&mut self, pc: u32, ev: Event) {
        match ev {
            Event::Svc(n) => self.svc(pc, n),
            Event::ArmJump(target) => {
                let veneer = self.sys.fetch(pc) == 0x4778;
                self.arm_jump(pc, target, if veneer { 2 } else { 1 });
            }
            Event::Udf(hw) if hw == TRAP && self.sys.jit.contains(pc) => {
                self.sys.trap(pc, 0);
                let t = self.sys.resolve(pc);
                self.cpu.r[15] = t & !1;
            }
            Event::Udf(hw) if hw == STUB_TRAP && self.sys.jit.is_stub(pc) => {
                // a dispatch missed: continue at the target, translating it first
                self.sys.trap(pc, 0);
                let var = (self.sys.jit.miss_var() - NATIVE_BASE) as usize;
                let target =
                    u32::from_le_bytes(self.sys.mem.native[var..var + 4].try_into().unwrap());
                let dest = if !matches!(target >> 24, 2 | 3 | 8..=0xD) {
                    // native code: the cache or something copied to the stack
                    target
                } else {
                    let t = self.sys.translate(target);
                    if target & 1 != 0 {
                        let at = (self.sys.jit.dispatch_entry(target) - NATIVE_BASE) as usize;
                        self.sys.mem.native[at..at + 4].copy_from_slice(&target.to_le_bytes());
                        self.sys.mem.native[at + 4..at + 8].copy_from_slice(&t.to_le_bytes());
                    }
                    t
                };
                self.cpu.r[15] = dest & !1;
            }
            Event::Udf(hw) => self.sys.die(&format!("udf {hw:#x}")),
            Event::Bkpt => self.sys.die("bkpt"),
            Event::Undefined(hw) => self.sys.die(&format!("undefined instruction {hw:#x}")),
        }
    }

    // a branch to an even address: ARM code, or the irq handler returning
    fn arm_jump(&mut self, pc: u32, target: u32, kind: usize) {
        if target & !1 == MAGIC {
            self.cpu.r[15] = MAGIC;
            return;
        }
        if !matches!(target >> 24, 2 | 3 | 8 | 9) {
            self.sys
                .die(&format!("jump to ARM code outside GBA memory: {target:#x}"));
        }
        self.sys.trap(pc, kind);
        let t = self.sys.translate(target);
        self.cpu.r[15] = t & !1;
    }

    // jump to a handler address from memory, which may be Thumb or ARM
    fn jump(&mut self, pc: u32, target: u32) {
        if target & 1 != 0 {
            self.cpu.r[15] = target & !1;
        } else {
            self.arm_jump(pc, target, 1);
        }
    }

    fn svc(&mut self, at: u32, n: u8) {
        self.sys.stats.svc_count += 1;
        self.charge_svc(at, SVC_CYCLES);
        let (r0, r1) = (self.cpu.r[0], self.cpu.r[1]);
        match n {
            // Div
            0x06 => {
                let (num, den) = (r0 as i32, r1 as i32);
                let r = &mut self.cpu.r;
                if den != 0 {
                    let q = num.wrapping_div(den);
                    r[0] = q as u32;
                    r[1] = num.wrapping_rem(den) as u32;
                    r[3] = q.unsigned_abs();
                }
            }
            // DivArm: the same with the operands swapped
            0x07 => {
                let (num, den) = (r1 as i32, r0 as i32);
                if den != 0 {
                    let q = num.wrapping_div(den);
                    let r = &mut self.cpu.r;
                    r[0] = q as u32;
                    r[1] = num.wrapping_rem(den) as u32;
                    r[3] = q.unsigned_abs();
                }
            }
            0x08 => self.cpu.r[0] = common::bios::sqrt(r0),
            0x09 => self.cpu.r[0] = common::bios::arctan(r0 as i32) as u32,
            0x0A => self.cpu.r[0] = common::bios::arctan2(r0 as i32, r1 as i32),
            0x0E => {
                let r2 = self.cpu.r[2];
                common::bios::bg_affine_set(&mut GbaMem(&mut self.sys), r0, r1, r2);
            }
            0x0F => {
                let (r2, r3) = (self.cpu.r[2], self.cpu.r[3]);
                common::bios::obj_affine_set(&mut GbaMem(&mut self.sys), r0, r1, r2, r3);
            }
            0x11 | 0x12 => common::bios::lz77(&mut GbaMem(&mut self.sys), r0, r1, n == 0x12),
            0x13 => common::bios::huffman(&mut GbaMem(&mut self.sys), r0, r1),
            0x14 | 0x15 => common::bios::run_length(&mut GbaMem(&mut self.sys), r0, r1, n == 0x15),
            0x0B => self.cpu_set(at, false),
            0x0C => self.cpu_set(at, true),
            // Halt
            0x02 => {}
            0x04 => self.intr_wait(at, r0 != 0, r1 as u16),
            0x05 => self.intr_wait(at, true, 1),
            n => self.sys.die(&format!("unhandled BIOS call {n:#x}")),
        }
        self.try_inject(at);
    }

    fn charge_svc(&mut self, pc: u32, c: u64) {
        self.sys.stats.svc_cycles += c;
        self.sys.charge_other(pc, c);
    }

    fn cpu_set(&mut self, at: u32, fast: bool) {
        let control = self.cpu.r[2];
        let mut count = (control & 0x1F_FFFF) as u64;
        let fill = control & (1 << 24) != 0;
        let word = fast || control & (1 << 26) != 0;
        if fast {
            count = (count + 7) & !7;
        }
        if count == 0 {
            return;
        }
        let align = if word { !3 } else { !1 };
        let (src, dst) = (self.cpu.r[0] & align, self.cpu.r[1] & align);
        let unit: u32 = if word { 4 } else { 2 };
        self.charge_svc(at, count * CPU_SET_UNIT_CYCLES);
        for i in 0..count as u32 {
            let from = if fill {
                src
            } else {
                src.wrapping_add(i * unit)
            };
            // like the firmware, an address that is not a GBA one is native (the stack)
            let v = match (self.sys.mem.read(from, unit), Sys::native_off(from, unit)) {
                (Some(v), _) => v,
                (None, Some(off)) => self.sys.read_native(off, unit),
                _ => self
                    .sys
                    .die(&format!("CpuSet read from unmapped {from:#x}")),
            };
            let to = dst.wrapping_add(i * unit);
            if !self.sys.mem.write(to, unit, v) {
                self.sys.die(&format!("CpuSet write to unmapped {to:#x}"));
            }
        }
    }

    // IntrWait and VBlankIntrWait, as intr_wait in the firmware: the call is run again until the
    // handler has set the flag. A frame ends whenever the flag is not set yet, which is when the
    // game waits for VBlank.
    fn intr_wait(&mut self, at: u32, discard: bool, mask: u16) {
        let mem = &mut self.sys.mem;
        let flags = mem.iwram16(BIOS_IF);
        if !self.intr_waiting {
            self.intr_waiting = true;
            mem.set_io16(IME, 1);
            if discard {
                mem.set_iwram16(BIOS_IF, flags & !mask);
            }
        }
        let flags = mem.iwram16(BIOS_IF);
        if flags & mask != 0 {
            mem.set_iwram16(BIOS_IF, flags & !mask);
            self.intr_waiting = false;
            // VBlank is over
            mem.set_vcount(0);
        } else {
            if mask & 1 == 0 {
                self.sys
                    .die(&format!("IntrWait for {mask:#x}, only VBlank is modelled"));
            }
            self.cpu.r[15] = at;
            self.end_frame(true);
        }
    }

    // Record the frame and start the next one. With vblank the game is waiting for it, which
    // sets the DISPSTAT flag, VCOUNT = 160 and raises the interrupt as raise_vblank_irq does.
    fn end_frame(&mut self, vblank: bool) {
        let sys = &mut self.sys;
        sys.stats.frames.push(sys.frame_cycles);
        if let Some(p) = &sys.paging {
            let total = p.cache.misses;
            let before: u32 = sys.stats.rom_misses_by_frame.iter().sum();
            sys.stats.rom_misses_by_frame.push(total - before);
        }
        sys.frame_cycles = 0;
        sys.frame_no += 1;
        if sys.frame_no % 60 == 0 && std::env::var_os("JITBENCH_WSET").is_some() {
            let now = sys.frame_no as u32;
            let within = |n: u32| {
                2 * sys
                    .stats
                    .last_run
                    .iter()
                    .filter(|&&f| f != u32::MAX && now - f <= n)
                    .count()
            };
            eprintln!(
                "wset frame {now}: code run in the last 1 frame {} B, 10 frames {} B, 60 frames {} B, 300 frames {} B (cache holds {} B)",
                within(1),
                within(10),
                within(60),
                within(300),
                sys.jit.code_used_bytes()
            );
        }
        // JITBENCH_WATCH=addr prints a 32 bit word of memory whenever it changes (debugging)
        if let Some(addr) = std::env::var("JITBENCH_WATCH")
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        {
            static mut LAST: u32 = 1;
            let now = sys.raw_read(addr, 4);
            if now != unsafe { LAST } {
                eprintln!("frame {}: [{:#x}] = {:#x}", sys.frame_no, addr, now);
                unsafe { LAST = now };
            }
        }
        let keys = if sys.input.pressed(sys.frame_no) {
            0x03FE
        } else {
            0x03FF
        };
        sys.mem.set_io16(KEYINPUT, keys);
        if vblank {
            sys.mem.set_vcount(160);
            if sys.mem.io16(DISPSTAT) & (1 << 3) != 0 {
                let f = sys.mem.io16(IF);
                sys.mem.set_io16(IF, f | 1);
            }
        }
    }

    // try_inject: if an enabled interrupt is pending, run the game's handler before resuming
    fn try_inject(&mut self, at: u32) {
        let mem = &self.sys.mem;
        let enabled = mem.io16(IME) & 1 != 0;
        let pending = mem.io16(IE) & mem.io16(IF);
        let vector = mem.iwram32(IRQ_VECTOR);
        if self.in_irq || !enabled || pending == 0 || vector == 0 {
            return;
        }
        self.in_irq = true;
        self.sys.in_irq = true;
        self.sys.stats.tramp_count += 1;
        self.sys.stats.tramp_cycles += TRAMPOLINE_CYCLES;
        self.sys.charge_other(at, TRAMPOLINE_CYCLES);
        self.saved = Some(Saved {
            r: self.cpu.r,
            flags: self.cpu.flags(),
            ret_pc: self.cpu.r[15] | 1,
            miss: self.miss_word(),
        });
        self.sys.irq_ret = self.cpu.r[15] | 1;
        self.cpu.r[0] = 0x0400_0000;
        self.cpu.r[14] = MAGIC | 1;
        self.jump(at, vector);
    }

    // irq_finish and the rest of the trampoline: acknowledge, restore everything, resume
    fn finish_irq(&mut self) {
        let mem = &mut self.sys.mem;
        let f = mem.io16(IF);
        mem.set_io16(IF, f & !1);
        self.in_irq = false;
        self.sys.in_irq = false;
        let s = self.saved.take().expect("irq return without an irq");
        self.cpu.r = s.r;
        self.cpu.set_flags(s.flags);
        self.cpu.r[15] = s.ret_pc & !1;
        self.set_miss_word(s.miss);
    }

    fn miss_word(&self) -> u32 {
        if !self.thumb_jit {
            return 0;
        }
        let at = (self.sys.jit.miss_var() - NATIVE_BASE) as usize;
        u32::from_le_bytes(self.sys.mem.native[at..at + 4].try_into().unwrap())
    }

    fn set_miss_word(&mut self, value: u32) {
        if !self.thumb_jit {
            return;
        }
        let at = (self.sys.jit.miss_var() - NATIVE_BASE) as usize;
        self.sys.mem.native[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
}
