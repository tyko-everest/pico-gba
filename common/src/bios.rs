// The BIOS calls that work on memory, written against a trait so the firmware and the jitbench
// host model can share them. Addresses are GBA addresses, the implementation of Memory decides
// where they live.
//
// Not present on the device: the BIOS ROM itself, these are what a game gets from its SWIs.

pub trait Memory {
    fn read8(&mut self, addr: u32) -> u8;
    fn read16(&mut self, addr: u32) -> u16;
    fn read32(&mut self, addr: u32) -> u32;
    fn write8(&mut self, addr: u32, value: u8);
    fn write16(&mut self, addr: u32, value: u16);
    fn write32(&mut self, addr: u32, value: u32);
}

// The VRAM variants of the decompressors can only write 16 bits at a time, a byte write there
// goes to both bytes of the halfword on hardware. Here a byte is merged into its halfword.
fn put_byte<M: Memory>(mem: &mut M, addr: u32, value: u8, vram: bool) {
    if vram {
        let aligned = addr & !1;
        let old = mem.read16(aligned);
        let new = if addr & 1 == 0 {
            old & 0xFF00 | value as u16
        } else {
            old & 0x00FF | (value as u16) << 8
        };
        mem.write16(aligned, new);
    } else {
        mem.write8(addr, value);
    }
}

fn get_byte<M: Memory>(mem: &mut M, addr: u32, vram: bool) -> u8 {
    if vram {
        (mem.read16(addr & !1) >> (8 * (addr & 1))) as u8
    } else {
        mem.read8(addr)
    }
}

// LZ77UnCompWram (SWI 0x11) and LZ77UnCompVram (SWI 0x12)
pub fn lz77<M: Memory>(mem: &mut M, mut src: u32, mut dst: u32, vram: bool) {
    let header = mem.read32(src);
    src += 4;
    let mut remaining = (header >> 8) as i32;
    while remaining > 0 {
        let flags = mem.read8(src);
        src += 1;
        for bit in (0..8).rev() {
            if remaining <= 0 {
                break;
            }
            if flags & (1 << bit) == 0 {
                let b = mem.read8(src);
                src += 1;
                put_byte(mem, dst, b, vram);
                dst += 1;
                remaining -= 1;
            } else {
                let b0 = mem.read8(src) as u32;
                let b1 = mem.read8(src + 1) as u32;
                src += 2;
                let len = (b0 >> 4) + 3;
                let disp = ((b0 & 0xF) << 8 | b1) + 1;
                for _ in 0..len {
                    let b = get_byte(mem, dst - disp, vram);
                    put_byte(mem, dst, b, vram);
                    dst += 1;
                    remaining -= 1;
                    if remaining <= 0 {
                        break;
                    }
                }
            }
        }
    }
}

// RLUnCompWram (SWI 0x14) and RLUnCompVram (SWI 0x15)
pub fn run_length<M: Memory>(mem: &mut M, mut src: u32, mut dst: u32, vram: bool) {
    let header = mem.read32(src);
    src += 4;
    let mut remaining = (header >> 8) as i32;
    while remaining > 0 {
        let flag = mem.read8(src);
        src += 1;
        let len = (flag & 0x7F) as i32;
        if flag & 0x80 != 0 {
            let value = mem.read8(src);
            src += 1;
            for _ in 0..len + 3 {
                if remaining <= 0 {
                    break;
                }
                put_byte(mem, dst, value, vram);
                dst += 1;
                remaining -= 1;
            }
        } else {
            for _ in 0..len + 1 {
                if remaining <= 0 {
                    break;
                }
                let value = mem.read8(src);
                src += 1;
                put_byte(mem, dst, value, vram);
                dst += 1;
                remaining -= 1;
            }
        }
    }
}

