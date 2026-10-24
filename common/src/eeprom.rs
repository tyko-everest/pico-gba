// The save EEPROM some games have at 0x0D000000, a serial chip that the game talks to with DMA3
// one bit per halfword (bit 0). A request is a bit string, sent as a run of writes:
//   read:  1 1, the block address (6 bits for a 4 Kbit chip, 14 for 64 Kbit), 0
//   write: 1 0, the address, 64 data bits (most significant first), 0
// A read request is answered by reads: 4 junk bits and then the 64 bits of the block. After a write
// the chip is busy until it has stored the block, reads give 0 for that, then 1. Here it is never
// busy. The chip's size is not known, the length of the request says it.
//
// Only the blocks that were written are kept, there is little RAM: a block that was never written
// reads as erased (all ones). The contents are lost at reset, they are not saved anywhere.

const BLOCKS: usize = 64;
const EMPTY: u16 = u16::MAX;

pub struct Eeprom {
    addr: [u16; BLOCKS],
    data: [u64; BLOCKS],
    // the request being sent, the bits so far in the low bits
    request: u128,
    request_len: u32,
    // the answer to a read request: `left` bits remain in the low bits of `out`
    out: u128,
    left: u32,
    // blocks that could not be kept because the store was full
    pub dropped: u32,
}

impl Eeprom {
    pub const fn new() -> Self {
        Eeprom {
            addr: [EMPTY; BLOCKS],
            data: [0; BLOCKS],
            request: 0,
            request_len: 0,
            out: 0,
            left: 0,
            dropped: 0,
        }
    }

    fn find(&self, addr: u16) -> Option<usize> {
        self.addr.iter().position(|&a| a == addr)
    }

    fn block(&self, addr: u16) -> u64 {
        self.find(addr).map_or(u64::MAX, |i| self.data[i])
    }

    fn store(&mut self, addr: u16, value: u64) {
        match self
            .find(addr)
            .or_else(|| self.addr.iter().position(|&a| a == EMPTY))
        {
            Some(i) => {
                self.addr[i] = addr;
                self.data[i] = value;
            }
            None => self.dropped += 1,
        }
    }

    // The request has been sent: do what it says
    fn finish_request(&mut self) {
        let (bits, len) = (self.request, self.request_len);
        self.request_len = 0;
        if len < 3 {
            return;
        }
        let command = (bits >> (len - 2)) & 3;
        // the address bits sit between the command and the stop bit (and the data)
        let data_bits = if command == 2 { 64 } else { 0 };
        let Some(addr_bits) = len.checked_sub(3 + data_bits) else {
            return;
        };
        if addr_bits != 6 && addr_bits != 14 {
            return;
        }
        let addr = ((bits >> (1 + data_bits)) & ((1 << addr_bits) - 1)) as u16;
        match command {
            3 => {
                // 4 junk bits, then the block
                self.out = self.block(addr) as u128;
                self.left = 68;
            }
            2 => self.store(addr, (bits >> 1) as u64),
            _ => {}
        }
    }

    // A halfword written to the chip's address range
    pub fn write_bit(&mut self, bit: u32) {
        // a new request drops an answer that was not read to the end
        self.left = 0;
        if self.request_len < 128 {
            self.request = self.request << 1 | (bit & 1) as u128;
            self.request_len += 1;
        }
    }

    // A halfword read from the chip's address range: bit 0 is the data
    pub fn read_bit(&mut self) -> u32 {
        if self.request_len > 0 {
            self.finish_request();
        }
        if self.left == 0 {
            // ready
            return 1;
        }
        self.left -= 1;
        (self.out >> self.left) as u32 & 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(e: &mut Eeprom, value: u128, bits: u32) {
        for i in (0..bits).rev() {
            e.write_bit((value >> i) as u32 & 1);
        }
    }

    fn write_block(e: &mut Eeprom, addr: u16, addr_bits: u32, data: u64) {
        send(e, 0b10, 2);
        send(e, addr as u128, addr_bits);
        send(e, data as u128, 64);
        send(e, 0, 1);
        // the game polls the status
        assert_eq!(e.read_bit(), 1);
    }

    fn read_block(e: &mut Eeprom, addr: u16, addr_bits: u32) -> u64 {
        send(e, 0b11, 2);
        send(e, addr as u128, addr_bits);
        send(e, 0, 1);
        let junk: u32 = (0..4).map(|_| e.read_bit()).sum();
        assert_eq!(junk, 0);
        (0..64).fold(0u64, |v, _| v << 1 | e.read_bit() as u64)
    }

    #[test]
    fn blocks_read_back_on_both_chip_sizes() {
        for bits in [6, 14] {
            let mut e = Eeprom::new();
            // never written: erased
            assert_eq!(read_block(&mut e, 5, bits), u64::MAX);
            write_block(&mut e, 5, bits, 0x0123_4567_89AB_CDEF);
            write_block(&mut e, 6, bits, 0xFEDC_BA98_7654_3210);
            assert_eq!(read_block(&mut e, 5, bits), 0x0123_4567_89AB_CDEF);
            assert_eq!(read_block(&mut e, 6, bits), 0xFEDC_BA98_7654_3210);
            // overwrite
            write_block(&mut e, 5, bits, 7);
            assert_eq!(read_block(&mut e, 5, bits), 7);
            assert_eq!(read_block(&mut e, 6, bits), 0xFEDC_BA98_7654_3210);
        }
    }

    #[test]
    fn idle_reads_are_ready_and_a_cut_short_read_is_dropped() {
        let mut e = Eeprom::new();
        assert_eq!(e.read_bit(), 1);
        write_block(&mut e, 1, 6, 0xFFFF_0000_FFFF_0000);
        // a read request, only a few bits read, then another request
        send(&mut e, 0b11, 2);
        send(&mut e, 1, 6);
        send(&mut e, 0, 1);
        e.read_bit();
        e.read_bit();
        assert_eq!(read_block(&mut e, 1, 6), 0xFFFF_0000_FFFF_0000);
    }

    #[test]
    fn a_full_store_drops_new_blocks() {
        let mut e = Eeprom::new();
        for a in 0..BLOCKS as u16 + 3 {
            write_block(&mut e, a, 14, a as u64);
        }
        assert_eq!(e.dropped, 3);
        assert_eq!(read_block(&mut e, 0, 14), 0);
        assert_eq!(read_block(&mut e, BLOCKS as u16 - 1, 14), BLOCKS as u64 - 1);
    }
}
