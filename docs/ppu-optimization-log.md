# PPU renderer optimisation log

**Date:** 2026-10-13
**Commits:** `56a6cdd` (renderer rewrite), `170f4fc` (SRAM placement and per-line sprite masks)
**Goal:** `render_scanline` for a full frame fits within one 60 Hz frame (16.7 ms) on the RP2040, Cortex-M0+ at 125 MHz. Pushing pixels to the display was out of scope.

## Result

| Step | Render time per frame (Pico) | vs. baseline |
|---|---|---|
| Baseline | 95.1 ms (97.1 ms with profiling on) | 1× |
| 1. OAM decoded once per frame | 80.3 ms | 1.2× |
| 2. Painter's-algorithm tile renderer | 14.8 ms | 6.6× |
| 3. Hot code moved to SRAM | 13.9 ms | 7.0× |
| 4. Per-scanline sprite bitmasks | **10.0 ms** | **9.7×** |

The budget was 16.7 ms, and about 6.7 ms of it is now spare. Every number in this log comes from the Pico running pong live, read over RTT. The whole frame, including the SPI transfer to the display, went from 163 ms to 79 ms. About 69 ms of that is now the display path, which wasn't part of this work.

---

## 1. Measurement setup

You can't optimise what you can't measure reliably, so this came first.

### 1.1 Desktop profile (samply)
- `profile.json.gz` wasn't symbolicated. I matched it to `target/release/sim` (same build ID, built 3 s apart) and resolved every sampled address with `addr2line -i`, including inlined frames. I then aggregated self time and inclusive time per function and per source line.
- **What it showed:**
  - About 30% of samples were minifb (Wayland writes, image scaling, the 60 fps sleep), which is noise for the Pico.
  - `render_scanline` was 56% inclusive.
  - The per-pixel composite loop (video.rs:620-652) was about 20%.
  - The bilge constructors (`masked_new`, `Palette16Colour::new`) were about 8%.
- **Decision:** profile only the `render_scanline` subtree on the desktop (your call), and treat Pico numbers as the source of truth, since M0+ costs differ from x86.

### 1.2 Pico timing over RTT
- **Flash and watch:**
  `probe-rs run --chip rp2040 --no-catch-hardfault --rtt-scan-memory --no-location target/thumbv6m-none-eabi/release/firmware`
  The firmware hard-faults on purpose to translate GBA addresses, and `probe-rs run` halts on HardFault unless you pass `--no-catch-hardfault`. That was the first run's failure.
- **Clock:** SysTick ticks at 1 MHz on this board (you confirmed this empirically). I cross-checked it: 166 frames over about 26 s of RTT wall-clock timestamps is about 157 ms per frame, which matches the printed ~163 ms.
- **Render separated from SPI.** The old "rendering took X us" timed the whole loop, including the blocking `display.set_pixels` call. I added a separate total that sums only the `render_scanline` calls, plus `begin_frame` once it existed. Without that, the gains in rendering would have been hidden by SPI time.
- **Fixed a wraparound bug in the frame timer:** `render_start - render_end` overflowed when SysTick wrapped, and one frame printed `4278340535 us`. It's now `wrapping_sub(...) & 0x00FF_FFFF`.

### 1.3 Per-phase profiling (`common/src/profile.rs`)
- **Feature-gated:** a `profile` cargo feature on `common`, enabled only by the firmware. With the feature off, `start()` and `mark()` are empty `#[inline(always)]` functions, so the sim and normal builds pay nothing.
- **How it works:**
  - The firmware installs a timer as `|| (!SYST::get_current()) << 8`. Inverting turns the down-counter into an up-counter, and shifting the 24-bit value up by 8 means plain `wrapping_sub` handles wraparound.
  - `mark(n)` adds the time since the previous mark to phase `n`.
  - The firmware prints each phase's total per frame, then resets them.
- **Overhead:** about 2 ms per frame (95.1 → 97.1 ms at baseline), and the timer has 1 µs resolution. Phase sums land within a few percent of the total. The numbers were stable to within a few µs from frame to frame.

### 1.4 Correctness: golden frame test (`common/tests/golden.rs`)
- **What it does:** loads `simulation/ram_dumps/*` into aligned storage, renders all 160 lines and checks an FNV-1a hash of the output. `GOLDEN_DUMP=<path>` also writes the raw frame, which I converted to PNG to look at.
- **Run it with:** `cargo test -p common --test golden`. Use debug mode to also get overflow checks and `debug_assert`s.
- **How it was used:**
  - Step 1 was required to keep the hash unchanged, so it couldn't change any output.
  - Step 2 changed the output on purpose. I compared before and after images, then you confirmed the Pico display looked right, and only then was the new hash recorded (`0xee316f3a95681005`).
  - Steps 3 and 4 were required to keep the new hash.
