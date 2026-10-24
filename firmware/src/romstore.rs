// The ROM on the SD card. Core 1 owns the card and the FAT file system, and answers read
// requests from core 0 (the fault handler paging ROM in) in between the display's SPI1 traffic.
// A request is served in the VBlank, when SPI1 is idle, so it takes up to a frame to be answered.
use crate::sdcard::{self, SdCard, SdDevice};
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU32, Ordering};
use embedded_hal::digital::OutputPin;
use embedded_sdmmc::{Mode, RawFile, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use rtt_target::rprintln;

// The ROM to run when the card has several: the one whose name starts with this, set when
// building (GBA_ROM=SM2 cargo build --release). Without it the first .GBA is used.
const ROM_PREFIX: Option<&str> = option_env!("GBA_ROM");

const IDLE: u32 = 0;
const PENDING: u32 = 1;
const DONE: u32 = 2;
const FAILED: u32 = 3;

// The request mailbox. Core 0 fills in the parameters and then sets PENDING, core 1 answers
// with DONE or FAILED. Only one request is outstanding since core 0 waits for it.
static STATE: AtomicU32 = AtomicU32::new(IDLE);
// what the request is: a ROM read, or a read or write of a page of the swap file
static OP: AtomicU32 = AtomicU32::new(OP_ROM);
const OP_ROM: u32 = 0;
const OP_SWAP_READ: u32 = 1;
const OP_SWAP_WRITE: u32 = 2;
static OFFSET: AtomicU32 = AtomicU32::new(0);
static LEN: AtomicU32 = AtomicU32::new(0);
static DST: AtomicU32 = AtomicU32::new(0);
// the size of the ROM file, 0 until core 1 has found it
// timing, in microseconds: how long requests waited for core 1 to get to them, how long it took
// to read, and how many there were
pub static US_WAITED: AtomicU32 = AtomicU32::new(0);
pub static US_READING: AtomicU32 = AtomicU32::new(0);
pub static REQUESTS: AtomicU32 = AtomicU32::new(0);
static POSTED_AT: AtomicU32 = AtomicU32::new(0);

fn now_us() -> u32 {
    unsafe { (*crate::pac::TIMER::ptr()).timerawl().read().bits() }
}

static ROM_SIZE: AtomicU32 = AtomicU32::new(NOT_LOOKED_UP);
const NOT_LOOKED_UP: u32 = 0;
const NO_ROM: u32 = u32::MAX;

// The size of the ROM on the card. Outer None while core 1 has not looked yet, the inner one is
// None when there is no ROM.
pub fn rom_size() -> Option<Option<u32>> {
    match ROM_SIZE.load(Ordering::Acquire) {
        NOT_LOOKED_UP => None,
        NO_ROM => Some(None),
        size => Some(Some(size)),
    }
}

// Post a request and wait for core 1 to answer it. Core 1 needs a VBlank to get to it, so this
// must not be used before core 1 runs.
fn request(op: u32, offset: u32, ptr: *mut u8, len: usize) -> bool {
    OP.store(op, Ordering::Relaxed);
    OFFSET.store(offset, Ordering::Relaxed);
    LEN.store(len as u32, Ordering::Relaxed);
    DST.store(ptr as u32, Ordering::Relaxed);
    POSTED_AT.store(now_us(), Ordering::Relaxed);
    STATE.store(PENDING, Ordering::Release);
    loop {
        match STATE.load(Ordering::Acquire) {
            PENDING => {}
            result => {
                STATE.store(IDLE, Ordering::Relaxed);
                return result == DONE;
            }
        }
    }
}

// Read `dst.len()` bytes of the ROM starting at `offset`. Blocks until core 1 has done it.
// Returns false when it failed.
pub fn read(offset: u32, dst: &mut [u8]) -> bool {
    request(OP_ROM, offset, dst.as_mut_ptr(), dst.len())
}

// The swap file holds one 512 byte page for each page of the EWRAM, a file named *.SWP in the
// root directory that is at least 256 KiB and lies in few runs of clusters (EWRAM.SWP is made
// when there is none). Its contents are never looked at, pages are written before they are read.
static SWAP_PAGES: AtomicU32 = AtomicU32::new(0);

// Is there a swap file to put pages in
pub fn swap_available() -> bool {
    SWAP_PAGES.load(Ordering::Acquire) != 0
}

pub fn swap_read(page: usize, dst: &mut [u8; BLOCK]) -> bool {
    request(OP_SWAP_READ, (page * BLOCK) as u32, dst.as_mut_ptr(), BLOCK)
}

pub fn swap_write(page: usize, src: &[u8; BLOCK]) -> bool {
    request(
        OP_SWAP_WRITE,
        (page * BLOCK) as u32,
        src.as_ptr() as *mut u8,
        BLOCK,
    )
}

// Is a read waiting for core 1
pub fn request_pending() -> bool {
    STATE.load(Ordering::Relaxed) == PENDING
}

// There is no clock on the board and the ROM is only read
struct NoClock;

impl TimeSource for NoClock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp::from_calendar(2024, 1, 1, 0, 0, 0).unwrap()
    }
}

