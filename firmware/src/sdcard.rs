// SD card over SPI, ported from the Waveshare Pico-ResTouch-LCD driver (MMC_SD.c).
// The card sits on SPI1 next to the display and the touch controller, with its own CS on GPIO22.
// Every call expects the other devices' CS lines to be high and leaves the card deselected.
#![allow(dead_code)] // the ROM loader is not written yet
use crate::controls;
use embedded_hal::digital::OutputPin;

// SPI1 runs at clk_peri / (2 * (1 + SCR)), see controls.rs
const INIT_SCR: u8 = 156; // 398 kHz, card initialisation must stay at or below 400 kHz
const FAST_SCR: u8 = 2; // 20.8 MHz, the SPI mode limit of a card is 25 MHz

pub const BLOCK_SIZE: usize = 512;

// how many reads had to be tried again, and why the last one failed (for the status report)
pub static RETRIES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static LAST_ERROR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn error_code(r: &Result<(), Error>) -> u32 {
    match r {
        Ok(()) => 0,
        Err(Error::Timeout) => 1,
        Err(Error::Command(r1)) => 0x100 | *r1 as u32,
        Err(Error::UnsupportedCard) => 2,
        Err(Error::WriteRejected(t)) => 0x200 | *t as u32,
        Err(Error::BadCsd) => 3,
        Err(Error::Crc) => 4,
    }
}

const CMD0: u8 = 0;
const CMD1: u8 = 1;
const CMD8: u8 = 8;
const CMD9: u8 = 9;
const CMD10: u8 = 10;
const CMD12: u8 = 12;
const CMD16: u8 = 16;
const CMD17: u8 = 17;
const CMD18: u8 = 18;
const CMD23: u8 = 23;
const CMD24: u8 = 24;
const CMD25: u8 = 25;
const CMD41: u8 = 41; // ACMD, preceded by CMD55
const CMD55: u8 = 55;
const CMD58: u8 = 58;

const R1_IDLE: u8 = 0x01;
const TOKEN_READ_START: u8 = 0xFE;
const TOKEN_WRITE_SINGLE: u8 = 0xFE;
const TOKEN_WRITE_MULTI: u8 = 0xFC;
const TOKEN_STOP_TRAN: u8 = 0xFD;
const DATA_ACCEPTED: u8 = 0x05;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    // no card answered, or the card never became ready
    Timeout,
    // the card rejected a command, holds the R1 response
    Command(u8),
    // the card answered CMD8 with a voltage range or check pattern we do not support
    UnsupportedCard,
    // the card refused a block that was written, holds the data response token
    WriteRejected(u8),
    // the CSD register could not be understood
    BadCsd,
    // a block arrived with a CRC that does not match its data
    Crc,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CardType {
    Mmc,
    V1,
    V2,
    V2Hc,
}

// Byte transfers on SPI1, with the clock speed of the card selectable
pub struct Spi1Bus;

impl Spi1Bus {
    fn configure(scr: u8) {
        controls::spi1_configure(scr, false, false);
    }

    fn transfer(byte: u8) -> u8 {
        controls::spi1_transfer(byte)
    }
}

// CRC-16/XMODEM, what the card sends after every block of data
fn crc16_update(mut crc: u16, byte: u8) -> u16 {
    crc ^= (byte as u16) << 8;
    for _ in 0..8 {
        crc = if crc & 0x8000 != 0 {
            crc << 1 ^ 0x1021
        } else {
            crc << 1
        };
    }
    crc
}

pub struct SdCard<Cs> {
    cs: Cs,
    pub card_type: CardType,
}

impl<Cs: OutputPin> SdCard<Cs> {
    // Bring the card out of its power on state and into SPI mode at full speed.
    // SPI1 must be idle, and is left in the card's configuration: the caller sets it up for the
    // display again.
    pub fn new(mut cs: Cs) -> Result<Self, Error> {
        cs.set_high().ok();
        Spi1Bus::configure(INIT_SCR);
        // at least 74 clocks with the card deselected put it in SPI mode
        for _ in 0..10 {
            Spi1Bus::transfer(0xFF);
        }

        let mut card = Self {
            cs,
            card_type: CardType::Mmc,
        };
        card.init()?;
        Spi1Bus::configure(FAST_SCR);
        Ok(card)
    }

