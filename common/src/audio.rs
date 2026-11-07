// The sound hardware: the four PSG channels (two square waves, the wave RAM channel and noise) and
// the two Direct Sound FIFOs, mixed to stereo samples at 32768 Hz like the GBA's own output.
//
// The time is the GBA's 16.78 MHz cycle count, and the host decides where it comes from: the
// simulation counts it, the device reads it off a clock. Nothing here runs by itself. The host
//
//   - calls `run(now, ..)` to bring the sound up to `now`: the FIFOs play a sample at every
//     overflow of their timer (worked out from `Timers`, which do not run by themselves either),
//     a FIFO that runs low is refilled by its DMA channel (`SoundDma`) out of GBA memory, and
//     the output samples up to `now` go to the sink,
//   - calls `run` first whenever the game writes a register that changes what has been playing
//     (`syncs_audio` says which: sound, timers 0 and 1, DMA 1 and 2), then hands the write to
//     `write`, the timers and `dma_control`, so what came before the write plays with the old
//     settings,
//   - puts the interrupt flags and DMA enable bits that `run` returns back into its registers.
//
// The sound registers from 0x60 to 0x8F are kept here, and `read` gives what the game reads back.
use crate::bios::Memory;
use crate::dma::SoundDma;
use crate::timers::Timers;

pub const SOUND_BASE: u32 = 0x60;
pub const SOUND_END: u32 = 0xA8;
const REGS_END: u32 = 0x90;
const WAVE_RAM: u32 = 0x90;
const FIFO_A: u32 = 0xA0;
const FIFO_B: u32 = 0xA4;

pub const SAMPLE_RATE: u32 = 32768;
pub const CYCLES_PER_SAMPLE: u64 = (1 << 24) / SAMPLE_RATE as u64;
// the frame sequencer that clocks length, sweep and envelope runs at 512 Hz
const SEQUENCER_CYCLES: u64 = 1 << 15;

// SOUNDCNT_X: the master enable
const MASTER_ON: u16 = 0x80;

// Whether a write to the IO register at this offset changes what plays, so the host has to bring
// the sound up to date before it
pub fn syncs_audio(offset: u32, size: u32) -> bool {
    let hit = |lo: u32, hi: u32| offset < hi && offset + size > lo;
    // timers 0 and 1, DMA channels 1 and 2
    hit(SOUND_BASE, SOUND_END) || hit(0x100, 0x108) || hit(0xBC, 0xD4)
}

pub trait Sink {
    fn sample(&mut self, left: i16, right: i16);
}

impl<F: FnMut(i16, i16)> Sink for F {
    fn sample(&mut self, left: i16, right: i16) {
        self(left, right)
    }
}

// What the host has to do to its registers after a run
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Effects {
    // interrupt flags to raise in IF
    pub irq: u16,
    // DMA channels (bit n for channel n) that finished, their enable bit clears
    pub dma_stopped: u8,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub samples: u64,
    pub pops: [u64; 2],
    // a sample was due and the FIFO was empty although a DMA channel feeds it, so the last one
    // played again (the game falling behind). An empty FIFO with no DMA is the sound stopped.
    pub underruns: [u64; 2],
    // bytes written to a full FIFO, which are lost
    pub overflows: [u64; 2],
    pub refills: [u64; 2],
}

#[derive(Clone, Copy)]
struct Fifo {
    bytes: [u8; 32],
    start: u8,
    len: u8,
    // what is playing, until the next timer overflow
    sample: i8,
}

impl Fifo {
    const EMPTY: Fifo = Fifo {
        bytes: [0; 32],
        start: 0,
        len: 0,
        sample: 0,
    };

    fn push(&mut self, b: u8) -> bool {
        if self.len == 32 {
            return false;
        }
        self.bytes[((self.start + self.len) % 32) as usize] = b;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<i8> {
        if self.len == 0 {
            return None;
        }
        let b = self.bytes[self.start as usize] as i8;
        self.start = (self.start + 1) % 32;
        self.len -= 1;
        Some(b)
    }

    fn reset(&mut self) {
        self.start = 0;
        self.len = 0;
    }
}

#[derive(Clone, Copy)]
struct Envelope {
    volume: u8,
    period: u8,
    up: bool,
    timer: u8,
}

impl Envelope {
    // from the register with the initial volume in bits 12-15, the direction in bit 11 and the
    // step time in bits 8-10
    fn start(&mut self, reg: u16) {
        self.volume = (reg >> 12) as u8;
        self.up = reg & 0x800 != 0;
        self.period = ((reg >> 8) & 7) as u8;
        self.timer = self.period;
    }