// HuffUnComp (SWI 0x13)
pub fn huffman<M: Memory>(mem: &mut M, mut src: u32, mut dst: u32) {
    let header = mem.read32(src);
    let bits = header & 0xF; // 4 or 8 bits per symbol
    let mut remaining = (header >> 8) as i32;
    let tree = src + 4;
    let tree_size = (mem.read8(tree) as u32 + 1) * 2;
    src = tree + tree_size;
    // symbols come out in 32 bit words
    let mut word = 0u32;
    let mut filled = 0;
    let mut node = tree + 1;
    while remaining > 0 {
        let bits_word = mem.read32(src);
        src += 4;
        for bit in (0..32).rev() {
            if remaining <= 0 {
                break;
            }
            let info = mem.read8(node);
            let child = (node & !1) + (info as u32 & 0x3F) * 2 + 2;
            let right = bits_word >> bit & 1 != 0;
            let (next, is_data) = if right {
                (child + 1, info & 0x40 != 0)
            } else {
                (child, info & 0x80 != 0)
            };
            if is_data {
                let symbol = mem.read8(next) as u32;
                word |= symbol << filled;
                filled += bits;
                node = tree + 1;
                if filled == 32 {
                    mem.write32(dst, word);
                    dst += 4;
                    remaining -= 4;
                    word = 0;
                    filled = 0;
                }
            } else {
                node = next;
            }
        }
    }
}

