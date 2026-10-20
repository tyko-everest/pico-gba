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
        Dma {
            channel,
            src: src & src_mask,
            dst: dst & dst_mask,
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

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;

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
