// The GBA's 256 KiB of EWRAM on a board with a fraction of that. Games touch only part of it, so
// it is handed out in small pages the first time a page is used, and the pages that have not
// been used for a while go to a swap store (a file on the SD card) when the pool of RAM runs out.
//
// Translated code reaches a page through `table`, which holds the native address of every page.
// A page that is not mapped holds its own GBA address, which faults and ends up in `native`.
// There are no access bits, so `sweep` (once a frame) unmaps every resident page: the ones the
// game uses fault straight back in and get a fresh stamp, and the oldest stamp is the victim.
//
// A page that is all zeros costs nothing: it is not resident and not on the card. A page that
// was read back from the card and is unchanged is dropped without a write.

pub const PAGE_SIZE: usize = 512;
// 256 KiB, the EWRAM that repeats up to IWRAM
pub const PAGES: usize = 256 * 1024 / PAGE_SIZE;

const NO_SLOT: u8 = u8::MAX;
const FREE: u16 = u16::MAX;

// Where evicted pages go. A page number is 0..PAGES.
pub trait SwapStore {
    // false when there is nowhere to put pages, so the pool must not evict
    fn available(&self) -> bool {
        true
    }
    fn read(&mut self, page: usize, dst: &mut [u8; PAGE_SIZE]) -> bool;
    fn write(&mut self, page: usize, src: &[u8; PAGE_SIZE]) -> bool;
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    // pages read back from the store, written to it, dropped because unchanged, dropped as zero
    pub reads: u32,
    pub writes: u32,
    pub clean_drops: u32,
    pub zero_drops: u32,
    // pages that could not be written (their contents are lost) or read, and uses of a page that
    // found no slot and no way to make one
    pub write_errors: u32,
    pub read_errors: u32,
    pub full: u32,
}

pub struct EwramPool<'a> {
    // `slots` pages of PAGE_SIZE bytes
    data: &'a mut [u8],
    // PAGES entries: the native address of the slot, or the GBA address of the page
    table: &'a mut [u32],
    // the page in each slot, FREE for an empty one
    slot_page: &'a mut [u16],
    // the sweep count at the last use of each slot
    stamp: &'a mut [u32],
    // the checksum of the page as it was read back from the card, if `known` says it is valid
    sum: &'a mut [u32],
    known: &'a mut [bool],
    page_slot: [u8; PAGES],
    // the store holds a copy of the page
    swapped: [u8; PAGES / 8],
    gba_base: u32,
    epoch: u32,
    pub stats: Stats,
}

fn checksum(page: &[u8]) -> u32 {
    let mut h = 0x811C_9DC5u32;
    for w in page.chunks_exact(4) {
        h = (h ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]])).wrapping_mul(0x0100_0193);
        h ^= h >> 15;
    }
    h
}

fn all_zero(page: &[u8]) -> bool {
    page.chunks_exact(4)
        .all(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) == 0)
}

impl<'a> EwramPool<'a> {
    // `gba_base` is the GBA address of the start of EWRAM. The slices set the number of slots:
    // data holds slots * PAGE_SIZE bytes, the per slot ones `slots` entries, table PAGES.
    pub fn new(
        data: &'a mut [u8],
        table: &'a mut [u32],
        slot_page: &'a mut [u16],
        stamp: &'a mut [u32],
        sum: &'a mut [u32],
        known: &'a mut [bool],
        gba_base: u32,
    ) -> Self {
        let slots = slot_page.len();
        assert!(slots > 0 && slots < NO_SLOT as usize);
        assert!(data.len() >= slots * PAGE_SIZE && table.len() == PAGES);
        assert!(stamp.len() == slots && sum.len() == slots && known.len() == slots);
        slot_page.fill(FREE);
        for (p, t) in table.iter_mut().enumerate() {
            *t = gba_base + (p * PAGE_SIZE) as u32;
        }
        EwramPool {
            data,
            table,
            slot_page,
            stamp,
            sum,
            known,
            page_slot: [NO_SLOT; PAGES],
            swapped: [0; PAGES / 8],
            gba_base,
            epoch: 0,
            stats: Stats::default(),
        }
    }

    pub fn slots(&self) -> usize {
        self.slot_page.len()
    }

    pub fn resident(&self) -> usize {
        self.slot_page.iter().filter(|&&p| p != FREE).count()
    }

    // The native address of the first slot, slot s starts at base() + s * PAGE_SIZE
    pub fn base(&self) -> u32 {
        self.data.as_ptr() as usize as u32
    }

    pub fn data(&self) -> &[u8] {
        self.data
    }

    fn is_swapped(&self, page: usize) -> bool {
        self.swapped[page / 8] >> (page % 8) & 1 != 0
    }

