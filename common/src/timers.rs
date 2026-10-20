// The four timers: what the game wrote, and the counter worked out from the cycles when it is read
// (the counter does not run on its own here). Timer interrupts and cascading are not modelled.
//
// The cycles are the GBA's own, 16.78 MHz.

pub const TIMER_BASE: u32 = 0x100;
pub const TIMER_END: u32 = 0x110;

#[derive(Clone, Copy, Default)]
pub struct Timer {
    reload: u16,
    running: bool,
    started: u64,
    start_value: u16,
    prescale_shift: u32,
}

impl Timer {
    fn counter(&self, now: u64) -> u16 {
        if !self.running {
            return self.start_value;
        }
        let ticks = (now - self.started) >> self.prescale_shift;
        let period = 0x1_0000 - self.reload as u64;
        let first = 0x1_0000 - self.start_value as u64;
        let t = if ticks < first {
            self.start_value as u64 + ticks
        } else {
            self.reload as u64 + (ticks - first) % period
        };
        t as u16
    }
}

#[derive(Clone, Copy, Default)]
pub struct Timers([Timer; 4]);

impl Timers {
    pub const fn new() -> Self {
        Timers(
            [Timer {
                reload: 0,
                running: false,
                started: 0,
                start_value: 0,
                prescale_shift: 0,
            }; 4],
        )
    }

    // A write to the IO register at this offset in 0x100..0x110
    pub fn write(&mut self, offset: u32, size: u32, value: u32, now: u64) {
        let n = ((offset - TIMER_BASE) / 4) as usize;
        let (low, high) = match (offset % 4, size) {
            (0, 2) => (Some(value as u16), None),
            (0, 4) => (Some(value as u16), Some((value >> 16) as u16)),
            (2, 2) => (None, Some(value as u16)),
            _ => (None, None),
        };
        let t = &mut self.0[n];
        if let Some(l) = low {
            t.reload = l;
        }
        if let Some(h) = high {
            let enable = h & 0x80 != 0;
            if enable && !t.running {
                t.start_value = t.reload;
                t.started = now;
            } else if !enable && t.running {
                t.start_value = t.counter(now);
            }
            t.running = enable;
            t.prescale_shift = [0, 6, 8, 10][(h & 3) as usize];
        }
    }

    // A read of a counter register, value is what is in the register memory (the control half)
    pub fn read(&self, offset: u32, size: u32, value: u32, now: u64) -> u32 {
        if offset % 4 != 0 {
            return value;
        }
        let counter = self.0[((offset - TIMER_BASE) / 4) as usize].counter(now);
        if size == 2 {
            counter as u32
        } else {
            value & 0xFFFF_0000 | counter as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_from_the_reload_at_the_prescaler() {
        let mut t = Timers::new();
        // reload 0xFFF0, enable with the 64 cycle prescaler at cycle 1000
        t.write(0x100, 2, 0xFFF0, 0);
        t.write(0x102, 2, 0x81, 1000);
        assert_eq!(t.read(0x100, 2, 0, 1000), 0xFFF0);
        assert_eq!(t.read(0x100, 2, 0, 1000 + 64 * 5), 0xFFF5);
        // after the overflow it restarts at the reload
        assert_eq!(t.read(0x100, 2, 0, 1000 + 64 * 16), 0xFFF0);
        // stopped, it holds
        t.write(0x102, 2, 0, 1000 + 64 * 3);
        assert_eq!(t.read(0x100, 2, 0, 5000), 0xFFF3);
    }
}