pub fn sqrt(value: u32) -> u32 {
    let mut root = 0u32;
    let mut bit = 1u32 << 30;
    let mut rest = value;
    while bit > rest {
        bit >>= 2;
    }
    while bit != 0 {
        if rest >= root + bit {
            rest -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root
}

// ArcTan of a 1.14 fixed point tangent, the polynomial the BIOS uses
pub fn arctan(tan: i32) -> i32 {
    let a = -((tan.wrapping_mul(tan)) >> 14);
    let mut b = ((0xA9 * a) >> 14) + 0x390;
    for c in [0x91C, 0xFB6, 0x16AA, 0x2081, 0x3651, 0xA2F9] {
        b = ((a.wrapping_mul(b)) >> 14) + c;
    }
    tan.wrapping_mul(b) >> 16
}

// ArcTan2: the angle of (x, y) as 0..0xFFFF for a full circle
pub fn arctan2(x: i32, y: i32) -> u32 {
    let r = if y == 0 {
        if x >= 0 { 0 } else { 0x8000 }
    } else if x == 0 {
        if y >= 0 { 0x4000 } else { 0xC000 }
    } else if y >= 0 {
        if x >= 0 {
            if x >= y {
                arctan(((y as i64) << 14) as i32 / x)
            } else {
                0x4000 - arctan(((x as i64) << 14) as i32 / y)
            }
        } else if -x >= y {
            arctan(((y as i64) << 14) as i32 / x) + 0x8000
        } else {
            0x4000 - arctan(((x as i64) << 14) as i32 / y)
        }
    } else if x <= 0 {
        if -x > -y {
            arctan(((y as i64) << 14) as i32 / x) + 0x8000
        } else {
            0xC000 - arctan(((x as i64) << 14) as i32 / y)
        }
    } else if x >= -y {
        arctan(((y as i64) << 14) as i32 / x) + 0x10000
    } else {
        0xC000 - arctan(((x as i64) << 14) as i32 / y)
    };
    r as u32 & 0xFFFF
}

// sin of a 16 bit angle in 1.14 fixed point: 0x4000 is 1.0
const SIN_STEPS: usize = 256;
const SIN_TABLE: [i16; SIN_STEPS] = {
    let mut t = [0i16; SIN_STEPS];
    let mut i = 0;
    while i < SIN_STEPS {
        // Taylor series of sin(2 pi i / 256) over the first quadrant, mirrored for the rest
        let q = i % 128;
        let q = if q > 64 { 128 - q } else { q };
        let x = q as f64 * (core::f64::consts::PI / 128.0);
        let x2 = x * x;
        let mut term = x;
        let mut sum = x;
        let mut n = 1;
        while n < 12 {
            term = -term * x2 / ((2 * n) as f64 * (2 * n + 1) as f64);
            sum += term;
            n += 1;
        }
        let v = (sum * 16384.0 + if sum >= 0.0 { 0.5 } else { -0.5 }) as i32;
        t[i] = if i >= 128 { -(v as i16) } else { v as i16 };
        i += 1;
    }
    t
};

// (sin, cos) of a 16 bit angle, 1.14 fixed point
pub fn sin_cos(angle: u16) -> (i32, i32) {
    let i = (angle >> 8) as usize;
    (SIN_TABLE[i] as i32, SIN_TABLE[(i + 64) % SIN_STEPS] as i32)
}

// ObjAffineSet (SWI 0x0F): count entries of (sx, sy, angle) halfwords read from src, written as
// pa, pb, pc, pd halfwords `stride` bytes apart
pub fn obj_affine_set<M: Memory>(mem: &mut M, mut src: u32, mut dst: u32, count: u32, stride: u32) {
    for _ in 0..count {
        let sx = mem.read16(src) as i16 as i32;
        let sy = mem.read16(src + 2) as i16 as i32;
        let angle = mem.read16(src + 4);
        src += 8;
        let (sin, cos) = sin_cos(angle);
        let pa = (sx * cos) >> 14;
        let pb = -((sx * sin) >> 14);
        let pc = (sy * sin) >> 14;
        let pd = (sy * cos) >> 14;
        mem.write16(dst, pa as u16);
        mem.write16(dst + stride, pb as u16);
        mem.write16(dst + 2 * stride, pc as u16);
        mem.write16(dst + 3 * stride, pd as u16);
        dst += 4 * stride;
    }
}

// BgAffineSet (SWI 0x0E): entries of 20 bytes (origin x, y as 24.8, centre x, y, scale x, y as
// halfwords, angle) become 16 byte matrices (pa, pb, pc, pd, start x, start y)
pub fn bg_affine_set<M: Memory>(mem: &mut M, mut src: u32, mut dst: u32, count: u32) {
    for _ in 0..count {
        let ox = mem.read32(src) as i32;
        let oy = mem.read32(src + 4) as i32;
        let cx = mem.read16(src + 8) as i16 as i32;
        let cy = mem.read16(src + 10) as i16 as i32;
        let sx = mem.read16(src + 12) as i16 as i32;
        let sy = mem.read16(src + 14) as i16 as i32;
        let angle = mem.read16(src + 16);
        src += 20;
        let (sin, cos) = sin_cos(angle);
        let pa = (sx * cos) >> 14;
        let pb = -((sx * sin) >> 14);
        let pc = (sy * sin) >> 14;
        let pd = (sy * cos) >> 14;
        mem.write16(dst, pa as u16);
        mem.write16(dst + 2, pb as u16);
        mem.write16(dst + 4, pc as u16);
        mem.write16(dst + 6, pd as u16);
        let dx = ox - (pa * cx + pb * cy);
        let dy = oy - (pc * cx + pd * cy);
        mem.write32(dst + 8, dx as u32);
        mem.write32(dst + 12, dy as u32);
        dst += 16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    struct Flat(Vec<u8>);

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
    fn lz77_literals_and_a_reference() {
        let mut mem = Flat(std::vec![0; 64]);
        // size 8: 'a' 'b' then a copy of 6 bytes from 2 back
        let data = [0x10, 8, 0, 0, 0b0010_0000, b'a', b'b', 0x30 | 0, 1];
        mem.0[..data.len()].copy_from_slice(&data);
        lz77(&mut mem, 0, 32, false);
        assert_eq!(&mem.0[32..40], b"abababab");
    }

    #[test]
    fn run_length_runs_and_literals() {
        let mut mem = Flat(std::vec![0; 64]);
        let data = [0x30, 7, 0, 0, 0x81, 9, 0x01, 1, 2];
        mem.0[..data.len()].copy_from_slice(&data);
        run_length(&mut mem, 0, 32, false);
        assert_eq!(&mem.0[32..39], &[9, 9, 9, 9, 1, 2, 0]);
    }

    #[test]
    fn sin_table_is_close() {
        assert_eq!(sin_cos(0), (0, 0x4000));
        assert_eq!(sin_cos(0x4000), (0x4000, 0));
        let (s, _) = sin_cos(0x2000);
        assert!((s - 11585).abs() <= 1);
    }

    #[test]
    fn sqrt_and_arctan() {
        assert_eq!(sqrt(144), 12);
        assert_eq!(sqrt(0xFFFF_FFFF), 65535);
        assert_eq!(arctan2(1, 0), 0);
        assert_eq!(arctan2(0, 1), 0x4000);
        assert_eq!(arctan2(-1, 0), 0x8000);
    }
}