    fn set_swapped(&mut self, page: usize, on: bool) {
        let bit = 1 << (page % 8);
        if on {
            self.swapped[page / 8] |= bit;
        } else {
            self.swapped[page / 8] &= !bit;
        }
    }

    // A page nothing but zeros has been stored in: it reads as zero and needs no slot
    pub fn is_absent(&self, page: usize) -> bool {
        self.page_slot[page] == NO_SLOT && !self.is_swapped(page)
    }

    fn unmapped(&self, page: usize) -> u32 {
        self.gba_base + (page * PAGE_SIZE) as u32
    }

    fn slot_bytes(&mut self, slot: usize) -> &mut [u8; PAGE_SIZE] {
        (&mut self.data[slot * PAGE_SIZE..][..PAGE_SIZE])
            .try_into()
            .unwrap()
    }

    // Unmap every resident page. The next use of one faults and renews its stamp.
    pub fn sweep(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        for slot in 0..self.slot_page.len() {
            let page = self.slot_page[slot];
            if page != FREE {
                self.table[page as usize] = self.unmapped(page as usize);
            }
        }
    }

    // Take a page out of its slot. Returns false when its contents could not be saved.
    fn evict<S: SwapStore>(&mut self, swap: &mut S, slot: usize) -> bool {
        let page = self.slot_page[slot] as usize;
        let (zero, sum) = {
            let bytes = &self.data[slot * PAGE_SIZE..][..PAGE_SIZE];
            (all_zero(bytes), checksum(bytes))
        };
        let mut ok = true;
        if zero {
            // the copy on the card, if any, is out of date now
            self.set_swapped(page, false);
            self.stats.zero_drops += 1;
        } else if self.is_swapped(page) && self.known[slot] && self.sum[slot] == sum {
            self.stats.clean_drops += 1;
        } else {
            let bytes: &[u8; PAGE_SIZE] = (&self.data[slot * PAGE_SIZE..][..PAGE_SIZE])
                .try_into()
                .unwrap();
            if swap.write(page, bytes) || swap.write(page, bytes) {
                self.set_swapped(page, true);
                self.stats.writes += 1;
            } else {
                self.stats.write_errors += 1;
                ok = false;
            }
        }
        self.table[page] = self.unmapped(page);
        self.page_slot[page] = NO_SLOT;
        self.slot_page[slot] = FREE;
        ok
    }

    // A slot for a page: a free one, else the slots of pages that turned out to be all zero,
    // else the one used longest ago. None when there is no way to make room.
    fn alloc<S: SwapStore>(&mut self, swap: &mut S) -> Option<usize> {
        if let Some(slot) = self.slot_page.iter().position(|&p| p == FREE) {
            return Some(slot);
        }
        // zero pages are the same as pages that were never used
        for slot in 0..self.slot_page.len() {
            if all_zero(&self.data[slot * PAGE_SIZE..][..PAGE_SIZE]) {
                self.evict(swap, slot);
            }
        }
        if let Some(slot) = self.slot_page.iter().position(|&p| p == FREE) {
            return Some(slot);
        }
        if !swap.available() {
            return None;
        }
        // an interrupt can come between translated code reading the table and using the entry,
        // and the handler may evict that page: the page of the access in progress has the newest
        // stamp, so the oldest is the one to take
        let victim = (0..self.slot_page.len())
            .max_by_key(|&s| self.epoch.wrapping_sub(self.stamp[s]))
            .unwrap();
        self.evict(swap, victim);
        Some(victim)
    }

