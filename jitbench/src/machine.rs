// The pico-gba runtime on the host: GBA memory, the code cache, the fault model, BIOS calls and
// interrupts, mirroring firmware/src/main.rs. Time is counted in Cortex-M0+ cycles and only moves
// when the interpreter runs, so the GBA state does not depend on how fast the JIT code is.
use crate::cpu::{Access, Bus, Cpu, Event};
use common::jit::{Jit, MapEntry, TRAP};
use std::collections::HashMap;
use std::time::Instant;

// native RAM: the code cache is at its start, the stack at its end
pub const NATIVE_BASE: u32 = 0x2000_0000;
pub const NATIVE_LEN: u32 = 0x42000;
pub const CACHE_BASE: u32 = NATIVE_BASE;
// as in the firmware: 24 KiB of code and a 1024 entry block map
pub const CACHE_HW: usize = 12 * 1024;
pub const MAP_LEN: usize = 1024;

// The region offset table the translated code reads, after the code cache. A GBA address a that
// is flat in native memory is reached at a + table[a >> 24], in a window the interpreter treats
// as a non-faulting view of the GBA region. EWRAM and the BIOS have no entry, they still fault.
pub const REGIONS_ADDR: u32 = NATIVE_BASE + 0x8000;
// the page tables of EWRAM and IWRAM (512 entries each) follow the region table
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
            Which::Rom => return None,
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
    pub cache_other: Vec<u64>,
    pub blocks: Vec<BlockInfo>,
    pub translate_ns: u64,
    pub arm_instrs: u64,
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
        if let Some(path) = std::env::var_os("JITBENCH_DUMP_IWRAM") {
            let _ = std::fs::write(path, self.mem.iwram.bytes());
        }
        panic!("{msg} (pc {:#x}{at}, frame {})", self.cur_pc, self.frame_no);
    }

    fn charge_instr(&mut self, pc: u32, cycles: u32) {
        let c = cycles as u64;
        self.frame_cycles += c;
        if Self::in_cache(pc) {
            self.stats.jit_cycles += c;
            self.stats.cache_instr[((pc - CACHE_BASE) / 2) as usize] += c;
        } else {
            self.stats.native_cycles += c;
        }
    }

    // cycles that belong to the instruction at pc but are not the instruction itself
    fn charge_other(&mut self, pc: u32, c: u64) {
        self.frame_cycles += c;
        if Self::in_cache(pc) {
            self.stats.cache_other[((pc - CACHE_BASE) / 2) as usize] += c;
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
    fn gba_word(mem: &Mem, addr: u32) -> Option<u32> {
        if addr >= REGIONS_ADDR && addr < REGIONS_ADDR + 0x1400 {
            let at = (addr - NATIVE_BASE) as usize;
            return Some(u32::from_le_bytes(
                mem.native[at..at + 4].try_into().unwrap(),
            ));
        }
        mem.read(addr, 4)
    }

    // Translate the block at an ARM address, charging nothing. Returns the thumb address.
    fn translate(&mut self, arm: u32) -> u32 {
        let used0 = self.jit.used_halfwords();
        let t0 = Instant::now();
        let mem = &self.mem;
        let r = self.jit.translate(arm, &mut |a| Self::gba_word(mem, a));
        self.stats.translate_ns += t0.elapsed().as_nanos() as u64;
        let thumb = match r {
            Ok(t) => t,
            Err(e) => self.die(&format!("cannot translate ARM code at {arm:#x}: {e:?}")),
        };
        self.record_block(used0, arm);
        thumb
    }

    fn resolve(&mut self, slot: u32) -> u32 {
        let used0 = self.jit.used_halfwords();
        let arm = self.jit.halfword(slot + 4) as u32 | (self.jit.halfword(slot + 6) as u32) << 16;
        let t0 = Instant::now();
        let mem = &self.mem;
        let r = self.jit.resolve_slot(slot, &mut |a| Self::gba_word(mem, a));
        self.stats.translate_ns += t0.elapsed().as_nanos() as u64;
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
                u16::from_le_bytes([self.mem.rom[o], self.mem.rom[o + 1]])
            }
            _ => match self.mem.read(addr, 2) {
                Some(v) => v as u16,
                None => self.die(&format!("fetch from {addr:#x}")),
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
        let Some(v) = self.mem.read(addr, size) else {
            self.die(&format!("read of {size} bytes at unmapped {addr:#x}"));
        };
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
        if !self.mem.write(addr, size, value) {
            self.die(&format!("write of {size} bytes at unmapped {addr:#x}"));
        }
    }
}

// registers and flags saved by the irq trampoline
struct Saved {
    r: [u32; 16],
    flags: u32,
    ret_pc: u32,
}

pub struct Machine {
    pub cpu: Cpu,
    pub sys: Sys,
    in_irq: bool,
    saved: Option<Saved>,
    intr_waiting: bool,
    // print every instruction executed, for debugging
    pub trace: bool,
}

impl Machine {
    pub fn new(rom: &[u8], input: InputScript) -> Self {
        let code: &'static mut [u16] = Box::leak(vec![0u16; CACHE_HW].into_boxed_slice());
        let map: &'static mut [MapEntry] =
            Box::leak(vec![Jit::empty_map_entry(); MAP_LEN].into_boxed_slice());
        let jit = Jit::new(code, CACHE_BASE, map, REGIONS_ADDR);
        let mut mem = Mem::new(rom);
        mem.set_io16(KEYINPUT, 0x03FF);
        let stats = Stats {
            cache_instr: vec![0; CACHE_HW],
            cache_other: vec![0; CACHE_HW],
            ..Default::default()
        };
        let mut cpu = Cpu::new();
        // main starts the game in Thumb at ROM + 0xdc on the native stack
        cpu.r[15] = ROM_BASE + 0xdc;
        cpu.r[13] = NATIVE_BASE + NATIVE_LEN;
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
            },
            in_irq: false,
            saved: None,
            intr_waiting: false,
            trace: false,
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
            self.sys.cur_pc = pc;
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
            let Some(v) = self.sys.mem.read(from, unit) else {
                self.sys
                    .die(&format!("CpuSet read from unmapped {from:#x}"));
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
        sys.frame_cycles = 0;
        sys.frame_no += 1;
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
        self.sys.stats.tramp_count += 1;
        self.sys.stats.tramp_cycles += TRAMPOLINE_CYCLES;
        self.sys.charge_other(at, TRAMPOLINE_CYCLES);
        self.saved = Some(Saved {
            r: self.cpu.r,
            flags: self.cpu.flags(),
            ret_pc: self.cpu.r[15] | 1,
        });
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
        let s = self.saved.take().expect("irq return without an irq");
        self.cpu.r = s.r;
        self.cpu.set_flags(s.flags);
        self.cpu.r[15] = s.ret_pc & !1;
    }
}
