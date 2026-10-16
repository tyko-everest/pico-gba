// Runtime translator from ARM (the GBA CPU's native mode) to Thumb-1 (the only thing the RP2040's
// Cortex-M0+ can run).
//
// ARM code is translated one basic block at a time the first time it is reached, and kept in a
// code cache in RAM. Blocks end in exit slots that fault the first time they are used, the fault
// handler then translates the target block (if needed) and rewrites the slot into a direct jump.
// Everything here is no_std and allocation free so it can run inside the fault handler.
//
// Loads and stores translate their address inline through a table of offsets indexed by the top
// byte of the address (the `regions` argument of Jit::new), so GBA memory that the embedder can
// map flat is reached without a fault. Entry 0 leaves an address alone, such addresses fault and
// the embedder emulates the access. An odd entry is the address of a page table (512 native page
// addresses, indexed by bits 9-17 of the address) | 1, for memory that is mirrored or paged in.
// A page that is not there points at its own GBA address, so the access faults.
//
// The memory callback of translate reads the ARM code, and also the words of the region table and
// the page tables it points to (native addresses), which lets a load or store through a register
// with a known value skip the lookup. An embedder that answers None for those loses that
// optimisation and nothing else. Only pages that are there are used, they must stay where they are.
//
// Known gaps: the translator is verified against Unicorn for every ARM block reachable in the
// Dino ROM (decoding/jit_diff.py, zero mismatches), but not for ARM in general. Running
// `jit_diff.py random 800` generates random instructions of every supported kind and still shows
// mismatches. Unsupported: long multiplies, swap, coprocessors, RRX, S-bit LDM/STM, sp in an
// LDM/STM list, register offsets from sp with writeback.
mod decode;
mod emit;
mod thumb;
mod translate;