    // The native address of the start of a page, making it resident. For a page that has never
    // been used this gives None unless `create` (a store of something other than zero), since
    // such a page reads as zero. None also when the pool is full and nothing can be evicted.
    pub fn native<S: SwapStore>(&mut self, swap: &mut S, page: usize, create: bool) -> Option<u32> {
        let slot = self.page_slot[page];
        if slot != NO_SLOT {
            let slot = slot as usize;
            self.stamp[slot] = self.epoch;
            let native = self.base() + (slot * PAGE_SIZE) as u32;
            self.table[page] = native;
            return Some(native);
        }
        let swapped = self.is_swapped(page);
        if !swapped && !create {
            return None;
        }
        let Some(slot) = self.alloc(swap) else {
            self.stats.full += 1;
            return None;
        };
        if swapped {
            if !swap.read(page, self.slot_bytes(slot)) {
                self.stats.read_errors += 1;
                return None;
            }
            self.stats.reads += 1;
            self.sum[slot] = checksum(&self.data[slot * PAGE_SIZE..][..PAGE_SIZE]);
            self.known[slot] = true;
        } else {
            self.slot_bytes(slot).fill(0);
            self.known[slot] = false;
        }
        self.slot_page[slot] = page as u16;
        self.page_slot[page] = slot as u8;
        self.stamp[slot] = self.epoch;
        let native = self.base() + (slot * PAGE_SIZE) as u32;
        self.table[page] = native;
        Some(native)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    struct Mem {
        pages: Vec<Option<[u8; PAGE_SIZE]>>,
        reads: u32,
        writes: u32,
        fail_writes: bool,
    }

    impl Mem {
        fn new() -> Self {
            Mem {
                pages: vec![None; PAGES],
                reads: 0,
                writes: 0,
                fail_writes: false,
            }
        }
    }

    impl SwapStore for Mem {
        fn read(&mut self, page: usize, dst: &mut [u8; PAGE_SIZE]) -> bool {
            self.reads += 1;
            match &self.pages[page] {
                Some(p) => {
                    *dst = *p;
                    true
                }
                None => false,
            }
        }
        fn write(&mut self, page: usize, src: &[u8; PAGE_SIZE]) -> bool {
            if self.fail_writes {
                return false;
            }
            self.writes += 1;
            self.pages[page] = Some(*src);
            true
        }
    }

    struct Setup {
        data: Vec<u8>,
        table: Vec<u32>,
        slot_page: Vec<u16>,
        stamp: Vec<u32>,
        sum: Vec<u32>,
        known: Vec<bool>,
    }

    impl Setup {
        fn new(slots: usize) -> Self {
            Setup {
                data: vec![0; slots * PAGE_SIZE],
                table: vec![0; PAGES],
                slot_page: vec![0; slots],
                stamp: vec![0; slots],
                sum: vec![0; slots],
                known: vec![false; slots],
            }
        }
        fn pool(&mut self) -> EwramPool<'_> {
            EwramPool::new(
                &mut self.data,
                &mut self.table,
                &mut self.slot_page,
                &mut self.stamp,
                &mut self.sum,
                &mut self.known,
                0x0200_0000,
            )
        }
    }

    // loads and stores the way the firmware does them
    fn load(p: &mut EwramPool, s: &mut Mem, off: usize) -> u32 {
        let page = off / PAGE_SIZE;
        if p.is_absent(page) {
            return 0;
        }
        let n = p.native(s, page, false).expect("resident or swapped page");
        let at = (n - p.base()) as usize + off % PAGE_SIZE;
        u32::from_le_bytes(p.data()[at..at + 4].try_into().unwrap())
    }

    fn store(p: &mut EwramPool, s: &mut Mem, off: usize, v: u32) {
        let page = off / PAGE_SIZE;
        if p.is_absent(page) && v == 0 {
            return;
        }
        let n = p.native(s, page, true).expect("a slot");
        let at = (n - p.base()) as usize + off % PAGE_SIZE;
        p.write_for_test(at, v);
    }