- **Note:** the RAM dump isn't pong. It's an overworld scene (4 text BGs, BG1–3 scrolled to (16, 280), 50 on-screen sprites), which made it a good test of scrolling and sprites.

---

## 2. Baseline: where the 95 ms went

| Phase | µs per frame | Share | Cost at 125 MHz |
|---|---|---|---|
| BG render (4× `render_bg_scanline`) | 57,030 | 58% | ~46 cycles per pixel per BG |
| Composite (per-pixel priority walk) | 19,640 | 20% | ~64 cycles per pixel |
| OAM scan (all 128 entries, every line) | 16,370 | 16% | ~100 cycles per entry |
| Sprite pixels | 3,425 | 3% | |
| Priority sort | 535 | <1% | |

**Budget:** 125 MHz / 60 fps is about 2.08M cycles per frame, or about **54 cycles per pixel** for everything. The baseline used about 310 cycles per pixel for rendering alone.

**Root causes, from reading the code:**
- **Three passes per line:**
  1. Decode every BG (including disabled ones) into an intermediate `PaletteColour` buffer.
  2. Build a sprite buffer.
  3. Composite: for each pixel, walk the whole sorted priority list, and `match` on the palette mode for each layer.
- **Per-pixel bilge construction:** `Palette16Colour::new(u4, u4)` plus `masked_new` range checks for every pixel.
- **Double fetches:** both the 4bpp and 8bpp tile rows were fetched for every tile, whatever the mode.
- **Repeated OAM work:** all 128 OAM entries were re-read and re-decoded (`width()`/`height()` lookup tables, packed reads) on every one of the 160 lines.
- **`repr(C, packed)` costs on M0+.** M0+ has no unaligned access, so every u16 read from `Palette`, `OAM` or `DisplayRegisters` becomes two byte loads plus a shift and OR. Verified by compiling a test for `thumbv6m-none-eabi`:
  ```
  packed:  ldrb r2,[r0,r1]; adds r0,r0,r1; ldrb r0,[r0,#1]; lsls r0,r0,#8; adds r0,r0,r2
  aligned: ldrh r0,[r0,r1]
  ```
  Your constraint was that these GBA-mapped types keep their exact definitions, so I didn't remove `packed`. Instead the design reads them only once per frame or once per line, never per pixel.

---

## 3. The optimisations

### Step 1: decode OAM once per frame (97.1 → 80.3 ms)

**Thinking.** The OAM scan was 16% of the time and did identical work on all 160 lines. It was also the lowest-risk change: it alters no rendering logic, so it could be verified by an unchanged hash.

**Change.**
- **New `Video::begin_frame()`:** walks OAM once, decodes each entry into a plain, aligned `CachedSprite` (x, y, size, tile, palette, priority), and keeps only sprites that are enabled and start above line 160. The list stays in OAM order, so the existing sort and priority behaviour don't change.
- **`Video` owns the cache now,** so it's built with `Video::new(...)`. The sim, firmware and test were updated.

**Verification.**
- The golden hash was unchanged.
- On the Pico, OAM scan went from 16,370 to 5,790 µs. `begin_frame` itself costs 216 µs.

**Side observation.** BG render also dropped 57.0 → 51.1 ms even though I didn't touch it. The best explanation is less code competing for the 16 KB XIP flash cache. That was the first hint that flash fetches mattered, which led to step 3.

### Step 2: painter's-algorithm tile renderer (80.3 → 14.8 ms)

**Thinking.** BG render plus composite was 78% of the time, and both came from the three-pass design. Fast GBA renderers draw tile by tile, back to front, straight into the final line. Each pixel is then one table lookup, behind a transparency check, per layer that covers it. Priority is resolved by draw order, so there is no per-pixel priority logic at all.

**Changes.**
- **Palette cache.** `begin_frame` copies BG and OBJ palette RAM into a `[u16; 512]`, masking bit 15, which is unused on the GBA. After that, drawing never touches the packed `Palette` or bilge. The output format is the GBA's own BGR555, so no conversion is needed.
- **Draw order, per gbatek priority rules:**
  - Fill the line with the backdrop (BG palette 0, colour 0).
  - For each priority from 3 down to 0:
    - draw the enabled BGs at that priority, from BG3 down to BG0, so BG0 wins ties;
    - then draw that priority's sprites from the highest OAM number to the lowest, so lower OAM numbers win and sprites beat BGs at equal priority.
