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
// a miss reads this many neighbouring pages at once, code and data are mostly used in order
pub const RUN_PAGES: usize = 4;
// a miss right after the previous run reads this many of them in one go. 2 was tried on the
// device and saved nothing: the streams are short
pub const RUN_GROUPS: usize = 1;
pub const RUN_BYTES: usize = RUN_GROUPS * RUN_PAGES * PAGE_SIZE;

pub trait RomSource {
    // Read a run of pages, a multiple of RUN_PAGES of them from an offset that is a multiple of
    // RUN_PAGES * PAGE_SIZE. A source that reads in blocks does this in one go, which is much
    // cheaper than a read per page on an SD card.
    fn read_run(&mut self, offset: u32, dst: &mut [u8]) {
        for (i, page) in dst.chunks_exact_mut(PAGE_SIZE).enumerate() {
            self.read_page(offset + (i * PAGE_SIZE) as u32, page.try_into().unwrap());
        }
    }

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
    // where a run of pages is read to before it is spread over the slots
    run: &'a mut [u8; RUN_BYTES],
    clock: u32,
    // the group of pages after the last run read, a miss there is a stream
    next_group: u32,
    sets: usize,
    pub hits: u32,
    pub misses: u32,
}

impl<'a> RomCache<'a> {
    pub fn new(
        data: &'a mut [u8],
        tags: &'a mut [u32],
        stamps: &'a mut [u32],
        run: &'a mut [u8; RUN_BYTES],
    ) -> Self {
        let slots = tags.len();
        assert!(data.len() >= slots * PAGE_SIZE && stamps.len() == slots);
        assert!(slots % WAYS == 0 && slots > 0);
        tags.fill(0);
        stamps.fill(0);
        RomCache {
            data,
            tags,
            stamps,
            run,
            clock: 0,
            next_group: u32::MAX,
            sets: slots / WAYS,
            hits: 0,
            misses: 0,
        }
    }

    pub fn capacity_bytes(&self) -> usize {
        self.tags.len() * PAGE_SIZE
    }

    // The slot of a ROM page, read in first if it is not there. The pages around it are read in
    // the same request, since code and data are mostly used in order.
    fn slot<S: RomSource>(&mut self, src: &mut S, page: u32) -> usize {
        match self.find(page) {
            Some(slot) => {
                self.hits += 1;
                slot
            }
            None => {
                self.misses += 1;
                let first = page & !(RUN_PAGES as u32 - 1);
                let groups = if first / RUN_PAGES as u32 == self.next_group {
                    RUN_GROUPS
                } else {
                    1
                };
                self.next_group = first / RUN_PAGES as u32 + groups as u32;
                let pages = groups * RUN_PAGES;
                src.read_run(first << PAGE_SHIFT, &mut self.run[..pages * PAGE_SIZE]);
                let run = self.run.as_ptr();
                // the page asked for goes in last, so the others can not push it out again
                let target = (page - first) as usize;
                for i in (0..pages).filter(|&i| i != target) {
                    if self.find(first + i as u32).is_some() {
                        continue;
                    }
                    // Safety: fill does not touch the run buffer
                    let contents =
                        unsafe { core::slice::from_raw_parts(run.add(i * PAGE_SIZE), PAGE_SIZE) };
                    self.fill(first + i as u32, contents);
                }
                // Safety: as above
                let contents =
                    unsafe { core::slice::from_raw_parts(run.add(target * PAGE_SIZE), PAGE_SIZE) };
                self.fill(page, contents)
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

    // put a page in the least recently used slot of its set
    fn fill(&mut self, page: u32, contents: &[u8]) -> usize {
        let first = self.set_of(page);
        let mut victim = first;
        for i in first..first + WAYS {
            // an empty slot goes first, then the least recently used one
            if self.tags[i] == 0 || (self.tags[victim] != 0 && self.stamps[i] < self.stamps[victim])
            {
                victim = i;
            }
        }
        self.data[victim * PAGE_SIZE..][..PAGE_SIZE].copy_from_slice(contents);
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
        let mut run = [0u8; RUN_BYTES];
        let mut cache = RomCache::new(&mut data, &mut tags, &mut stamps, &mut run);
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