    impl EwramPool<'_> {
        fn write_for_test(&mut self, at: usize, v: u32) {
            self.data[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
    }

    #[test]
    fn behaves_like_flat_memory_under_eviction() {
        for slots in [2, 3, 8, 20] {
            let mut setup = Setup::new(slots);
            let mut pool = setup.pool();
            let mut swap = Mem::new();
            let mut flat = vec![0u32; 64 * PAGE_SIZE / 4];
            let mut rng = Rng(slots as u64 * 977);
            for i in 0..60_000u32 {
                // a few hot pages and a long tail of cold ones
                let page = if rng.next() % 4 == 0 {
                    rng.next() as usize % 64
                } else {
                    rng.next() as usize % 5
                };
                let word = rng.next() as usize % (PAGE_SIZE / 4);
                let off = page * PAGE_SIZE + word * 4;
                match rng.next() % 8 {
                    0 => {
                        // writes of zero, which can make a page all zero
                        store(&mut pool, &mut swap, off, 0);
                        flat[off / 4] = 0;
                    }
                    1..=3 => {
                        let v = rng.next() | 1;
                        store(&mut pool, &mut swap, off, v);
                        flat[off / 4] = v;
                    }
                    _ => {
                        assert_eq!(
                            load(&mut pool, &mut swap, off),
                            flat[off / 4],
                            "slots {slots} step {i} offset {off:#x}"
                        );
                    }
                }
                if rng.next() % 50 == 0 {
                    pool.sweep();
                }
            }
            // everything reads back
            for (w, &v) in flat.iter().enumerate() {
                assert_eq!(
                    load(&mut pool, &mut swap, w * 4),
                    v,
                    "slots {slots} word {w}"
                );
            }
            assert_eq!(pool.stats.write_errors, 0);
            assert_eq!(pool.stats.read_errors, 0);
            assert_eq!(pool.stats.full, 0);
            if slots < 20 {
                assert!(pool.stats.writes > 0 && pool.stats.reads > 0);
            }
        }
    }

    #[test]
    fn unchanged_and_zero_pages_are_not_written() {
        let mut setup = Setup::new(2);
        let mut pool = setup.pool();
        let mut swap = Mem::new();
        store(&mut pool, &mut swap, 0, 7);
        store(&mut pool, &mut swap, PAGE_SIZE, 8);
        pool.sweep();
        // page 2 evicts page 0 (oldest): a write
        store(&mut pool, &mut swap, 2 * PAGE_SIZE, 9);
        assert_eq!(swap.writes, 1);
        // page 0 comes back and goes again without having changed: no second write
        pool.sweep();
        assert_eq!(load(&mut pool, &mut swap, 0), 7);
        assert_eq!(load(&mut pool, &mut swap, 2 * PAGE_SIZE), 9);
        pool.sweep();
        assert_eq!(load(&mut pool, &mut swap, 0), 7);
        assert_eq!(load(&mut pool, &mut swap, 2 * PAGE_SIZE), 9);
        pool.sweep();
        // page 1 comes back from the card, pushing out the oldest of the two resident pages
        assert_eq!(load(&mut pool, &mut swap, PAGE_SIZE), 8);
        let resident: Vec<usize> = (0..4).filter(|&p| !pool.is_absent(p)).collect();
        assert_eq!(resident.len(), 3);
        // it is unchanged, so it goes again without a write
        pool.sweep();
        let keep = (0..3)
            .find(|&p| p != 1 && pool.page_slot[p] != NO_SLOT)
            .unwrap();
        assert_eq!(load(&mut pool, &mut swap, keep * PAGE_SIZE) != 0, true);
        let before = swap.writes;
        store(&mut pool, &mut swap, 3 * PAGE_SIZE, 1);
        assert_eq!(pool.page_slot[1], NO_SLOT);
        assert_eq!(swap.writes, before);
        assert_eq!(pool.stats.clean_drops, 1);
        let writes = swap.writes;
        // a page that went back to zero is not written, and reads as zero from then on
        store(&mut pool, &mut swap, PAGE_SIZE, 0);
        store(&mut pool, &mut swap, 3 * PAGE_SIZE, 1);
        store(&mut pool, &mut swap, 4 * PAGE_SIZE, 1);
        assert_eq!(load(&mut pool, &mut swap, PAGE_SIZE), 0);
        assert!(pool.stats.zero_drops > 0);
        assert!(swap.writes <= writes + 2);
    }

    #[test]
    fn the_oldest_page_goes_first() {
        let mut setup = Setup::new(2);
        let mut pool = setup.pool();
        let mut swap = Mem::new();
        store(&mut pool, &mut swap, 0, 1);
        pool.sweep();
        store(&mut pool, &mut swap, PAGE_SIZE, 2);
        pool.sweep();
        // touch page 0 again: page 1 is now the oldest
        assert_eq!(load(&mut pool, &mut swap, 0), 1);
        store(&mut pool, &mut swap, 2 * PAGE_SIZE, 3);
        assert!(pool.is_absent(1) == false);
        assert_eq!(swap.pages[1].is_some(), true);
        assert_eq!(swap.pages[0].is_some(), false);
    }

    #[test]
    fn no_swap_means_no_eviction() {
        struct NoSwap;
        impl SwapStore for NoSwap {
            fn available(&self) -> bool {
                false
            }
            fn read(&mut self, _: usize, _: &mut [u8; PAGE_SIZE]) -> bool {
                false
            }
            fn write(&mut self, _: usize, _: &[u8; PAGE_SIZE]) -> bool {
                false
            }
        }
        let mut setup = Setup::new(1);
        let mut pool = setup.pool();
        let mut swap = NoSwap;
        let n = pool.native(&mut swap, 0, true).unwrap();
        // the page is still zero: its slot is reclaimed for the next page
        assert!(pool.native(&mut swap, 1, true).is_some());
        assert!(pool.is_absent(0));
        let at = (n - pool.base()) as usize;
        pool.write_for_test(at, 5);
        // a page with contents cannot be evicted
        assert!(pool.native(&mut swap, 2, true).is_none());
        assert_eq!(pool.stats.full, 1);
        assert_eq!(pool.stats.write_errors, 0);
    }

    #[test]
    fn a_failed_write_is_counted() {
        let mut setup = Setup::new(1);
        let mut pool = setup.pool();
        let mut swap = Mem::new();
        store(&mut pool, &mut swap, 0, 5);
        swap.fail_writes = true;
        store(&mut pool, &mut swap, PAGE_SIZE, 6);
        assert_eq!(pool.stats.write_errors, 1);
    }
}
