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
// Thumb code is translated too, an address with bit 0 set (see translate/thumb_src.rs).
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
    // a conditional branch does not reach, translate then retries with long skips
    CondRange,
    Internal(&'static str),
    // an internal limit was hit while translating the instruction at addr
    At { addr: u32, msg: &'static str },
}

#[derive(Clone, Copy)]
pub struct MapEntry {
    arm: u32,
    thumb: u32,
}

// The indirect jump dispatcher: a direct mapped table of (GBA address, native address) pairs the
// translated Thumb code looks its jump targets up in, followed by the word a miss leaves the
// target in.
pub const DISPATCH_BITS: u32 = 7;
pub const DISPATCH_ENTRIES: usize = 1 << DISPATCH_BITS;
pub const DISPATCH_BYTES: usize = DISPATCH_ENTRIES * 8 + 4;

// UDF #0xFD, where a dispatch that missed ends up
pub const STUB_TRAP: u16 = 0xDEFD;

// The cache can be divided in regions that are thrown away one at a time, oldest first, instead of
// all at once. A direct jump from one region into another is written down, so that it can be
// turned back into an exit slot when its target goes.
pub const MAX_REGIONS: usize = 16;
const MAX_LINKS: usize = 160;

#[derive(Clone, Copy)]
struct Link {
    // the halfword of the slot that was patched
    slot: u16,
    // the region the jump goes to
    to: u8,
    // where the slot went to, to put back in it
    guest: u32,
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
    // the end of the cache translations may use: the last `reserve` halfwords are only for
    // emergencies, and so are the last eighth of the map
    limit: usize,
    reserve: usize,
    emergency: bool,
    // native address of the dispatch table, 0 when the code cache has no dispatcher (and Thumb
    // code cannot be translated)
    dispatch: u32,
    // halfwords at the start of the cache that hold the dispatcher
    reserved: usize,
    // the address of the UDF a dispatch that missed runs
    stub: u32,
    // regions: how many (0 for the cache to be used as a whole), how many halfwords each, the
    // one being filled
    nregions: usize,
    region_hw: usize,
    cur: usize,
    links: [Link; MAX_LINKS],
    nlinks: usize,
    // the ARM address after the last block translated
    pub last_arm_end: u32,
}

// the halfword an unresolved exit slot starts with
pub const TRAP: u16 = thumb::TRAP;

pub const MAP_ENTRY_SIZE: usize = core::mem::size_of::<MapEntry>();

impl<'a> Jit<'a> {
    // base must be the address code will be executed at, and word aligned. regions is the native
    // address of a table of 256 u32 offsets: a load or store through a register holding the GBA
    // address a goes to a + table[a >> 24] (or through a page table, see the top). The embedder
    // fills it, 0 entries leave the address alone. With regions == 0 nothing is translated.
    // dispatch is the native address of DISPATCH_BYTES of zeroed memory for the indirect jump
    // dispatcher that Thumb code needs, 0 for ARM only.
    pub fn new(
        code: &'a mut [u16],
        base: u32,
        map: &'a mut [MapEntry],
        regions: u32,
        dispatch: u32,
    ) -> Self {
        for m in map.iter_mut() {
            m.arm = 0;
        }
        let mut jit = Jit {
            code,
            base,
            used: 0,
            map,
            regions,
            blocks: 0,
            limit: 0,
            reserve: 0,
            emergency: false,
            dispatch,
            reserved: 0,
            stub: 0,
            nregions: 0,
            region_hw: 0,
            cur: 0,
            links: [Link {
                slot: 0,
                to: 0,
                guest: 0,
            }; MAX_LINKS],
            nlinks: 0,
            last_arm_end: 0,
        };
        jit.reset();
        jit
    }

    // Forget every translated block. The memory behind dispatch has to be cleared too, and nothing
    // may jump into the old code afterwards.
    pub fn reset(&mut self) {
        for m in self.map.iter_mut() {
            m.arm = 0;
        }
        self.blocks = 0;
        self.used = 0;
        self.emergency = false;
        self.limit = self.code.len() - self.reserve;
        self.nlinks = 0;
        if self.dispatch != 0 {
            let mut e = Emit::new(&mut self.code[..self.limit], self.base, 0);
            let stub_hw = translate::emit_dispatcher(&mut e, self.dispatch);
            self.used = e.pos();
            self.reserved = self.used;
            self.stub = self.base + 2 * stub_hw as u32;
        }
        if self.nregions != 0 {
            self.set_regions(self.nregions);
        }
    }

    // where the dispatch table keeps the entry for a GBA address
    pub fn dispatch_entry(&self, target: u32) -> u32 {
        self.dispatch + ((target >> 1) & (DISPATCH_ENTRIES as u32 - 1)) * 8
    }

    // the word a dispatch that missed stores its target in
    pub fn miss_var(&self) -> u32 {
        self.dispatch + DISPATCH_ENTRIES as u32 * 8
    }

    // Is this the UDF a dispatch that missed runs
    pub fn is_stub(&self, pc: u32) -> bool {
        self.dispatch != 0 && pc & !1 == self.stub
    }

    pub const fn empty_map_entry() -> MapEntry {
        MapEntry { arm: 0, thumb: 0 }
    }

    // blocks that fit in the map before it is full
    pub fn free_blocks(&self) -> usize {
        (self.map.len() * 3 / 4).saturating_sub(self.blocks)
    }

    // halfwords of code cache left
    pub fn free_halfwords(&self) -> usize {
        self.limit - self.used
    }

    // Hold the last halfwords of the cache back from translations, for emergency. Call right
    // after new.
    pub fn set_reserve(&mut self, halfwords: usize) {
        self.reserve = halfwords;
        self.limit = self.code.len() - halfwords;
    }

    // Is the reserve in use
    pub fn in_emergency(&self) -> bool {
        self.emergency
    }

    // Use what was held back, until the next reset: for when the cache is full at a time when it
    // cannot be thrown away.
    pub fn use_reserve(&mut self) {
        self.emergency = true;
        self.limit = self.code.len();
    }

    pub fn code_used_bytes(&self) -> usize {
        self.used * 2
    }

    pub fn block_count(&self) -> usize {
        self.blocks
    }

    // is addr inside the translated code
    pub fn contains(&self, addr: u32) -> bool {
        let end = if self.nregions != 0 {
            self.code.len()
        } else {
            self.used
        };
        addr >= self.base && addr < self.base + 2 * end as u32
    }

    // the halfword at an address inside the cache
    pub fn halfword(&self, addr: u32) -> u16 {
        self.code[((addr - self.base) / 2) as usize]
    }

    // Divide the cache after the dispatcher into this many regions, which are used in turn and
    // thrown away one by one with evict_region. Starts empty.
    pub fn set_regions(&mut self, count: usize) {
        self.nregions = count.min(MAX_REGIONS);
        let space = self.code.len() - self.used_by_dispatcher();
        self.region_hw = space / self.nregions & !1;
        self.cur = 0;
        self.nlinks = 0;
        for m in self.map.iter_mut() {
            m.arm = 0;
        }
        self.blocks = 0;
        self.used = self.region_start(0);
    }

    fn used_by_dispatcher(&self) -> usize {
        (self.reserved + 1) & !1
    }

    fn region_start(&self, r: usize) -> usize {
        self.used_by_dispatcher() + r * self.region_hw
    }

    fn region_end(&self, r: usize) -> usize {
        if r + 1 == self.nregions {
            self.code.len()
        } else {
            self.region_start(r + 1)
        }
    }

    // the region a halfword of the cache is in
    fn region_of_hw(&self, hw: usize) -> Option<usize> {
        let first = self.used_by_dispatcher();
        (hw >= first && self.region_hw != 0).then(|| ((hw - first) / self.region_hw).min(self.nregions - 1))
    }

    // The region a native address is in, for the embedder to keep the regions that code is running
    // in. None for an address that is not in a region.
    pub fn region_of(&self, addr: u32) -> Option<usize> {
        if self.nregions == 0 || addr < self.base {
            return None;
        }
        let hw = ((addr - self.base) / 2) as usize;
        if hw >= self.code.len() {
            return None;
        }
        self.region_of_hw(hw)
    }

    // Throw away a region to make room: the oldest one that is not in the set `keep` (a bit for
    // each region the code that is running, or going to be returned to, is in). The jumps into
    // it become exit slots again, and the blocks in it are forgotten. Returns the native
    // addresses it covered, for the embedder to remove from its dispatch table, or None when
    // there is nothing it may throw away.
    pub fn evict_region(&mut self, keep: u32) -> Option<(u32, u32)> {
        let n = self.nregions;
        let r = (1..=n)
            .map(|k| (self.cur + k) % n)
            .find(|&r| keep & (1 << r) == 0)?;
        let (lo_hw, hi_hw) = (self.region_start(r), self.region_end(r));
        let (lo, hi) = (self.base + 2 * lo_hw as u32, self.base + 2 * hi_hw as u32);
        // the blocks
        let mask = self.map.len() - 1;
        let mut i = 0;
        while i < self.map.len() {
            let m = self.map[i];
            if m.arm != 0 && (m.thumb & !1) >= lo && (m.thumb & !1) < hi {
                self.remove_at(i, mask);
                self.blocks -= 1;
            } else {
                i += 1;
            }
        }
        // the jumps
        let mut kept = 0;
        for j in 0..self.nlinks {
            let l = self.links[j];
            let from = self.region_of_hw(l.slot as usize);
            if from == Some(r) {
                continue;
            }
            if l.to as usize == r {
                let at = l.slot as usize;
                self.code[at] = thumb::TRAP;
                self.code[at + 1] = thumb::NOP;
                self.code[at + 2] = l.guest as u16;
                self.code[at + 3] = (l.guest >> 16) as u16;
                self.code[at + 4] = thumb::NOP;
                self.code[at + 5] = thumb::NOP;
                continue;
            }
            self.links[kept] = l;
            kept += 1;
        }
        self.nlinks = kept;
        self.cur = r;
        self.used = lo_hw;
        Some((lo, hi))
    }

    // delete an entry of the map, moving the ones after it that belong further back
    fn remove_at(&mut self, mut i: usize, mask: usize) {
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            if self.map[j].arm == 0 {
                break;
            }
            let k = self.slot_index(self.map[j].arm);
            let between = if i <= j { i < k && k <= j } else { i < k || k <= j };
            if between {
                continue;
            }
            self.map[i] = self.map[j];
            i = j;
        }
        self.map[i].arm = 0;
    }

    // the address Thumb code jumps to to dispatch through the table
    fn dispatch_routine(&self) -> u32 {
        if self.dispatch == 0 { 0 } else { self.base | 1 }
    }

    fn slot_index(&self, arm: u32) -> usize {
        // the map is a power of two sized open addressing table
        ((arm >> 1).wrapping_mul(0x9E37_79B1) >> 8) as usize & (self.map.len() - 1)
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

    // keep the table at most three quarters full
    fn map_full(&self) -> bool {
        let eighths = if self.emergency { 7 } else { 6 };
        self.blocks * 8 >= self.map.len() * eighths
    }

    fn insert(&mut self, arm: u32, thumb: u32) {
        let mask = self.map.len() - 1;
        let mut i = self.slot_index(arm);
        while self.map[i].arm != 0 {
            i = (i + 1) & mask;
        }
        self.map[i] = MapEntry { arm, thumb };
        self.blocks += 1;
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
        if self.map_full() {
            return Err(JitError::CacheFull);
        }
        let start = self.used;
        // blocks start on a word boundary so literal pools stay aligned
        let start = start + (start & 1);
        // a conditional skip that is too long to branch over makes a second try without sharing
        let mut long_skips = false;
        let routine = self.dispatch_routine();
        let (end, len) = loop {
            let end_hw = if self.nregions != 0 {
                self.region_end(self.cur)
            } else {
                self.limit
            };
            let e = Emit::new(&mut self.code[..end_hw], self.base, start);
            let mut tr = translate::Tr::new(e, mem, self.regions, long_skips, routine);
            let end = if arm & 1 != 0 {
                tr.thumb_block(arm & !1)
            } else {
                tr.block(arm)
            };
            if tr.e.overflowed() {
                if self.nregions != 0 && start <= self.region_start(self.cur) + 1 {
                    return Err(JitError::Internal("a block is bigger than a region"));
                }
                return Err(JitError::CacheFull);
            }
            match tr.e.err {
                Some(JitError::CondRange) if !long_skips => long_skips = true,
                Some(e) => return Err(e),
                None => break (end, tr.e.pos()),
            }
        };
        let thumb = self.base + 2 * start as u32 + 1;
        self.insert(arm, thumb);
        self.used = len;
        self.last_arm_end = end;
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
        if self.nregions != 0 {
            let to = self.region_of(thumb & !1);
            if to != self.region_of_hw(i) {
                if self.nlinks == MAX_LINKS {
                    // no way to take this jump back later: leave the slot
                    return;
                }
                self.links[self.nlinks] = Link {
                    slot: i as u16,
                    to: to.unwrap_or(0) as u8,
                    guest: self.code[i + 2] as u32 | (self.code[i + 3] as u32) << 16,
                };
                self.nlinks += 1;
            }
        }
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
