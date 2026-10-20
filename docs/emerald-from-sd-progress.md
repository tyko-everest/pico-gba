# Emerald from the SD card: where things stand (2026-10-19, nothing committed yet)

Goal: run pokeemerald (`~/Dev/pokeemerald/pokeemerald_modern.gba`, 16 MiB, copied to the card as `EMERALD.GBA`) on the pico, ROM paged in from SD, code and data paged through RAM. A `/goal` was set: "continue until the game loop of emerald works without crashing".

## Result so far
- The device boots Emerald from SD, runs the Game Freak intro and the title screen. Soak test of ~9 minutes (build before the last fixes below): no panic or lockup, 60 fps at the title (`0 late`), but slow (17 fps) during busy intro scenes because the code cache thrashes.
- Host harness (jitbench) runs 12000 frames of Emerald with button presses without a crash.
- **Not yet verified on the device:** the last batch of changes (MISS-word race fix, `cpsid` in the dispatcher, EWRAM zero-page reclaim, no EWRAM constant folding, mate-page prefetch, JIT margin 3K hw). They build and pass the host tests. Flash and soak them first. Also untested: `--features autopress` soak (presses A so it goes past the title), and what the display actually shows (`$scratch/snap.sh` dumps VRAM/palette/OAM/IO over gdb and renders it with `jitbench --render-dump DIR out.png`, the user stopped me before the first snapshot).

## What was added (all uncommitted)
common/
- `bios.rs` BIOS calls on a `Memory` trait (LZ77, RLE, Huffman, Sqrt, ArcTan/2, Bg/ObjAffineSet); `dma.rs` immediate DMA; `timers.rs` timer counters from cycles; `flash.rs` Macronix 0xC2/0x09 ID emulation; `romcache.rs` set-associative 256 B page cache (`RomSource` trait, loads the 512 B mate page too); `rom.rs::find_entry` follows the ARM crt0 to find the Thumb entry and the IRQ handler.
- `jit/`: long multiplies (UMULL/SMULL/UMLAL/SMLAL, verified with decoding/jit_diff.py), a Thumb translator (`jit/translate/thumb_src.rs`: instructions copied, pc-relative things made constants, branches go through exit slots, `bl` sets lr to the GBA return address, `bx`/`pop {pc}` go through an inline hash dispatcher emitted at the start of the cache, misses trap at a UDF stub), cache `reset()`, reserve + `use_reserve()` for emergencies, `free_blocks()`/`free_halfwords()`. The dispatcher masks interrupts (`cpsid i`) while it reads the table. EWRAM page addresses are never folded into code.
firmware/
- `sdcard.rs` SPI SD driver (ported from the Waveshare C code, CRC16-checked reads, re-init and retry on error), `romstore.rs` (core 1 owns the card: FAT via embedded-sdmmc, then a one-time FAT walk so ROM reads go straight to blocks; mailbox from core 0), `gba_mem.rs` (all emulated memory accesses: BIOS open bus, EWRAM pages, IO with timers/DMA/IF write-1-clear, ROM through the cache, SRAM/flash), `main.rs` (Thumb JIT in the HardFault handler, code-cache flush with IRQ-pinned blocks retranslated after a flush, ROM cache, SD service between scanlines, stack watermarks, panic prints to RTT, `autopress` feature).
- Cargo: `embedded-sdmmc`, rp2040-hal `disable-intrinsics`.
jitbench/: Emerald support (`--thumb-jit`, `--timed`, `--rom-cache KB`, auto entry, DMA/timers/flash/IF hooks, open bus, cache flush model, LRU cache sims, `--render-dump`). Dino hash `1dd66ffd40e4f925` still holds (`cargo run --release -p jitbench -- --frames 1800`).

## Bugs found and fixed (worth remembering)
1. Core 0's MPU (boot ROM no-access, the BIOS trick) breaks rp2040-hal ROM memcpy/memset: `disable-intrinsics`.
2. Core stacks: big values moved between stacks overflowed core 1's stack and corrupted the RTT block (symptom: output stops). Video and RomStore live in statics; `setup()` is its own function so its frame is freed. Keep `__ebss` at least 6K below the stack top (check with nm after changing sizes).
3. DISPCNT must start as 0x0080 (forced blank), else pokeemerald never gets its first VBlank IRQ.
4. The dispatch-miss word was shared with the IRQ handler: an interrupt between the store and the trap made the stub jump to the wrong target (random crashes, reproduced in jitbench with a 16 KB ROM cache). Fixed by saving/restoring it around IRQs.
5. IRQ handler needs up to ~13 KB of translation: blocks translated in IRQ context are pinned and retranslated right after each flush.
6. SD card goes into an "illegal command" state after a few minutes: CRC check + reinit + retry (a handful of retries per 10 minutes now).

## Open problems / next steps
- Code cache thrash (hundreds of flushes, 100K+ ROM cache misses in 9 minutes in busy scenes): working set (~50 KB of translated code) is bigger than the 32 KB cache; blocks are ~2.4x the guest bytes. Ideas: smaller exit slots, inline IWRAM/EWRAM accesses in Thumb blocks (the idle loop alone is ~40% of faults), skip the m4a mixer, a bigger ROM cache.
- RAM budget (264K): VRAM 96, IWRAM 32, EWRAM pool 34, JIT code 32 + map 8, ROM cache 16, core1 stack 10, core0 stack ~7, Video 5. EWRAM pool filled up at the title after ~6 minutes (68 pages); the new zero-page reclaim should help, unverified.
- PPU lacks blending, windows, affine BGs/sprites (some intro scenes look wrong in jitbench renders).
- Savegame (flash writes are ignored), RTC, HBlank/VBlank DMA, timer IRQs, SoftReset.
- `cargo test -p common` passes; jit_diff random (SEQ=4) shows 5/305 mismatching blocks, which is the known pre-existing gap.

## How to run
- Host: `cargo run --release -p jitbench -- --rom ~/Dev/pokeemerald/pokeemerald_modern.gba --timed --thumb-jit [--press-every 0] [--rom-cache 32] --frames 8000 --png out.png`
- Device: `cd firmware && cargo build --release [--features autopress]`, then from the repo root `probe-rs run --chip rp2040 --no-catch-hardfault --rtt-scan-memory --no-location target/thumbv6m-none-eabi/release/firmware`. Kill stale probe-rs with `pkill -x probe-rs`. The RTT report line shows stacks, flushes, ROM cache hits/misses, EWRAM pool full, SD retries.
- Debug over gdb: `probe-rs gdb --chip rp2040 ELF` then `gdb -batch -ex "set architecture arm" -ex "target remote :1337" ...` (`info threads` shows both cores); `#[no_mangle]` statics `NATIVE_FAULT`, `FAULT_PCS`; map JIT pcs back to guest blocks by dumping `JIT_MAP`.
- Also in `~/.claude/projects/-home-tyko-Dev-pico-gba/memory/emerald-sd-paging-plan.md`.