use emit::{Emit, SLOT_HW};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitError {
    // an ARM instruction the translator cannot handle yet
    Unsupported { addr: u32, word: u32 },
    // the ARM code could not be read
    BadRead(u32),
    // the code cache is full
    CacheFull,
    Internal(&'static str),
    // an internal limit was hit while translating the instruction at addr
    At { addr: u32, msg: &'static str },
}

#[derive(Clone, Copy)]
pub struct MapEntry {
    arm: u32,
    thumb: u32,
}

pub struct Jit<'a> {
    code: &'a mut [u16],
    // address code[0] runs at
    base: u32,
    // halfwords used
    used: usize,
    map: &'a mut [MapEntry],
    // native address of the 256 entry region offset table, 0 for none
    regions: u32,
    blocks: usize,
    // the ARM address after the last block translated
    pub last_arm_end: u32,
    // the registers (bit r for rN) and flags (N Z C V from bit 3) the code after it may read
    pub last_live_out: (u16, u8),
}

// the halfword an unresolved exit slot starts with
pub const TRAP: u16 = thumb::TRAP;

pub const MAP_ENTRY_SIZE: usize = core::mem::size_of::<MapEntry>();

impl<'a> Jit<'a> {
    // base must be the address code will be executed at, and word aligned. regions is the native
    // address of a table of 256 u32 offsets: a load or store through a register holding the GBA
    // address a goes to a + table[a >> 24] (or through a page table, see the top). The embedder
    // fills it, 0 entries leave the address alone. With regions == 0 nothing is translated.
    pub fn new(code: &'a mut [u16], base: u32, map: &'a mut [MapEntry], regions: u32) -> Self {
        for m in map.iter_mut() {
            m.arm = 0;
        }
        Jit {
            code,
            base,
            used: 0,
            map,
            regions,
            blocks: 0,
            last_arm_end: 0,
            last_live_out: (0xFFFF, 0xF),
        }
    }

    pub const fn empty_map_entry() -> MapEntry {
        MapEntry { arm: 0, thumb: 0 }
    }

    pub fn code_used_bytes(&self) -> usize {
        self.used * 2
    }

    pub fn block_count(&self) -> usize {
        self.blocks
    }

    // is addr inside the translated code
    pub fn contains(&self, addr: u32) -> bool {
        addr >= self.base && addr < self.base + 2 * self.used as u32
    }

    // the halfword at an address inside the cache
    pub fn halfword(&self, addr: u32) -> u16 {
        self.code[((addr - self.base) / 2) as usize]
    }

    fn slot_index(&self, arm: u32) -> usize {
        // the map is a power of two sized open addressing table
        ((arm >> 2).wrapping_mul(0x9E37_79B1) >> 8) as usize & (self.map.len() - 1)
    }

    // The thumb address (with the thumb bit set) of an ARM address that is already translated
    pub fn lookup(&self, arm: u32) -> Option<u32> {
        let mask = self.map.len() - 1;
        let mut i = self.slot_index(arm);
        loop {
            let m = self.map[i];
            if m.arm == 0 {
                return None;
            }
            if m.arm == arm {
                return Some(m.thumb);
            }
            i = (i + 1) & mask;
        }
    }

    fn insert(&mut self, arm: u32, thumb: u32) -> Result<(), JitError> {
        // keep the table at most half full
        if self.blocks * 2 >= self.map.len() {
            return Err(JitError::CacheFull);
        }
        let mask = self.map.len() - 1;
        let mut i = self.slot_index(arm);
        while self.map[i].arm != 0 {
            i = (i + 1) & mask;
        }
        self.map[i] = MapEntry { arm, thumb };
        self.blocks += 1;
        Ok(())
    }

    // Translate the block at an ARM address, or find it. Returns the address to run it at.
    pub fn translate(
        &mut self,
        arm: u32,
        mem: &mut dyn FnMut(u32) -> Option<u32>,
    ) -> Result<u32, JitError> {
        if let Some(t) = self.lookup(arm) {
            return Ok(t);
        }
        let start = self.used;
        // blocks start on a word boundary so literal pools stay aligned
        let start = start + (start & 1);
        // a conditional skip that is too long to branch over makes a second try without sharing
        let mut long_skips = false;
        let (end, len, live_out) = loop {
            let e = Emit::new(self.code, self.base, start);
            let mut tr = translate::Tr::new(e, mem, self.regions, long_skips);
            let end = tr.block(arm);
            if tr.e.overflowed() {
                return Err(JitError::CacheFull);
            }
            match tr.e.err {
                Some(JitError::At {
                    msg: emit::COND_RANGE,
                    ..
                }) if !long_skips => long_skips = true,
                Some(e) => return Err(e),
                None => break (end, tr.e.len, tr.live_out),
            }
        };
        self.used = len;
        self.last_arm_end = end;
        self.last_live_out = live_out;
        let thumb = self.base + 2 * start as u32 + 1;
        self.insert(arm, thumb)?;
        Ok(thumb)
    }

    // An exit slot trapped. Translate its target, rewrite the slot into a jump and return the
    // address to continue at.
    pub fn resolve_slot(
        &mut self,
        slot_addr: u32,
        mem: &mut dyn FnMut(u32) -> Option<u32>,
    ) -> Result<u32, JitError> {
        let i = ((slot_addr - self.base) / 2) as usize;
        let target = self.code[i + 2] as u32 | (self.code[i + 3] as u32) << 16;
        let thumb = self.translate(target, mem)?;
        self.patch_slot(i, thumb);
        Ok(thumb)
    }

    fn patch_slot(&mut self, i: usize, thumb: u32) {
        let slot_addr = self.base + 2 * i as u32;
        // a direct branch reaches +-2 KiB from the slot's pc
        let delta = (thumb & !1) as i32 - (slot_addr as i32 + 4);
        let hws = delta / 2;
        if (-1024..1024).contains(&hws) {
            self.code[i] = thumb::b(hws);
        } else {
            // push {r0, r1}; ldr r0, [pc, #4]; str r0, [sp, #4]; pop {r0, pc}; .word target
            self.code[i] = thumb::push(0b11, false);
            self.code[i + 1] = thumb::ldr_pc(0, 1);
            self.code[i + 2] = thumb::str_sp(0, 1);
            self.code[i + 3] = thumb::pop(0b01, true);
            self.code[i + 4] = thumb as u16;
            self.code[i + 5] = (thumb >> 16) as u16;
        }
        debug_assert!(SLOT_HW == 6);
    }

    // Every unresolved exit slot in the cache, used by tests to resolve everything up front
    pub fn is_unresolved_slot(&self, hw: usize) -> bool {
        hw + SLOT_HW <= self.used
            && (self.base as usize + 2 * hw) % 4 == 0
            && self.code[hw] == thumb::TRAP
            && self.code[hw + 1] == thumb::NOP
    }

    pub fn slot_addr(&self, hw: usize) -> u32 {
        self.base + 2 * hw as u32
    }

    pub fn used_halfwords(&self) -> usize {
        self.used
    }

    pub fn code(&self) -> &[u16] {
        &self.code[..self.used]
    }

    pub fn blocks(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.map
            .iter()
            .filter(|m| m.arm != 0)
            .map(|m| (m.arm, m.thumb))
    }
}
