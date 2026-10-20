// The part of a 128 KiB flash save chip that a game needs to start: its identification. Games
// detect the chip by putting it in ID mode (0xAA to 0x5555, 0x55 to 0x2AAA, 0x90 to 0x5555) and
// reading the maker and device from the first two bytes of the save area. Erase, program and
// reads of the data are not emulated, the save area stays blank.

// Macronix MX29L010, one of the two chips FLASH1M games accept
const MAKER: u8 = 0xC2;
const DEVICE: u8 = 0x09;

#[derive(Default)]
pub struct FlashChip {
    // how far into a command sequence the writes are
    step: u8,
    id_mode: bool,
}

impl FlashChip {
    pub const fn new() -> Self {
        Self {
            step: 0,
            id_mode: false,
        }
    }

    // A byte written to the save area at this offset
    pub fn write(&mut self, offset: u32, value: u8) {
        match (self.step, offset, value) {
            (0, 0x5555, 0xAA) => self.step = 1,
            (1, 0x2AAA, 0x55) => self.step = 2,
            (2, 0x5555, 0x90) => {
                self.id_mode = true;
                self.step = 0;
            }
            (2, 0x5555, 0xF0) => {
                self.id_mode = false;
                self.step = 0;
            }
            (_, _, 0xF0) => {
                self.id_mode = false;
                self.step = 0;
            }
            _ => self.step = 0,
        }
    }

    // The byte read from the save area at this offset when the chip is answering with its ID
    pub fn read(&self, offset: u32) -> Option<u8> {
        if !self.id_mode {
            return None;
        }
        match offset {
            0 => Some(MAKER),
            1 => Some(DEVICE),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_mode(chip: &mut FlashChip) {
        chip.write(0x5555, 0xAA);
        chip.write(0x2AAA, 0x55);
        chip.write(0x5555, 0x90);
    }

    #[test]
    fn answers_with_its_id_in_id_mode_only() {
        let mut chip = FlashChip::new();
        assert_eq!(chip.read(0), None);
        id_mode(&mut chip);
        assert_eq!((chip.read(0), chip.read(1)), (Some(0xC2), Some(0x09)));
        chip.write(0x5555, 0xF0);
        assert_eq!(chip.read(0), None);
    }

    #[test]
    fn a_broken_sequence_does_not_enter_id_mode() {
        let mut chip = FlashChip::new();
        chip.write(0x5555, 0xAA);
        chip.write(0x2AAB, 0x55);
        chip.write(0x5555, 0x90);
        assert_eq!(chip.read(0), None);
    }
}