    fn clock(&mut self) {
        if self.period == 0 {
            return;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 {
            self.timer = self.period;
            if self.up && self.volume < 15 {
                self.volume += 1;
            } else if !self.up && self.volume > 0 {
                self.volume -= 1;
            }
        }
    }
}

// The channel is silent for good with an initial volume of 0 going down
fn dac_on(envelope_reg: u16) -> bool {
    envelope_reg & 0xF800 != 0
}

#[derive(Clone, Copy)]
struct Channel {
    on: bool,
    length: u16,
    length_on: bool,
    env: Envelope,
    // cycles into the current step, and the step: the position in the duty cycle or the wave
    acc: u32,
    pos: u8,
}

impl Channel {
    const OFF: Channel = Channel {
        on: false,
        length: 0,
        length_on: false,
        env: Envelope {
            volume: 0,
            period: 0,
            up: false,
            timer: 0,
        },
        acc: 0,
        pos: 0,
    };

    fn clock_length(&mut self) {
        if self.length_on && self.length > 0 {
            self.length -= 1;
            if self.length == 0 {
                self.on = false;
            }
        }
    }

    // Move on by one output sample, the steps being `period` cycles long (0 stands still)
    fn advance(&mut self, period: u32, steps: u8) -> u32 {
        let mut n = 0;
        if period != 0 {
            self.acc += CYCLES_PER_SAMPLE as u32;
            while self.acc >= period {
                self.acc -= period;
                n += 1;
            }
            self.pos = ((self.pos as u32 + n) % steps as u32) as u8;
        }
        n
    }
}

#[derive(Clone, Copy)]
struct Sweep {
    shadow: u16,
    timer: u8,
    enabled: bool,
}

// the four duty cycles of the square channels, bit n is step n
const DUTY: [u8; 4] = [0b0000_0001, 0b1000_0001, 0b1000_0111, 0b0111_1110];
// the longest length of each channel
const MAX_LENGTH: [u16; 4] = [64, 64, 256, 64];

// offsets of the registers of each channel
const SQUARE_ENV: [u32; 2] = [0x62, 0x68];
const SQUARE_FREQ: [u32; 2] = [0x64, 0x6C];
const SWEEP: u32 = 0x60;
const WAVE_CNT: u32 = 0x70;
const WAVE_VOL: u32 = 0x72;
const WAVE_FREQ: u32 = 0x74;
const NOISE_ENV: u32 = 0x78;
const NOISE_FREQ: u32 = 0x7C;
const SOUNDCNT_L: u32 = 0x80;
const SOUNDCNT_H: u32 = 0x82;
const SOUNDCNT_X: u32 = 0x84;
const SOUNDBIAS: u32 = 0x88;

pub struct Apu {
    // the registers from 0x60 to 0x8F as written
    regs: [u8; (REGS_END - SOUND_BASE) as usize],
    wave: [[u8; 16]; 2],
    ch: [Channel; 4],
    sweep: Sweep,
    lfsr: u16,
    fifo: [Fifo; 2],
    pub dma: SoundDma,
    // the sound has been worked out up to this cycle
    now: u64,
    next_sample: u64,
    next_step: u64,
    step: u8,
    pub stats: Stats,
}

impl Apu {
    // The sound as the BIOS leaves it, starting at cycle `now`
    pub const fn new(now: u64) -> Self {
        let mut regs = [0; (REGS_END - SOUND_BASE) as usize];
        // the bias is halfway
        regs[(SOUNDBIAS - SOUND_BASE + 1) as usize] = 0x02;
        Apu {
            regs,
            wave: [[0; 16]; 2],
            ch: [Channel::OFF; 4],
            sweep: Sweep {
                shadow: 0,
                timer: 0,
                enabled: false,
            },
            lfsr: 0,
            fifo: [Fifo::EMPTY; 2],
            dma: SoundDma::new(),
            now,
            next_sample: now + CYCLES_PER_SAMPLE,
            next_step: now + SEQUENCER_CYCLES,
            step: 0,
            stats: Stats {
                samples: 0,
                pops: [0; 2],
                underruns: [0; 2],
                overflows: [0; 2],
                refills: [0; 2],
            },
        }
    }