- **Tile inner loop** (`draw_tile_row_4bpp` / `_8bpp`):
  - One aligned u32 load per tile row (two for 8bpp).
  - Skip immediately if the row is all transparent (`pixels == 0`).
  - Otherwise 8× `if idx != 0 { dst[i] = palette[idx] }`, with flipping handled by the index.
  - The palette and destination are passed as fixed-size `&[u16; 16]` / `&mut [u16; 8]` references, so the per-pixel indexing needs no bounds checks. Only one slice-to-array conversion per tile remains.
- **Line buffer with margins.** The line is drawn into a `[u16; 256]` with 8 pixels of margin on each side. A BG always draws 31 whole tiles starting at `8 - fine_x`, and sprite tiles partly off either edge land in the margin, so there's no partial-tile code at all. Only sprite tiles entirely off screen are skipped.
- **Branch hoisting.** The 4bpp vs 8bpp decision is made once per tile, not per pixel. Disabled BGs are never drawn.
- **Aligned VRAM.** `VRAM` became `#[repr(C, align(4))]`. That's still exactly 96 KB with the same layout; only its alignment changed. Word reads now compile to `ldr`, and the old u32 reads from an align-1 type, which were UB, are fixed. As a result the sim had to load VRAM into aligned storage (`Box<VRAM>` plus `bytes_mut()`), because it used to cast a stack `[u8]` array.
- **New API.** `render_scanline_into(&self, y, &mut [u16; 240])` produces raw BGR555. `render_scanline` keeps its old signature and wraps it.

**gbatek correctness fixes made in the same pass.** The hash was going to change anyway, so these were cheapest to do now.
- **BG scrolling wraps in both directions.** Previously `bg_x` was clamped and `bg_y` never wrapped. In the dump, BG1–3 have VOFS = 280, which is why the old renderer showed almost nothing.
- **Maps larger than 256 px use 2 KB screenblocks** (SC0 SC1 on the top row, SC2 SC3 below), not linear 64-wide rows.
- **BG tile flips and 8bpp BGs work.**
- **Sprite priority interleaves with BGs.** Previously all sprites collapsed into one layer.
- **Sprite transparency is fixed.** Transparent pixels of lower-priority sprites no longer overwrite higher-priority sprites. Previously the last sprite written won, and it also wrote opaque black.
- **Sprite position wrapping.** X is 9 bits, and values ≥ 240 now mean partly off the left edge. Y wraps at 256 (`(line - y) & 0xFF < height`).
- **Sprites:** flips work, 256-colour sprites are drawn, 1D vs 2D tile mapping follows DISPCNT bit 6 (2D uses a 32-tile row stride), and tile numbers wrap within the 32 KB OBJ area.
- **Skipped sprites:** OBJ-window sprites (mode 2), prohibited mode 3 and shape 3. Shape 3 would also have indexed past the end of the size table.
- **DISPCNT:** forced blank shows white, the OBJ enable bit is respected, and only BGs that are text BGs in the current mode are drawn.
- **Reads stay inside VRAM:** BG tile reads past the 64 KB BG area are treated as transparent, and OBJ reads are masked into 0x10000–0x17FFF. Nothing can read past the end of the 96 KB array.

**Verification.**
1. The debug-mode golden run (overflow checks and `debug_assert` alignment/bounds checks on) rendered with no panics.
2. Before and after PNGs of the dump. Before: nearly black, with a broken player sprite. After: a complete room with floor, walls, boxes, a whole player sprite and an NPC.
3. You confirmed pong looks correct on the Pico display.
4. Only after that was the new golden hash recorded.

**Pico result.**

| Phase | µs per frame |
|---|---|
| line setup | 6,837 |
| bg draw | 3,177 |
| sprite draw | 2,376 |
| output copy | 758 |
| frame prep | 324 |
| **total** | **14,796** |

BG drawing went from 51 ms (plus 20 ms of composite) to 3.2 ms in total. Some of that is because pong's BG rows are often fully transparent, which the row skip handles almost for free.

### Step 3: run the renderer from SRAM (14.8 → 13.9 ms)

**Thinking.** Code runs from QSPI flash through a 16 KB XIP cache. The game ROM (`include_bytes!("pong.gba")`) is also in flash, running on core 0 and sharing that cache. A cache miss stalls for a whole QSPI fetch. The step 1 side effect suggested this was a real cost.

**Change.**
- `begin_frame` and `render_scanline_into` are marked `#[inline(never)]` and placed in `.data.ram_func`. cortex-m-rt copies `.data.*` from flash to RAM at boot, so they execute from SRAM.
- The attribute only applies when `target_arch = "arm", target_os = "none"`. On the x86 sim, code in a `.data` section would sit in non-executable memory and crash.
- `#[inline(never)]` matters: if a function with `link_section` is inlined into a caller, it ends up wherever the caller is.