// Where the file is on the card, for reading any part of it without walking the cluster chain:
// the files that are copied onto a card are contiguous or nearly so, which a few extents describe
const MAX_EXTENTS: usize = 24;
const BLOCK: usize = 512;

struct Layout {
    // the device block of the first sector of cluster 2
    data_start: u32,
    sectors_per_cluster: u32,
    // runs of clusters: the first and how many
    extents: [(u32, u32); MAX_EXTENTS],
    count: usize,
}

impl Layout {
    // the device block that holds a byte of the file
    fn block_of(&self, offset: u32) -> Option<u32> {
        let cluster_bytes = self.sectors_per_cluster * BLOCK as u32;
        let mut index = offset / cluster_bytes;
        for &(start, n) in &self.extents[..self.count] {
            if index < n {
                return Some(
                    self.data_start
                        + (start - 2 + index) * self.sectors_per_cluster
                        + offset % cluster_bytes / BLOCK as u32,
                );
            }
            index -= n;
        }
        None
    }
}

pub struct RomStore<SdCs: OutputPin> {
    volumes: VolumeManager<SdDevice<SdCs>, NoClock, 2, 2, 1>,
    file: Option<RawFile>,
    layout: Option<Layout>,
    swap_layout: Option<Layout>,
    // the last block read and where it came from, so reads next to each other read once
    block: [u8; BLOCK],
    block_no: u32,
    scratch: [u8; BLOCK],
}

#[derive(Clone, Copy)]
enum Buf {
    Scratch,
    Data,
}

impl<SdCs: OutputPin> RomStore<SdCs> {
    // Mount the card and open the first .GBA file in its root directory. SPI1 must be idle, the
    // display's chip select is `lcd_cs`.
    pub fn new(card: SdCard<SdCs>, lcd_cs: &mut impl OutputPin) -> Self {
        with_card_bus(lcd_cs, || {
            let volumes = VolumeManager::new_with_limits(SdDevice::new(card), NoClock, 0);
            let mut store = Self {
                volumes,
                file: None,
                layout: None,
                swap_layout: None,
                block: [0; BLOCK],
                block_no: u32::MAX,
                scratch: [0; BLOCK],
            };
            store.open_rom();
            store
        })
    }