    // Start over with a card that stopped answering properly: the card is put through its
    // initialisation again. A transfer cut short can leave it expecting something else.
    pub fn reinit(&mut self) -> Result<(), Error> {
        self.cs.set_high().ok();
        Spi1Bus::configure(INIT_SCR);
        for _ in 0..10 {
            Spi1Bus::transfer(0xFF);
        }
        let result = self.init();
        Spi1Bus::configure(FAST_SCR);
        result
    }

    fn init(&mut self) -> Result<(), Error> {
        let mut r1 = 0xFF;
        for _ in 0..20 {
            r1 = self.command(CMD0, 0, 0x95);
            if r1 == R1_IDLE {
                break;
            }
        }
        if r1 != R1_IDLE {
            self.deselect();
            return Err(if r1 == 0xFF {
                Error::Timeout
            } else {
                Error::Command(r1)
            });
        }

        let result = if self.command(CMD8, 0x1AA, 0x87) == R1_IDLE {
            // SD v2: R7 follows the R1, with the voltage range and the check pattern echoed
            let r7 = self.read_array::<4>();
            if r7[2] != 0x01 || r7[3] != 0xAA {
                Err(Error::UnsupportedCard)
            } else {
                // HCS set, the card reports whether it is high capacity once it leaves idle
                self.wait_app_init(0x4000_0000).and_then(|_| {
                    let r1 = self.command(CMD58, 0, 0x01);
                    if r1 != 0 {
                        return Err(Error::Command(r1));
                    }
                    let ocr = self.read_array::<4>();
                    self.card_type = if ocr[0] & 0x40 != 0 {
                        CardType::V2Hc
                    } else {
                        CardType::V2
                    };
                    Ok(())
                })
            }
        } else {
            self.command(CMD55, 0, 0x01);
            if self.command(CMD41, 0, 0x01) <= 1 {
                self.card_type = CardType::V1;
                self.wait_app_init(0)
            } else {
                self.card_type = CardType::Mmc;
                self.wait_mmc_init()
            }
            .and_then(|_| {
                // the block size is fixed on high capacity cards, the others must be told
                match self.command(CMD16, BLOCK_SIZE as u32, 0x01) {
                    0 => Ok(()),
                    r1 => Err(Error::Command(r1)),
                }
            })
        };
        self.deselect();
        result
    }

