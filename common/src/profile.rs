// Optional per-phase timing of render_scanline, enabled with the `profile` feature.
// The host installs a timer that counts *up*; phase times accumulate until reset.

// phase 4 is timed in Video::begin_frame, once per frame
pub const PHASE_NAMES: [&str; PHASE_COUNT] =
    ["line setup", "bg draw", "sprite draw", "output", "frame prep"];
pub const PHASE_COUNT: usize = 5;

#[cfg(feature = "profile")]
mod imp {
    use super::PHASE_COUNT;
    use core::cell::Cell;

    pub struct Profiler {
        pub timer: Cell<fn() -> u32>,
        pub last: Cell<u32>,
        pub totals: [Cell<u32>; PHASE_COUNT],
    }

    // only ever touched from the core running the renderer
    unsafe impl Sync for Profiler {}

    pub static PROFILER: Profiler = Profiler {
        timer: Cell::new(|| 0),
        last: Cell::new(0),
        totals: [const { Cell::new(0) }; PHASE_COUNT],
    };

    pub fn set_timer(timer: fn() -> u32) {
        PROFILER.timer.set(timer);
    }

    #[inline(always)]
    pub fn start() {
        PROFILER.last.set((PROFILER.timer.get())());
    }

    // attribute time since the previous mark to `phase`
    #[inline(always)]
    pub fn mark(phase: usize) {
        let now = (PROFILER.timer.get())();
        let total = &PROFILER.totals[phase];
        total.set(total.get().wrapping_add(now.wrapping_sub(PROFILER.last.get())));
        PROFILER.last.set(now);
    }

    // returns the accumulated totals and clears them
    pub fn take() -> [u32; PHASE_COUNT] {
        core::array::from_fn(|i| PROFILER.totals[i].replace(0))
    }
}

#[cfg(not(feature = "profile"))]
mod imp {
    #[inline(always)]
    pub fn start() {}
    #[inline(always)]
    pub fn mark(_phase: usize) {}
}

pub use imp::*;
