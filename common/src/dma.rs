// DMA transfers, the part games use to copy memory: channels that start right away. Transfers
// that wait for VBlank, HBlank or the sound FIFO are not run.
use crate::bios::Memory;

// The registers of a channel at 0x040000B0 + 12 * channel: source, destination, count and control
pub const DMA_BASE: u32 = 0xB0;
pub const DMA_END: u32 = 0xE0;

pub struct Dma {
    pub channel: usize,
    pub src: u32,
    pub dst: u32,
    pub count: u32,
    pub control: u16,
}

impl Dma {
    // The offset of the control halfword of a channel, the write that starts a transfer
    pub const fn control_offset(channel: usize) -> u32 {
        DMA_BASE + 12 * channel as u32 + 10
    }

    // Which channel a write to the IO register at this offset (size bytes) can start
    pub fn started_by(offset: u32, size: u32) -> Option<usize> {
        if !(DMA_BASE..DMA_END).contains(&offset) {
            return None;
        }
        let channel = ((offset - DMA_BASE) / 12) as usize;
        let ctrl = Self::control_offset(channel);
        (offset <= ctrl && offset + size > ctrl).then_some(channel)
    }

    // The channel as its registers hold it. Only the enable bit is looked at here.
    pub fn from_registers(channel: usize, src: u32, dst: u32, cnt: u32) -> Self {
        let src_mask = if channel == 0 {
            0x07FF_FFFF
        } else {
            0x0FFF_FFFF
        };
        let dst_mask = if channel == 3 {
            0x0FFF_FFFF
        } else {
            0x07FF_FFFF
        };
        let mut count = cnt & 0xFFFF;
        if count == 0 {
            count = if channel == 3 { 0x1_0000 } else { 0x4000 };
        }
        // The native stack (0x2000_0000 and up) holds the value of a fill that the game keeps in
        // a local, those addresses are not GBA addresses and must not be wrapped into one
        let wrap = |addr: u32, mask: u32| if addr >> 28 == 2 { addr } else { addr & mask };
        Dma {
            channel,
            src: wrap(src, src_mask),
            dst: wrap(dst, dst_mask),
            count,
            control: (cnt >> 16) as u16,
        }
    }

    pub fn enabled(&self) -> bool {
        self.control & 0x8000 != 0
    }

    // starts at once, not at VBlank, HBlank or for the sound FIFO
    pub fn immediate(&self) -> bool {
        (self.control >> 12) & 3 == 0
    }

    // Run the transfer, the caller clears the enable bit afterwards. Returns the units moved.
    pub fn run<M: Memory>(&self, mem: &mut M) -> u32 {
        let word = self.control & (1 << 10) != 0;
        let unit: u32 = if word { 4 } else { 2 };
        let step = |ctrl: u16| match ctrl {
            0 | 3 => unit as i32,
            1 => -(unit as i32),
            _ => 0,
        };
        let (dstep, sstep) = (step((self.control >> 5) & 3), step((self.control >> 7) & 3));
        let (mut s, mut d) = (self.src, self.dst);
        for _ in 0..self.count {
            if word {
                let v = mem.read32(s & !3);
                mem.write32(d & !3, v);
            } else {
                let v = mem.read16(s & !1);
                mem.write16(d & !1, v);
            }
            s = s.wrapping_add_signed(sstep);
            d = d.wrapping_add_signed(dstep);
        }
        self.count
    }
}

// The sound FIFO transfers: channel 1 or 2 started with the special timing (3) and a FIFO as its
// destination moves four words into that FIFO every time the FIFO runs low, whatever its count and
// size say. The source is latched when the channel is enabled and moves on with every transfer,
// while the registers keep what the game wrote; the game starts the channel again to go back to
// the start of its buffer.
pub const FIFO_A_ADDR: u32 = 0x0400_00A0;
pub const FIFO_B_ADDR: u32 = 0x0400_00A4;

#[derive(Clone, Copy)]
struct FifoChannel {
    armed: bool,
    src: u32,
    control: u16,
    // 0 for FIFO A, 1 for FIFO B
    fifo: usize,
}

// What one refill did besides moving the words
pub struct Refill {
    pub channel: usize,
    pub words: [u32; 4],
    // the channel asks for its interrupt (IF bit 8 + channel)
    pub irq: bool,
    // the channel is not repeating, so it is done and its enable bit clears
    pub stopped: bool,
}

#[derive(Clone, Copy)]
pub struct SoundDma([FifoChannel; 2]);

impl Default for SoundDma {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundDma {
    pub const fn new() -> Self {
        SoundDma(
            [FifoChannel {
                armed: false,
                src: 0,
                control: 0,
                fifo: 0,
            }; 2],
        )
    }

    // A write that can start this channel (see Dma::started_by), with its registers as they are now
    pub fn control_written(&mut self, channel: usize, src: u32, dst: u32, cnt: u32) {
        if !(1..=2).contains(&channel) {
            return;
        }
        let dma = Dma::from_registers(channel, src, dst, cnt);
        let fifo = match dma.dst {
            FIFO_A_ADDR => Some(0),
            FIFO_B_ADDR => Some(1),
            _ => None,
        };
        let special = dma.enabled() && (dma.control >> 12) & 3 == 3;
        let c = &mut self.0[channel - 1];
        match fifo.filter(|_| special) {
            Some(fifo) => {
                // only turning the channel on latches the source
                if !c.armed {
                    c.src = dma.src;
                }
                c.armed = true;
                c.control = dma.control;
                c.fifo = fifo;
            }
            None => c.armed = false,
        }
    }