    // RegisterRamReset of the sound registers: everything as the BIOS leaves it, but the time
    // goes on, and the DMA channels have registers of their own
    pub fn reset(&mut self) {
        let fresh = Apu::new(self.now);
        *self = Apu {
            dma: self.dma,
            next_sample: self.next_sample,
            next_step: self.next_step,
            step: self.step,
            stats: self.stats,
            ..fresh
        };
    }

    fn reg(&self, offset: u32) -> u16 {
        let i = (offset - SOUND_BASE) as usize;
        u16::from_le_bytes([self.regs[i], self.regs[i + 1]])
    }

    fn set_reg(&mut self, offset: u32, value: u16) {
        let i = (offset - SOUND_BASE) as usize;
        self.regs[i..i + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn master_on(&self) -> bool {
        self.reg(SOUNDCNT_X) & MASTER_ON != 0
    }

    // Work the sound out up to cycle `until`
    pub fn run<M: Memory, S: Sink>(
        &mut self,
        until: u64,
        timers: &Timers,
        mem: &mut M,
        out: &mut S,
    ) -> Effects {
        let mut fx = Effects::default();
        loop {
            let (tick, timers_due) = self.next_tick(timers);
            let t = self.next_sample.min(self.next_step).min(tick);
            if t > until {
                break;
            }
            self.now = t;
            if tick == t {
                self.tick(timers_due, mem, &mut fx);
            }
            if self.next_step == t {
                self.sequencer();
                self.next_step += SEQUENCER_CYCLES;
            }
            if self.next_sample == t {
                let (l, r) = self.mix();
                out.sample(l, r);
                self.stats.samples += 1;
                self.next_sample += CYCLES_PER_SAMPLE;
            }
        }
        self.now = self.now.max(until);
        fx
    }

    // The timer of each FIFO: SOUNDCNT_H bit 10 for A, bit 14 for B
    fn fifo_timer(&self, fifo: usize) -> usize {
        (self.reg(SOUNDCNT_H) >> (10 + 4 * fifo)) as usize & 1
    }

    // The next overflow of a timer a FIFO plays from, and which timers (bit n) overflow then
    fn next_tick(&self, timers: &Timers) -> (u64, u8) {
        if !self.master_on() {
            return (u64::MAX, 0);
        }
        let mut best = (u64::MAX, 0);
        for n in 0..2 {
            if self.fifo_timer(0) != n && self.fifo_timer(1) != n {
                continue;
            }
            if let Some(at) = timers.next_overflow(n, self.now) {
                if at < best.0 {
                    best = (at, 1 << n);
                } else if at == best.0 {
                    best.1 |= 1 << n;
                }
            }
        }
        best
    }

    // Timers overflowed: the FIFOs on them play their next sample, and ask for more when half empty
    fn tick<M: Memory>(&mut self, timers_due: u8, mem: &mut M, fx: &mut Effects) {
        for f in 0..2 {
            if timers_due & (1 << self.fifo_timer(f)) == 0 {
                continue;
            }
            match self.fifo[f].pop() {
                Some(s) => {
                    self.fifo[f].sample = s;
                    self.stats.pops[f] += 1;
                }
                None if self.dma.feeds(f) => self.stats.underruns[f] += 1,
                None => {}
            }
            if self.fifo[f].len <= 16
                && let Some(r) = self.dma.refill(f, mem)
            {
                self.stats.refills[f] += 1;
                for w in r.words {
                    for b in w.to_le_bytes() {
                        if !self.fifo[f].push(b) {
                            self.stats.overflows[f] += 1;
                        }
                    }
                }
                if r.irq {
                    fx.irq |= 1 << (8 + r.channel);
                }
                if r.stopped {
                    fx.dma_stopped |= 1 << r.channel;
                }
            }
        }
    }

    // A step of the 512 Hz frame sequencer: lengths at 256 Hz, the sweep at 128 Hz and the
    // envelopes at 64 Hz
    fn sequencer(&mut self) {
        let step = self.step;
        self.step = (step + 1) % 8;
        if !self.master_on() {
            return;
        }
        if step.is_multiple_of(2) {
            for c in &mut self.ch {
                c.clock_length();
            }
        }
        if step == 2 || step == 6 {
            self.clock_sweep();
        }
        if step == 7 {
            for i in [0, 1, 3] {
                self.ch[i].env.clock();
            }
        }
    }

    fn sweep_target(&self) -> u16 {
        let reg = self.reg(SWEEP);
        let delta = self.sweep.shadow >> (reg & 7);
        if reg & 8 != 0 {
            self.sweep.shadow.wrapping_sub(delta)
        } else {
            self.sweep.shadow + delta
        }
    }

    fn start_sweep(&mut self) {
        let reg = self.reg(SWEEP);
        let (period, shift) = ((reg >> 4) & 7, reg & 7);
        self.sweep.shadow = self.reg(SQUARE_FREQ[0]) & 0x7FF;
        self.sweep.timer = if period == 0 { 8 } else { period as u8 };
        self.sweep.enabled = period != 0 || shift != 0;
        if shift != 0 && self.sweep_target() > 2047 {
            self.ch[0].on = false;
        }
    }

    fn clock_sweep(&mut self) {
        let reg = self.reg(SWEEP);
        let (period, shift) = ((reg >> 4) & 7, reg & 7);
        self.sweep.timer = self.sweep.timer.saturating_sub(1);
        if self.sweep.timer != 0 {
            return;
        }
        self.sweep.timer = if period == 0 { 8 } else { period as u8 };
        if !self.sweep.enabled || period == 0 {
            return;
        }
        let target = self.sweep_target();
        if target > 2047 {
            self.ch[0].on = false;
        } else if shift != 0 {
            self.sweep.shadow = target;
            let freq = self.reg(SQUARE_FREQ[0]);
            self.set_reg(SQUARE_FREQ[0], freq & !0x7FF | target);
            if self.sweep_target() > 2047 {
                self.ch[0].on = false;
            }
        }
    }

    // The channels' outputs for the next sample, each about -16..16
    fn square(&mut self, i: usize) -> i32 {
        let freq = (self.reg(SQUARE_FREQ[i]) & 0x7FF) as u32;
        let duty = DUTY[(self.reg(SQUARE_ENV[i]) >> 6) as usize & 3];
        let c = &mut self.ch[i];
        c.advance(16 * (2048 - freq), 8);
        if !c.on {
            return 0;
        }
        let v = c.env.volume as i32;
        if duty >> c.pos & 1 != 0 { v } else { -v }
    }

    fn wave_out(&mut self) -> i32 {
        let cnt = self.reg(WAVE_CNT);
        let vol = self.reg(WAVE_VOL);
        let freq = (self.reg(WAVE_FREQ) & 0x7FF) as u32;
        // both banks make one wave of 64 samples, starting at the selected one
        let two_banks = cnt & 0x20 != 0;
        let c = &mut self.ch[2];
        c.advance(8 * (2048 - freq), if two_banks { 64 } else { 32 });
        if !c.on || cnt & 0x80 == 0 {
            return 0;
        }
        let pos = c.pos as usize;
        let bank = ((cnt >> 6) as usize + pos / 32) & 1;
        let byte = self.wave[bank][(pos % 32) / 2];
        let s = if pos.is_multiple_of(2) {
            byte >> 4
        } else {
            byte & 15
        };
        let centered = (s as i32 - 8) * 2;
        if vol & 0x8000 != 0 {
            return centered * 3 / 4;
        }
        match (vol >> 13) & 3 {
            0 => 0,
            1 => centered,
            2 => centered / 2,
            _ => centered / 4,
        }
    }

    fn noise_out(&mut self) -> i32 {
        let reg = self.reg(NOISE_FREQ);
        let (ratio, shift) = ((reg & 7) as u32, (reg >> 4) & 15);
        // 524288 Hz / ratio / 2^(shift + 1), with a ratio of 0 counting as a half
        let period = if shift >= 14 {
            0
        } else if ratio == 0 {
            16 << (shift + 1)
        } else {
            (32 * ratio) << (shift + 1)
        };
        let short = reg & 8 != 0;
        let steps = self.ch[3].advance(period, 1);
        for _ in 0..steps {
            let bit = (self.lfsr ^ (self.lfsr >> 1)) & 1;
            self.lfsr = (self.lfsr >> 1) | bit << 14;
            if short {
                self.lfsr = self.lfsr & !(1 << 6) | bit << 6;
            }
        }
        let c = &self.ch[3];
        if !c.on {
            return 0;
        }
        let v = c.env.volume as i32;
        if self.lfsr & 1 == 0 { v } else { -v }
    }

    fn mix(&mut self) -> (i16, i16) {
        if !self.master_on() {
            return (0, 0);
        }
        let psg = [
            self.square(0),
            self.square(1),
            self.wave_out(),
            self.noise_out(),
        ];
        let lr = self.reg(SOUNDCNT_L);
        let h = self.reg(SOUNDCNT_H);
        let bias = (self.reg(SOUNDBIAS) & 0x3FE) as i32;
        // the PSG at 25, 50 or 100%
        let psg_shift = match h & 3 {
            0 => 2,
            1 => 1,
            _ => 0,
        };
        let mut out = [0i16; 2];
        // side 0 is right, 1 is left
        for (side, o) in out.iter_mut().enumerate() {
            let enabled = (lr >> (8 + 4 * side)) & 15;
            let volume = ((lr >> (4 * side)) & 7) as i32;
            let sum: i32 = (0..4)
                .filter(|i| enabled >> i & 1 != 0)
                .map(|i| psg[i])
                .sum();
            let mut s = (sum * (1 + volume)) >> psg_shift;
            for f in 0..2 {
                if h >> (8 + 4 * f + side) & 1 != 0 {
                    // 50 or 100%
                    let full = h >> (2 + f) & 1 != 0;
                    s += self.fifo[f].sample as i32 * if full { 4 } else { 2 };
                }
            }
            // the output is 10 bits around the bias
            *o = (((s + bias).clamp(0, 0x3FF) - bias) * 32) as i16;
        }
        (out[1], out[0])
    }

    // A write by the game to the IO register at this offset, in 0x60..0xA8. Run up to the time
    // of the write first.
    pub fn write(&mut self, offset: u32, size: u32, value: u32) {
        for i in 0..size {
            self.write8(offset + i, (value >> (8 * i)) as u8);
        }
    }

    fn write8(&mut self, offset: u32, b: u8) {
        match offset {
            FIFO_A..=0xA3 | FIFO_B..=0xA7 => {
                let f = ((offset - FIFO_A) / 4) as usize;
                if !self.fifo[f].push(b) {
                    self.stats.overflows[f] += 1;
                }
            }
            WAVE_RAM..=0x9F => {
                // the game reaches the bank that is not playing
                let bank = 1 - ((self.reg(WAVE_CNT) >> 6) & 1) as usize;
                self.wave[bank][(offset - WAVE_RAM) as usize] = b;
            }
            SOUND_BASE..=0x8F => self.write_reg(offset, b),
            _ => {}
        }
    }

    fn write_reg(&mut self, offset: u32, b: u8) {
        // with the sound off only the DMA sound control, the master enable and the bias take writes
        if !self.master_on() && offset < SOUNDCNT_H {
            return;
        }
        self.regs[(offset - SOUND_BASE) as usize] = b;
        match offset {
            0x62 => self.ch[0].length = 64 - (b & 63) as u16,
            0x68 => self.ch[1].length = 64 - (b & 63) as u16,
            0x72 => self.ch[2].length = 256 - b as u16,
            0x78 => self.ch[3].length = 64 - (b & 63) as u16,
            // the high byte of an envelope register can turn the channel off
            0x63 | 0x69 | 0x79 => {
                let env = offset & !1;
                if !dac_on(self.reg(env)) {
                    let i = [0, 1, 3][((offset - 0x63) / 6).min(2) as usize];
                    self.ch[i].on = false;
                }
            }
            0x70 if b & 0x80 == 0 => self.ch[2].on = false,
            0x65 => self.restart(0, b),
            0x6D => self.restart(1, b),
            0x75 => self.restart(2, b),
            0x7D => self.restart(3, b),
            // FIFO resets, which do not stay set
            0x83 => {
                for f in 0..2 {
                    if b & (0x08 << (4 * f)) != 0 {
                        self.fifo[f].reset();
                    }
                }
                self.regs[(offset - SOUND_BASE) as usize] = b & 0x77;
            }
            0x84 if b & 0x80 == 0 => {
                // turning the sound off clears the PSG registers
                self.regs[..(SOUNDCNT_H - SOUND_BASE) as usize].fill(0);
                self.ch = [Channel::OFF; 4];
            }
            _ => {}
        }
    }

    // The high byte of a channel's control register: the length enable, and bit 7 restarts it
    fn restart(&mut self, i: usize, b: u8) {
        self.ch[i].length_on = b & 0x40 != 0;
        if b & 0x80 == 0 {
            return;
        }
        let c = &mut self.ch[i];
        if c.length == 0 {
            c.length = MAX_LENGTH[i];
        }
        c.acc = 0;
        c.pos = 0;
        match i {
            0 | 1 => {
                let env = self.reg(SQUARE_ENV[i]);
                self.ch[i].env.start(env);
                self.ch[i].on = dac_on(env);
                if i == 0 {
                    self.start_sweep();
                }
            }
            2 => self.ch[2].on = self.reg(WAVE_CNT) & 0x80 != 0,
            _ => {
                let env = self.reg(NOISE_ENV);
                self.ch[3].env.start(env);
                self.ch[3].on = dac_on(env);
                self.lfsr = if self.reg(NOISE_FREQ) & 8 != 0 {
                    0x7F
                } else {
                    0x7FFF
                };
            }
        }
    }

    // What the game reads from the IO register at this offset, in 0x60..0xA8. `value` is what the
    // host's register memory holds, for the parts that are not kept here.
    pub fn read(&self, offset: u32, size: u32, value: u32) -> u32 {
        let mut v = 0;
        for i in 0..size {
            let byte = (value >> (8 * i)) as u8;
            v |= (self.read8(offset + i, byte) as u32) << (8 * i);
        }
        v
    }

    fn read8(&self, offset: u32, byte: u8) -> u8 {
        match offset {
            WAVE_RAM..=0x9F => {
                let bank = 1 - ((self.reg(WAVE_CNT) >> 6) & 1) as usize;
                self.wave[bank][(offset - WAVE_RAM) as usize]
            }
            SOUNDCNT_X => {
                let on = (0..4).fold(0, |m, i| m | (self.ch[i].on as u8) << i);
                self.regs[(offset - SOUND_BASE) as usize] & MASTER_ON as u8 | on
            }
            SOUND_BASE..=0x8F => self.regs[(offset - SOUND_BASE) as usize],
            // the FIFOs are write only
            _ => byte,
        }
    }

    // A write that can start a DMA channel (see Dma::started_by), with its registers as they are
    // now. Run up to the time of the write first.
    pub fn dma_control(&mut self, channel: usize, src: u32, dst: u32, cnt: u32) {
        self.dma.control_written(channel, src, dst, cnt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dma::FIFO_A_ADDR;
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
        fn write8(&mut self, _: u32, _: u8) {}
        fn write16(&mut self, _: u32, _: u16) {}
        fn write32(&mut self, _: u32, _: u32) {}
    }

    fn collect(apu: &mut Apu, until: u64, timers: &Timers, mem: &mut Flat) -> Vec<(i16, i16)> {
        let mut v = Vec::new();
        apu.run(until, timers, mem, &mut |l, r| v.push((l, r)));
        v
    }

    #[test]
    fn silent_with_the_sound_off_but_samples_keep_coming() {
        let mut apu = Apu::new(0);
        let s = collect(&mut apu, 512 * 100, &Timers::new(), &mut Flat(Vec::new()));
        assert_eq!(s.len(), 100);
        assert!(s.iter().all(|&x| x == (0, 0)));
    }

    // The sound on, FIFO A at 100% on both sides on timer 0, fed by DMA 1 from address 0
    fn direct_sound(timers: &mut Timers, reload: u32) -> Apu {
        let mut apu = Apu::new(0);
        apu.write(SOUNDCNT_X, 2, 0x80);
        apu.write(SOUNDCNT_H, 2, 0x0B04);
        let cnt = (0x8000 | 0x3000 | 0x0200 | 0x0400) << 16;
        apu.dma_control(1, 0, FIFO_A_ADDR, cnt);
        timers.write(0x100, 2, reload, 0);
        timers.write(0x102, 2, 0x80, 0);
        apu
    }

    #[test]
    fn the_fifo_plays_a_byte_per_timer_overflow_and_dma_refills_it() {
        let mut timers = Timers::new();
        // a sample every 1024 cycles, two output samples each
        let mut apu = direct_sound(&mut timers, 0x10000 - 1024);
        let mut mem = Flat((0..=255u8).map(|b| b.wrapping_mul(3)).collect());
        let s = collect(&mut apu, 1024 * 40, &timers, &mut mem);
        // the first overflow finds the FIFO empty, which refills it from the DMA
        assert_eq!(apu.stats.underruns[0], 1);
        assert_eq!(apu.stats.pops[0], 39);
        // the bytes come out in order: sample n plays from overflow n + 1
        let bias = 0x200;
        for n in 1..39usize {
            let expect = (((n - 1) * 3) as u8 as i8 as i32 * 4 + bias).clamp(0, 0x3FF) - bias;
            let at = 2 * (n + 1) - 1;
            assert_eq!(s[at].0 as i32, expect * 32, "sample {n}");
            assert_eq!(s[at].0, s[at].1);
        }
        // four words per refill, every 16 bytes: never more than 32 in the FIFO
        assert_eq!(apu.stats.overflows[0], 0);
        assert_eq!(apu.stats.refills[0], 4);
    }

    #[test]
    fn a_square_wave_has_the_period_its_frequency_asks_for() {
        let mut apu = Apu::new(0);
        apu.write(SOUNDCNT_X, 2, 0x80);
        // channel 2 on both sides at full master volume, PSG at 100%
        apu.write(SOUNDCNT_L, 2, 0x2277);
        apu.write(SOUNDCNT_H, 2, 0x0002);
        // 50% duty, volume 15, no envelope
        apu.write(0x68, 2, 0xF080);
        // 131072 / (2048 - 1792) = 512 Hz: 64 output samples per period
        apu.write(0x6C, 2, 0x8000 | 1792);
        let s = collect(&mut apu, 512 * 640, &Timers::new(), &mut Flat(Vec::new()));
        let rises = s.windows(2).filter(|w| w[0].0 < 0 && w[1].0 > 0).count();
        assert_eq!(rises, 10);
        assert_eq!(s.iter().map(|x| x.0).max(), Some(15 * 8 * 32));
    }

    #[test]
    fn length_turns_a_channel_off() {
        let mut apu = Apu::new(0);
        apu.write(SOUNDCNT_X, 2, 0x80);
        // length 64 - 60 = 4 steps of the 256 Hz clock, enabled
        apu.write(0x68, 2, 0xF080 | 60);
        apu.write(0x6C, 2, 0xC000 | 1792);
        assert_eq!(apu.read(SOUNDCNT_X, 2, 0), 0x82);
        collect(
            &mut apu,
            SEQUENCER_CYCLES * 9,
            &Timers::new(),
            &mut Flat(Vec::new()),
        );
        assert_eq!(apu.read(SOUNDCNT_X, 2, 0), 0x80);
    }

    #[test]
    fn wave_ram_writes_go_to_the_bank_that_is_not_playing() {
        let mut apu = Apu::new(0);
        apu.write(SOUNDCNT_X, 2, 0x80);
        apu.write(WAVE_CNT, 2, 0x40);
        apu.write(0x90, 4, 0x1234_5678);
        assert_eq!(apu.wave[0][..4], [0x78, 0x56, 0x34, 0x12]);
        assert_eq!(apu.read(0x90, 4, 0), 0x1234_5678);
        apu.write(WAVE_CNT, 2, 0x00);
        assert_eq!(apu.read(0x90, 4, 0), 0);
    }

    #[test]
    fn the_registers_say_which_writes_need_a_sync() {
        assert!(syncs_audio(0xA0, 4));
        assert!(syncs_audio(0x102, 2));
        assert!(!syncs_audio(0x10A, 2));
        assert!(syncs_audio(0xC6, 2));
        assert!(!syncs_audio(0xDE, 2));
        assert!(!syncs_audio(0x00, 4));
    }
}
