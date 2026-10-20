// A cache of ROM pages in RAM, for a ROM that does not fit and is read from somewhere slow (the SD
// card). The embedder gives it the memory and a way to read a page, and every ROM access goes
// through it, whether it comes from a load the fault handler emulates, a BIOS call, DMA or the
// translator reading code.
//
// It is set associative: a page can only live in one of the WAYS slots of its set, which keeps a
// lookup to a handful of compares.

pub const PAGE_SHIFT: u32 = 8;
pub const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
const WAYS: usize = 4;

pub trait RomSource {
    // Read PAGE_SIZE bytes (or less at the end of the ROM, the rest is zero) from byte offset in
    // the ROM. The data is zero when the read fails.
    fn read_page(&mut self, offset: u32, dst: &mut [u8; PAGE_SIZE]);
}

// The memory a cache works in, `pages` slots of PAGE_SIZE bytes (word aligned) with a tag and a
// last use stamp each. pages must be a multiple of WAYS.
pub struct RomCache<'a> {
    data: &'a mut [u8],
    // page number + 1 for each slot, 0 for an empty one
    tags: &'a mut [u32],
    stamps: &'a mut [u32],
    clock: u32,
    sets: usize,
    pub hits: u32,
    pub misses: u32,
}

impl<'a> RomCache<'a> {
    pub fn new(data: &'a mut [u8], tags: &'a mut [u32], stamps: &'a mut [u32]) -> Self {
        let slots = tags.len();
        assert!(data.len() >= slots * PAGE_SIZE && stamps.len() == slots);
        assert!(slots % WAYS == 0 && slots > 0);
        tags.fill(0);
        stamps.fill(0);
        RomCache {
            data,
            tags,
            stamps,
            clock: 0,
            sets: slots / WAYS,
            hits: 0,
            misses: 0,
        }
    }

    pub fn capacity_bytes(&self) -> usize {
        self.tags.len() * PAGE_SIZE
    }

    // The slot of a ROM page, read in first if it is not there. A page is read together with
    // the other half of its 512 byte block, which the source usually has at hand already, since
    // code and data are mostly used in order.
    fn slot<S: RomSource>(&mut self, src: &mut S, page: u32) -> usize {
        match self.find(page) {
            Some(slot) => {
                self.hits += 1;
                slot
            }
            None => {
                self.misses += 1;
                let slot = self.fill(src, page);
                let mate = page ^ 1;
                if self.find(mate).is_none() {
                    self.fill(src, mate);
                }
                slot
            }
        }
    }

    fn set_of(&self, page: u32) -> usize {
        (page.wrapping_mul(0x9E37_79B1) >> 12) as usize % self.sets * WAYS
    }

    // the slot a page is in, which counts as a use of it
    fn find(&mut self, page: u32) -> Option<usize> {
        let first = self.set_of(page);
        self.clock = self.clock.wrapping_add(1);
        let tag = page + 1;
        (first..first + WAYS)
            .find(|&i| self.tags[i] == tag)
            .map(|i| {
                self.stamps[i] = self.clock;
                i
            })
    }

    // read a page into the least recently used slot of its set
    fn fill<S: RomSource>(&mut self, src: &mut S, page: u32) -> usize {
        let first = self.set_of(page);
        let mut victim = first;
        for i in first..first + WAYS {
            // an empty slot goes first, then the least recently used one
            if self.tags[i] == 0 || (self.tags[victim] != 0 && self.stamps[i] < self.stamps[victim])
            {
                victim = i;
            }
        }
        let dst: &mut [u8; PAGE_SIZE] = (&mut self.data[victim * PAGE_SIZE..][..PAGE_SIZE])
            .try_into()
            .unwrap();
        src.read_page(page << PAGE_SHIFT, dst);
        self.tags[victim] = page + 1;
        self.stamps[victim] = self.clock;
        victim
    }

    pub fn read8<S: RomSource>(&mut self, src: &mut S, offset: u32) -> u8 {
        let slot = self.slot(src, offset >> PAGE_SHIFT);
        self.data[slot * PAGE_SIZE + (offset as usize & (PAGE_SIZE - 1))]
    }

    // little endian, any alignment
    pub fn read16<S: RomSource>(&mut self, src: &mut S, offset: u32) -> u16 {
        let inside = offset as usize & (PAGE_SIZE - 1);
        if inside < PAGE_SIZE - 1 {
            let slot = self.slot(src, offset >> PAGE_SHIFT);
            let at = slot * PAGE_SIZE + inside;
            u16::from_le_bytes([self.data[at], self.data[at + 1]])
        } else {
            self.read8(src, offset) as u16 | (self.read8(src, offset + 1) as u16) << 8
        }
    }

    pub fn read32<S: RomSource>(&mut self, src: &mut S, offset: u32) -> u32 {
        let inside = offset as usize & (PAGE_SIZE - 1);
        if inside <= PAGE_SIZE - 4 {
            let slot = self.slot(src, offset >> PAGE_SHIFT);
            let at = slot * PAGE_SIZE + inside;
            u32::from_le_bytes(self.data[at..at + 4].try_into().unwrap())
        } else {
            self.read16(src, offset) as u32 | (self.read16(src, offset + 2) as u32) << 16
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;

    struct Rom(std::vec::Vec<u8>, u32);

    impl RomSource for Rom {
        fn read_page(&mut self, offset: u32, dst: &mut [u8; PAGE_SIZE]) {
            self.1 += 1;
            for (i, b) in dst.iter_mut().enumerate() {
                *b = self.0.get(offset as usize + i).copied().unwrap_or(0);
            }
        }
    }

    #[test]
    fn reads_match_the_rom_and_pages_are_reused() {
        let rom: std::vec::Vec<u8> = (0..4096u32).map(|i| (i * 7 + i / 256) as u8).collect();
        let mut src = Rom(rom.clone(), 0);
        let mut data = vec![0u8; 8 * PAGE_SIZE];
        let (mut tags, mut stamps) = (vec![0u32; 8], vec![0u32; 8]);
        let mut cache = RomCache::new(&mut data, &mut tags, &mut stamps);
        for round in 0..3 {
            for off in (0..4093u32).step_by(37) {
                let expect =
                    u32::from_le_bytes(rom[off as usize..off as usize + 4].try_into().unwrap());
                assert_eq!(
                    cache.read32(&mut src, off),
                    expect,
                    "round {round} at {off}"
                );
                assert_eq!(cache.read16(&mut src, off), expect as u16);
                assert_eq!(cache.read8(&mut src, off), expect as u8);
            }
        }
        // a hot page stays
        cache.read8(&mut src, 5);
        let before = src.1;
        for _ in 0..100 {
            cache.read8(&mut src, 5);
        }
        assert_eq!(src.1, before);
    }
}