    // Whether a channel is waiting to feed this FIFO
    pub fn feeds(&self, fifo: usize) -> bool {
        self.0.iter().any(|c| c.armed && c.fifo == fifo)
    }

    // A FIFO is running low: run the channel that feeds it, None when there is none
    pub fn refill<M: Memory>(&mut self, fifo: usize, mem: &mut M) -> Option<Refill> {
        let i = self.0.iter().position(|c| c.armed && c.fifo == fifo)?;
        let c = &mut self.0[i];
        let step: i32 = match (c.control >> 7) & 3 {
            1 => -4,
            2 => 0,
            _ => 4,
        };
        let mut words = [0; 4];
        for w in &mut words {
            *w = mem.read32(c.src & !3);
            c.src = c.src.wrapping_add_signed(step);
        }
        let stopped = c.control & (1 << 9) == 0;
        if stopped {
            c.armed = false;
        }
        Some(Refill {
            channel: i + 1,
            words,
            irq: c.control & (1 << 14) != 0,
            stopped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;

    #[test]
    fn sound_dma_latches_the_source_and_moves_on() {
        let mut mem = Flat((0..64).collect());
        let mut dma = SoundDma::new();
        // channel 1 to FIFO A, special timing, repeat, 32 bit
        let cnt = (0x8000 | 0x3000 | 0x0200 | 0x0400) << 16;
        dma.control_written(1, 0, FIFO_A_ADDR, cnt);
        assert!(dma.refill(1, &mut mem).is_none());
        let r = dma.refill(0, &mut mem).unwrap();
        assert_eq!((r.channel, r.stopped, r.irq), (1, false, false));
        assert_eq!(r.words[0], 0x0302_0100);
        assert_eq!(r.words[3], 0x0F0E_0D0C);
        // a write that keeps it enabled does not go back to the start
        dma.control_written(1, 0, FIFO_A_ADDR, cnt);
        assert_eq!(dma.refill(0, &mut mem).unwrap().words[0], 0x1312_1110);
        // turning it off and on again does
        dma.control_written(1, 0, FIFO_A_ADDR, 0);
        assert!(dma.refill(0, &mut mem).is_none());
        dma.control_written(1, 0, FIFO_A_ADDR, cnt);
        assert_eq!(dma.refill(0, &mut mem).unwrap().words[0], 0x0302_0100);
        // without repeat it runs once
        dma.control_written(2, 32, FIFO_B_ADDR, (0x8000 | 0x3000 | 0x4000) << 16);
        let r = dma.refill(1, &mut mem).unwrap();
        assert_eq!((r.channel, r.stopped, r.irq), (2, true, true));
        assert!(dma.refill(1, &mut mem).is_none());
    }

    struct Flat(std::vec::Vec<u8>);

    impl Memory for Flat {
        fn read8(&mut self, a: u32) -> u8 {
            self.0[a as usize]
        }
        fn read16(&mut self, a: u32) -> u16 {
            u16::from_le_bytes([self.0[a as usize], self.0[a as usize + 1]])
        }
        fn read32(&mut self, a: u32) -> u32 {
            u32::from_le_bytes(self.0[a as usize..a as usize + 4].try_into().unwrap())
        }
        fn write8(&mut self, a: u32, v: u8) {
            self.0[a as usize] = v;
        }
        fn write16(&mut self, a: u32, v: u16) {
            self.0[a as usize..a as usize + 2].copy_from_slice(&v.to_le_bytes());
        }
        fn write32(&mut self, a: u32, v: u32) {
            self.0[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    #[test]
    fn native_addresses_are_not_wrapped() {
        // a fill from a local on the stack
        let dma = Dma::from_registers(3, 0x2004_1956, 0x0600_0000, 0x800 | (0x8100 << 16));
        assert_eq!((dma.src, dma.dst), (0x2004_1956, 0x0600_0000));
        // GBA addresses still wrap at 27 bits (channel 0) or 28 bits
        let dma = Dma::from_registers(0, 0x1A00_0000, 0x0300_0000, 1);
        assert_eq!(dma.src, 0x0200_0000);
        let dma = Dma::from_registers(3, 0xF800_0000, 0x0600_0000, 1);
        assert_eq!(dma.src, 0x0800_0000);
    }

    #[test]
    fn copies_words_and_fills_with_a_fixed_source() {
        let mut mem = Flat(vec![0; 64]);
        mem.0[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        // 32 bit, both incrementing, 2 units
        let dma = Dma::from_registers(3, 0, 32, 2 | (0x8400 << 16));
        assert!(dma.enabled() && dma.immediate());
        dma.run(&mut mem);
        assert_eq!(&mem.0[32..40], &[1, 2, 3, 4, 5, 6, 7, 8]);
        // 16 bit fill: source fixed (control bits 7-8 = 2)
        let dma = Dma::from_registers(3, 0, 48, 3 | ((0x8000 | 2 << 7) << 16));
        dma.run(&mut mem);
        assert_eq!(&mem.0[48..54], &[1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn a_zero_count_is_the_maximum() {
        assert_eq!(Dma::from_registers(3, 0, 0, 0).count, 0x1_0000);
        assert_eq!(Dma::from_registers(1, 0, 0, 0).count, 0x4000);
    }

    #[test]
    fn the_control_halfword_starts_it() {
        assert_eq!(Dma::started_by(0xDE, 2), Some(3));
        assert_eq!(Dma::started_by(0xDC, 4), Some(3));
        assert_eq!(Dma::started_by(0xD4, 4), None);
        assert_eq!(Dma::started_by(0x100, 2), None);
    }
}