**Verification.**
- `nm` shows both functions at 0x2000xxxx. The linker added `__Thumbv6MABSLongThunk_*` veneers for the 256 MB jump from flash to RAM.
- No ARM disassembler was installed, so I wrote a small Python script that decodes Thumb `BL` instructions inside the RAM code. The only calls back into flash are `__aeabi_memclr4` and `__aeabi_memcpy`, both small and called on every line so they stay cached, plus cold panic paths.
- The golden hash was unchanged.

**Result.** 13.9 ms. That's a modest gain, and line setup barely moved (6,837 → 6,331 µs), so flash wasn't what made line setup slow. That needed a closer look.

### Step 4: per-scanline sprite bitmasks (13.9 → 10.0 ms)

**Investigation.** I temporarily split line setup into finer marks and added two counters; this instrumentation was reverted afterwards.

| Sub-phase | µs per frame |
|---|---|
| sprite select | 5,445 |
| backdrop fill | 789 |
| register reads | 87 |

- **Sprites checked per line: 128.** Every OAM entry survived the per-frame filter.
- **Sprite-lines actually drawn per frame: 1,048,** about 6.5 per line.

**Why.** Pong leaves its unused OAM entries zeroed. Per gbatek, an all-zero entry is a visible 8×8 sprite at (0,0), so all 128 sprites are "on screen". They're only on lines 0–7, but every line still tested all 128 at about 33 cycles each.

**Change.** `begin_frame` now also builds `line_masks: [[u32; 4]; 160]` (2.5 KB): for each scanline, bit n is set if cached sprite n covers that line, with Y wrapping at 256. Each line walks its mask from the top bit down using `leading_zeros`. That yields sprite indices in reverse OAM order, which is exactly the painter's drawing order, so the old reversal went away. Building the masks costs about 1,000 bit-sets per frame (frame prep went 322 → 483 µs) and saves about 4,000 µs per frame.

**Verification.** The golden hash was unchanged in debug and release.

**Pico result.**

| Phase | µs per frame |
|---|---|
| line setup | 2,192 |
| bg draw | 2,885 |
| sprite draw | 2,343 |
| output copy | 757 |
| frame prep | 483 |
| **total** | **9,996** |

Average 62 µs per line, maximum 410 µs per line.

---

## 4. Considered and not done

| Idea | Why not (yet) |
|---|---|
| Remove `repr(C, packed)` from the GBA types | Your constraint: they mirror GBA memory. It would have been layout-identical (asserted for `Palette` and `[ObjAttr; 128]`), but caching made it irrelevant because there are no per-pixel reads left. |
| Standalone desktop benchmark binary | Your call: focus samply on the `render_scanline` subtree instead. |
| SPI / display output | Out of scope for this round. It's now the dominant cost (about 69 ms per frame). |
| Exact OBJ-vs-OBJ hardware quirk | Hardware resolves overlapping sprites by OAM number before priority. Painter's order by (priority, OAM number) differs only when a lower-numbered, lower-priority sprite overlaps a higher-numbered, higher-priority one. Left as a `todo!`. |
| RP2040 interpolators, overclocking | Not needed yet; kept in reserve. |

## 5. Known limitations (marked `todo!` in code)
- Affine BGs (modes 1 and 2) and bitmap modes (3–5) are not drawn.
- Rotation/scaling sprites are drawn unrotated, with no double size.
- No mosaic, alpha blending, brightness effects or windows.
- Palette and OAM are snapshotted once per frame, so changes a game makes mid-frame (raster effects) are not seen. BG registers are still read per line.

## 6. Caveats on the numbers
- **The Pico runs live pong, but the desktop test uses a different scene.** Pico numbers depend on what pong is showing. Pong has sparse BGs, so a BG-heavy game will spend more time in "bg draw".
- **Profiling costs about 2 ms per frame,** and each phase is quantised to 1 µs per line.
- **Desktop and Pico costs differ a lot.** On the Pico, packed reads, flash fetches and missing instructions (no `clz`, no unaligned loads) cost far more than on x86.

## 7. What's next
- **Display output (about 69 ms per frame):**
  - Check the actual SPI clock: 60 MHz isn't reachable from a 125 MHz peripheral clock.
  - Set the display window once per frame, not once per line.
  - Convert the palette straight to the display's byte order.
  - DMA one line buffer while the CPU renders the next.
- **Renderer, if more headroom is needed:**
  - Render directly into the caller's buffer, removing the 0.76 ms output copy and some of the 0.79 ms backdrop fill.
  - RP2040 interpolators for the tile and palette lookups.