    // ACMD41 until the card leaves idle
    fn wait_app_init(&mut self, arg: u32) -> Result<(), Error> {
        for _ in 0..0xFFFE {
            self.command(CMD55, 0, 0x01);
            if self.command(CMD41, arg, 0x01) == 0 {
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }

    fn wait_mmc_init(&mut self) -> Result<(), Error> {
        for _ in 0..0xFFFE {
            if self.command(CMD1, 0, 0x01) == 0 {
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }

    fn read_array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0; N];
        for byte in &mut out {
            *byte = Spi1Bus::transfer(0xFF);
        }
        out
    }

    // Release the bus, the extra clocks let the card finish and release MISO
    fn deselect(&mut self) {
        self.cs.set_high().ok();
        Spi1Bus::transfer(0xFF);
    }

    // The card holds MISO low while it is busy
    fn wait_ready(&mut self) -> Result<(), Error> {
        for _ in 0..0xFF_FFFF {
            if Spi1Bus::transfer(0xFF) == 0xFF {
                return Ok(());
            }
        }
        Err(Error::Timeout)
    }

    fn select(&mut self) -> Result<(), Error> {
        self.cs.set_low().ok();
        if self.wait_ready().is_ok() {
            return Ok(());
        }
        self.deselect();
        Err(Error::Timeout)
    }

    // Send a command and return its R1 response, 0xFF when the card did not answer. The card is
    // left selected so a response or data can be read, call deselect when done.
    fn command(&mut self, cmd: u8, arg: u32, crc: u8) -> u8 {
        self.deselect();
        if self.select().is_err() {
            return 0xFF;
        }

        Spi1Bus::transfer(cmd | 0x40);
        for byte in arg.to_be_bytes() {
            Spi1Bus::transfer(byte);
        }
        Spi1Bus::transfer(crc);
        if cmd == CMD12 {
            // a stuff byte follows a stop transmission
            Spi1Bus::transfer(0xFF);
        }

        let mut r1 = 0xFF;
        for _ in 0..=0x1F {
            r1 = Spi1Bus::transfer(0xFF);
            if r1 & 0x80 == 0 {
                break;
            }
        }
        r1
    }

    // Receive a data block after a read command: wait for the start token, then the data and CRC
    fn receive(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        let mut started = false;
        for _ in 0..0xFFFF {
            if Spi1Bus::transfer(0xFF) == TOKEN_READ_START {
                started = true;
                break;
            }
        }
        if !started {
            return Err(Error::Timeout);
        }
        let mut crc = 0u16;
        for byte in buf {
            *byte = Spi1Bus::transfer(0xFF);
            crc = crc16_update(crc, *byte);
        }
        let sent = (Spi1Bus::transfer(0xFF) as u16) << 8 | Spi1Bus::transfer(0xFF) as u16;
        if sent != crc {
            return Err(Error::Crc);
        }
        Ok(())
    }

    fn send_block(&mut self, buf: &[u8; BLOCK_SIZE], token: u8) -> Result<(), Error> {
        self.wait_ready()?;
        Spi1Bus::transfer(token);
        for &byte in buf {
            Spi1Bus::transfer(byte);
        }
        Spi1Bus::transfer(0xFF);
        Spi1Bus::transfer(0xFF);
        let response = Spi1Bus::transfer(0xFF);
        if response & 0x1F != DATA_ACCEPTED {
            return Err(Error::WriteRejected(response));
        }
        Ok(())
    }

    // Standard capacity cards address by byte, high capacity ones by block
    fn address(&self, block: u32) -> u32 {
        if self.card_type == CardType::V2Hc {
            block
        } else {
            block << 9
        }
    }

    // A read that starts the card over and tries again when it fails, up to a few times. Reads
    // fail now and then on a card at full speed, and a card that went wrong stays that way.
    pub fn read_block_with_retries(
        &mut self,
        block: u32,
        dst: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), Error> {
        let mut result = Ok(());
        for attempt in 0..4 {
            result = self.read_blocks(block, core::slice::from_mut(dst));
            if result.is_ok() {
                return result;
            }
            RETRIES.store(
                RETRIES.load(core::sync::atomic::Ordering::Relaxed) + 1,
                core::sync::atomic::Ordering::Relaxed,
            );
            LAST_ERROR.store(error_code(&result), core::sync::atomic::Ordering::Relaxed);
            if attempt >= 1 {
                let _ = self.reinit();
            }
        }
        result
    }

    pub fn read_blocks(&mut self, start: u32, buf: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let result = self.read_blocks_inner(start, buf);
        self.deselect();
        result
    }

    fn read_blocks_inner(&mut self, start: u32, buf: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let address = self.address(start);
        match buf {
            [] => Ok(()),
            [block] => match self.command(CMD17, address, 0x01) {
                0 => self.receive(block),
                r1 => Err(Error::Command(r1)),
            },
            blocks => {
                match self.command(CMD18, address, 0x01) {
                    0 => {}
                    r1 => return Err(Error::Command(r1)),
                }
                let mut result = Ok(());
                for block in blocks {
                    result = self.receive(block);
                    if result.is_err() {
                        break;
                    }
                }
                // stop the stream even after a failure, or the card keeps sending
                self.command(CMD12, 0, 0x01);
                result
            }
        }
    }

    pub fn write_blocks(&mut self, start: u32, buf: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let result = self.write_blocks_inner(start, buf);
        self.deselect();
        result
    }

    fn write_blocks_inner(&mut self, start: u32, buf: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let address = self.address(start);
        match buf {
            [] => Ok(()),
            [block] => match self.command(CMD24, address, 0x01) {
                0 => self.send_block(block, TOKEN_WRITE_SINGLE),
                r1 => Err(Error::Command(r1)),
            },
            blocks => {
                if self.card_type != CardType::Mmc {
                    // pre-erase hint: ACMD23 with the number of blocks
                    self.command(CMD55, 0, 0x01);
                    self.command(CMD23, blocks.len() as u32, 0x01);
                }
                match self.command(CMD25, address, 0x01) {
                    0 => {}
                    r1 => return Err(Error::Command(r1)),
                }
                let mut result = Ok(());
                for block in blocks {
                    result = self.send_block(block, TOKEN_WRITE_MULTI);
                    if result.is_err() {
                        break;
                    }
                }
                Spi1Bus::transfer(0xFF);
                Spi1Bus::transfer(TOKEN_STOP_TRAN);
                result
            }
        }
    }

    fn read_register(&mut self, cmd: u8) -> Result<[u8; 16], Error> {
        let mut data = [0; 16];
        let result = match self.command(cmd, 0, 0x01) {
            0 => self.receive(&mut data),
            r1 => Err(Error::Command(r1)),
        };
        self.deselect();
        result.map(|_| data)
    }

    // Manufacturer information
    pub fn cid(&mut self) -> Result<[u8; 16], Error> {
        self.read_register(CMD10)
    }

    // Card specific data: capacity and speed
    pub fn csd(&mut self) -> Result<[u8; 16], Error> {
        self.read_register(CMD9)
    }

    // The number of 512 byte blocks on the card
    pub fn block_count(&mut self) -> Result<u32, Error> {
        let csd = self.csd()?;
        match csd[0] >> 6 {
            1 => {
                // CSD v2: C_SIZE is 22 bits, capacity is (C_SIZE + 1) * 512 KiB
                let c_size = u32::from_be_bytes([0, csd[7] & 0x3F, csd[8], csd[9]]);
                Ok((c_size + 1) << 10)
            }
            0 => {
                // CSD v1: capacity is (C_SIZE + 1) * 2^(C_SIZE_MULT + 2 + READ_BL_LEN) bytes
                let read_bl_len = (csd[5] & 15) as u32;
                let c_size =
                    (csd[8] as u32 >> 6) | (csd[7] as u32) << 2 | (csd[6] as u32 & 3) << 10;
                let c_size_mult = ((csd[10] as u32 & 128) >> 7) | (csd[9] as u32 & 3) << 1;
                Ok((c_size + 1) << (c_size_mult + 2 + read_bl_len - 9))
            }
            _ => Err(Error::BadCsd),
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl core::error::Error for Error {}

// Leave SPI1 set up for the card. The other devices on the bus must be deselected first and
// the caller sets the bus up for the display again afterwards.
pub fn enter_bus() {
    Spi1Bus::configure(FAST_SCR);
}

// The card as a block device for embedded-sdmmc, which wants to share it by reference.
// embedded-sdmmc only reads volumes through a partition table, but a card formatted without one
// (a "superfloppy", which is what mkfs.fat does to a whole disk) starts with the file system's
// boot sector. For those a partition table is made up: one FAT partition starting at block 1,
// and every other block is the card's block one lower.
pub struct SdDevice<Cs> {
    card: core::cell::RefCell<SdCard<Cs>>,
    superfloppy: bool,
}

impl<Cs: OutputPin> SdDevice<Cs> {
    // SPI1 must be set up for the card
    pub fn new(mut card: SdCard<Cs>) -> Self {
        let mut boot = [[0; BLOCK_SIZE]];
        let superfloppy = card.read_blocks(0, &mut boot).is_ok() && is_boot_sector(&boot[0]);
        Self {
            card: core::cell::RefCell::new(card),
            superfloppy,
        }
    }

    fn fake_mbr(&self, block: &mut [u8; BLOCK_SIZE]) -> Result<(), Error> {
        let blocks = self.card.borrow_mut().block_count()?;
        block.fill(0);
        let entry = &mut block[446..462];
        entry[4] = 0x0C; // FAT32 with LBA
        entry[8..12].copy_from_slice(&1u32.to_le_bytes());
        entry[12..16].copy_from_slice(&blocks.to_le_bytes());
        block[510] = 0x55;
        block[511] = 0xAA;
        Ok(())
    }
}

// A FAT boot sector rather than a partition table: 512 byte sectors in the BIOS parameter block
// and a file system name where FAT16 and FAT32 keep it
fn is_boot_sector(block: &[u8; BLOCK_SIZE]) -> bool {
    block[510..] == [0x55, 0xAA]
        && block[11..13] == [0x00, 0x02]
        && (&block[82..87] == b"FAT32" || &block[54..58] == b"FAT1")
}

impl<Cs: OutputPin> embedded_sdmmc::BlockDevice for SdDevice<Cs> {
    type Error = Error;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Error> {
        for (i, block) in blocks.iter_mut().enumerate() {
            let idx = start.0 + i as u32;
            if self.superfloppy && idx == 0 {
                self.fake_mbr(&mut block.contents)?;
            } else {
                let at = if self.superfloppy { idx - 1 } else { idx };
                self.card
                    .borrow_mut()
                    .read_block_with_retries(at, &mut block.contents)?;
            }
        }
        Ok(())
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Error> {
        let shift = self.superfloppy as u32;
        let mut card = self.card.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let idx = start.0 + i as u32;
            if idx >= shift {
                card.write_blocks(idx - shift, core::slice::from_ref(&block.contents))?;
            }
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Error> {
        let blocks = self.card.borrow_mut().block_count()?;
        Ok(embedded_sdmmc::BlockCount(blocks + self.superfloppy as u32))
    }
}