    fn open_rom(&mut self) {
        let result = (|| {
            let volume = self.volumes.open_raw_volume(VolumeIdx(0))?;
            let root = self.volumes.open_root_dir(volume)?;
            let mut found = None;
            let mut entry_at = (0, 0);
            self.volumes.iterate_dir(root, |entry| {
                rprintln!("  {} ({} bytes)", entry.name, entry.size);
                if !entry.attributes.is_directory() && entry.name.extension() == b"GBA" {
                    // the first .GBA, unless one starts with the name the firmware was built
                    // with (GBA_ROM=SM2 cargo build ...), which wins
                    let wanted = ROM_PREFIX.is_some_and(|p| {
                        entry.name.base_name().len() >= p.len()
                            && entry.name.base_name()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
                    });
                    if found.is_none() || wanted {
                        found = Some(entry.name.clone());
                        entry_at = (entry.entry_block.0, entry.entry_offset);
                    }
                    if wanted || ROM_PREFIX.is_none() {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            })?;
            let Some(name) = found else {
                rprintln!("no .GBA file in the root directory of the SD card");
                return Ok(());
            };
            let file = self.volumes.open_file_in_dir(root, &name, Mode::ReadOnly)?;
            let size = self.volumes.file_length(file)?;
            rprintln!("ROM on the SD card: {} ({} bytes)", name, size);
            self.file = Some(file);
            self.layout = self.find_layout(entry_at.0, entry_at.1, size);
            match &self.layout {
                Some(l) => rprintln!("ROM in {} runs of clusters", l.count),
                None => rprintln!("ROM is too fragmented to read directly, reading slowly"),
            }
            self.open_swap(root)?;
            ROM_SIZE.store(size, Ordering::Release);
            Ok::<_, embedded_sdmmc::Error<sdcard::Error>>(())
        })();
        if let Err(e) = result {
            rprintln!("cannot open the ROM on the SD card: {:?}", e);
            self.dump_first_block();
        }
        if self.file.is_none() {
            ROM_SIZE.store(NO_ROM, Ordering::Release);
        }
    }

    // The first *.SWP in the root directory: name, size and where its directory entry is
    fn find_swap(
        &mut self,
        root: embedded_sdmmc::RawDirectory,
    ) -> Result<
        Option<(embedded_sdmmc::ShortFileName, u32, u32, u32)>,
        embedded_sdmmc::Error<sdcard::Error>,
    > {
        let mut found = None;
        self.volumes.iterate_dir(root, |entry| {
            if !entry.attributes.is_directory() && entry.name.extension() == b"SWP" {
                found = Some((
                    entry.name.clone(),
                    entry.size,
                    entry.entry_block.0,
                    entry.entry_offset,
                ));
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })?;
        Ok(found)
    }

    // Make EWRAM.SWP: a file of zeros the size of the EWRAM. On a card the ROM was just copied to
    // the clusters follow each other, which is what reading it directly needs.
    fn create_swap(
        &mut self,
        root: embedded_sdmmc::RawDirectory,
    ) -> Result<(), embedded_sdmmc::Error<sdcard::Error>> {
        rprintln!("making EWRAM.SWP on the SD card");
        let file = self
            .volumes
            .open_file_in_dir(root, "EWRAM.SWP", Mode::ReadWriteCreate)?;
        self.scratch.fill(0);
        let mut result = Ok(());
        for _ in 0..common::ewram::PAGES {
            result = self.volumes.write(file, &self.scratch);
            if result.is_err() {
                break;
            }
        }
        self.volumes.close_file(file)?;
        result
    }

    // Find the swap file, making it if there is none, and work out where it is on the card
    fn open_swap(
        &mut self,
        root: embedded_sdmmc::RawDirectory,
    ) -> Result<(), embedded_sdmmc::Error<sdcard::Error>> {
        let mut found = self.find_swap(root)?;
        if found.is_none() {
            if let Err(e) = self.create_swap(root) {
                rprintln!("cannot make the swap file: {:?}", e);
                return Ok(());
            }
            found = self.find_swap(root)?;
        }
        let Some((name, size, block, offset)) = found else {
            rprintln!("no swap file on the SD card: EWRAM cannot be paged out");
            return Ok(());
        };
        let need = (common::ewram::PAGES * BLOCK) as u32;
        if size < need {
            rprintln!("swap file {} is {} bytes, it needs {}", name, size, need);
            return Ok(());
        }
        self.swap_layout = self.find_layout(block, offset, need);
        match &self.swap_layout {
            Some(l) => {
                rprintln!("swap file {} in {} runs of clusters", name, l.count);
                SWAP_PAGES.store(common::ewram::PAGES as u32, Ordering::Release);
            }
            None => rprintln!("swap file {} is too fragmented to use", name),
        }
        Ok(())
    }

    // Read a device block into a buffer. The buffers live in the struct: core 1's stack is small.
    fn read_block(&mut self, block: u32, dst: Buf) -> bool {
        use embedded_sdmmc::BlockDevice;
        let buffer = match dst {
            Buf::Scratch => &mut self.scratch,
            Buf::Data => &mut self.block,
        };
        // Safety: a Block is one array of 512 bytes
        let b = unsafe { &mut *(buffer as *mut [u8; BLOCK] as *mut embedded_sdmmc::Block) };
        self.volumes
            .device(|card| card.read(core::slice::from_mut(b), embedded_sdmmc::BlockIdx(block)))
            .is_ok()
    }

    // Work out where the clusters of the file are: parse the boot sector and walk the FAT once.
    // entry_block and entry_offset say where the directory entry of the file is.
    fn find_layout(&mut self, entry_block: u32, entry_offset: u32, size: u32) -> Option<Layout> {
        let le16 = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]) as u32;
        let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        if !self.read_block(entry_block, Buf::Scratch) {
            return None;
        }
        let at = entry_offset as usize;
        let first = le16(&self.scratch[at..], 20) << 16 | le16(&self.scratch[at..], 26);
        // the first partition: its start, and in it the boot sector
        if !self.read_block(0, Buf::Scratch) {
            return None;
        }
        let part_start = le32(&self.scratch, 446 + 8);
        if !self.read_block(part_start, Buf::Scratch) {
            return None;
        }
        let bpb = &self.scratch;
        if le16(bpb, 11) != BLOCK as u32 {
            return None;
        }
        let spc = bpb[13] as u32;
        let fat_start = part_start + le16(bpb, 14);
        let fat_size = le32(bpb, 36);
        let data_start = fat_start + bpb[16] as u32 * fat_size;
        let clusters = size.div_ceil(spc * BLOCK as u32);

        let mut layout = Layout {
            data_start,
            sectors_per_cluster: spc,
            extents: [(0, 0); MAX_EXTENTS],
            count: 0,
        };
        let (mut cluster, mut cached) = (first, u32::MAX);
        for _ in 0..clusters {
            match layout.extents[..layout.count].last_mut() {
                Some((start, n)) if *start + *n == cluster => *n += 1,
                _ => {
                    if layout.count == MAX_EXTENTS {
                        return None;
                    }
                    layout.extents[layout.count] = (cluster, 1);
                    layout.count += 1;
                }
            }
            let block = fat_start + cluster * 4 / BLOCK as u32;
            if block != cached {
                if !self.read_block(block, Buf::Scratch) {
                    return None;
                }
                cached = block;
            }
            cluster = le32(&self.scratch, cluster as usize * 4 % BLOCK) & 0x0FFF_FFFF;
        }
        Some(layout)
    }

    // Print the start of the card and its partition table, for working out why it did not mount
    fn dump_first_block(&mut self) {
        use embedded_sdmmc::BlockDevice;
        let mut block = [embedded_sdmmc::Block::new()];
        let read = self
            .volumes
            .device(|card| card.read(&mut block, embedded_sdmmc::BlockIdx(0)));
        let b = &block[0].contents;
        rprintln!("block 0 read {:?}, start {:02x?}", read, &b[..16]);
        for i in 0..4 {
            rprintln!("partition {}: {:02x?}", i, &b[446 + i * 16..462 + i * 16]);
        }
        rprintln!("signature {:02x?}", &b[510..]);
    }

    // Answer a pending request, returns whether there was one. Call it while SPI1 is idle.
    // Serve the request core 0 posted, if there is one. Returns true when one was answered.
    pub fn poll(&mut self, lcd_cs: &mut impl OutputPin) -> bool {
        if STATE.load(Ordering::Acquire) != PENDING {
            return false;
        }
        let offset = OFFSET.load(Ordering::Relaxed);
        let len = LEN.load(Ordering::Relaxed) as usize;
        let dst = DST.load(Ordering::Relaxed) as *mut u8;
        // Safety: core 0 handed over this buffer and waits until the request is answered
        let dst = unsafe { core::slice::from_raw_parts_mut(dst, len) };
        let op = OP.load(Ordering::Relaxed);
        let started = now_us();
        let ok = with_card_bus(lcd_cs, || match op {
            OP_ROM => self.read_rom(offset, dst),
            OP_SWAP_READ => self.swap_io(offset, dst, false),
            _ => self.swap_io(offset, dst, true),
        });
        let done = now_us();
        US_WAITED.store(
            US_WAITED
                .load(Ordering::Relaxed)
                .wrapping_add(started.wrapping_sub(POSTED_AT.load(Ordering::Relaxed))),
            Ordering::Relaxed,
        );
        US_READING.store(
            US_READING
                .load(Ordering::Relaxed)
                .wrapping_add(done.wrapping_sub(started)),
            Ordering::Relaxed,
        );
        REQUESTS.store(REQUESTS.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        STATE.store(if ok { DONE } else { FAILED }, Ordering::Release);
        true
    }

    // Read or write one page of the swap file. A write is checked by the card (CRC), not read back.
    fn swap_io(&mut self, offset: u32, buf: &mut [u8], write: bool) -> bool {
        use embedded_sdmmc::BlockDevice;
        let Some(block) = self.swap_layout.as_ref().and_then(|l| l.block_of(offset)) else {
            return false;
        };
        if buf.len() != BLOCK {
            return false;
        }
        // Safety: a Block is one array of 512 bytes with no padding
        self.volumes
            .device(|card| {
                if write {
                    let b = unsafe { &*(buf.as_ptr() as *const embedded_sdmmc::Block) };
                    card.write(core::slice::from_ref(b), embedded_sdmmc::BlockIdx(block))
                } else {
                    let b = unsafe { &mut *(buf.as_mut_ptr() as *mut embedded_sdmmc::Block) };
                    card.read(core::slice::from_mut(b), embedded_sdmmc::BlockIdx(block))
                }
            })
            .is_ok()
    }

    // The device blocks of a read of whole consecutive blocks inside the file: the first and how
    // many. None for anything else.
    fn run_blocks(&self, offset: u32, len: usize) -> Option<(u32, u32)> {
        let size = rom_size()??;
        if offset % BLOCK as u32 != 0 || len == 0 || len % BLOCK != 0 {
            return None;
        }
        if offset as usize + len > size as usize {
            return None;
        }
        let count = (len / BLOCK) as u32;
        let layout = self.layout.as_ref()?;
        let first = layout.block_of(offset)?;
        if layout.block_of(offset + (count - 1) * BLOCK as u32)? != first + count - 1 {
            return None;
        }
        Some((first, count))
    }

    fn read_rom(&mut self, offset: u32, dst: &mut [u8]) -> bool {
        if self.layout.is_some() && self.read_direct(offset, dst) {
            return true;
        }
        self.read_slow(offset, dst)
    }

    // Read straight from the blocks of the file. The data is zero past its end.
    fn read_direct(&mut self, offset: u32, dst: &mut [u8]) -> bool {
        let Some(size) = rom_size() else {
            return false;
        };
        let size = size.unwrap_or(0);
        if offset % BLOCK as u32 == 0 && dst.len() > BLOCK && dst.len() % BLOCK == 0 {
            if let Some(ok) = self.read_run(offset, dst, size) {
                return ok;
            }
        }
        let mut done = 0;
        while done < dst.len() {
            let at = offset + done as u32;
            if at >= size {
                dst[done..].fill(0);
                break;
            }
            let Some(block) = self.layout.as_ref().and_then(|l| l.block_of(at)) else {
                return false;
            };
            if self.block_no != block {
                // the data buffer is garbage if the read fails
                self.block_no = u32::MAX;
                if !self.read_block(block, Buf::Data) {
                    return false;
                }
                self.block_no = block;
            }
            let inside = at as usize % BLOCK;
            let n = (BLOCK - inside).min(dst.len() - done);
            dst[done..done + n].copy_from_slice(&self.block[inside..inside + n]);
            done += n;
        }
        true
    }

    // Whole blocks that are next to each other on the card, in one read. None when they are not
    // (or run past the end of the file), the caller reads them one at a time then.
    fn read_run(&mut self, offset: u32, dst: &mut [u8], _size: u32) -> Option<bool> {
        use embedded_sdmmc::BlockDevice;
        let (first, count) = self.run_blocks(offset, dst.len())?;
        self.block_no = u32::MAX;
        // Safety: a Block is one array of 512 bytes with no padding
        let blocks = unsafe {
            core::slice::from_raw_parts_mut(
                dst.as_mut_ptr() as *mut embedded_sdmmc::Block,
                count as usize,
            )
        };
        Some(
            self.volumes
                .device(|card| card.read(blocks, embedded_sdmmc::BlockIdx(first)))
                .is_ok(),
        )
    }

    fn read_slow(&mut self, offset: u32, dst: &mut [u8]) -> bool {
        let Some(file) = self.file else {
            return false;
        };
        let mut done = 0;
        let result = self
            .volumes
            .file_seek_from_start(file, offset)
            .and_then(|_| {
                while done < dst.len() {
                    match self.volumes.read(file, &mut dst[done..])? {
                        0 => break,
                        n => done += n,
                    }
                }
                Ok(())
            });
        // a read past the end of the ROM is zero filled, the GBA bus reads open bus there
        dst[done..].fill(0);
        match result {
            Ok(()) => true,
            Err(e) => {
                rprintln!("ROM read at {:#x} failed: {:?}", offset, e);
                false
            }
        }
    }
}

// Run `f` with SPI1 set up for the card and the display deselected, then hand the bus back
fn with_card_bus<R>(lcd_cs: &mut impl OutputPin, f: impl FnOnce() -> R) -> R {
    lcd_cs.set_high().ok();
    sdcard::enter_bus();
    let result = f();
    crate::controls::spi1_configure(crate::controls::DISPLAY_SCR, true, true);
    lcd_cs.set_low().ok();
    result
}
