# Emerald instruction traces (mGBA GDB stub)

Captured with [tools/gdb_trace.py](../../tools/gdb_trace.py) against mGBA (Flatpak) running Pokemon Emerald, GDB server on port 2345:

    python3 tools/gdb_trace.py --step --max 1000000 --out trace.log

Format: `index pc mode` (mode `A` = ARM, `T` = Thumb). The PC is r15 as reported by the stub; it may include the pipeline offset (+4 Thumb, +8 ARM), not verified.

## Tool notes

- Continue/interrupt sampling is useless on mGBA: the stub only services the interrupt at one place, so every sample was `08000482`. Use `--step`.
- Pipelined single-step: ~22,700 steps/s. Batches of 256 made the stub drop/duplicate replies; batch 8 (default) was reliable.
- The stub accepts one connection per restart. A second connection hangs in the handshake until mGBA's GDB server is restarted.

## Run 1: `emerald-step-1m-run1.log.gz`

1,000,000 instructions, 44 s. The first instruction is `08000482` (the stub's initial stop).

| Region | Instructions | Share |
|---|---|---|
| ROM `08xxxxxx` | 731,819 | 73% |
| IWRAM `03xxxxxx` | 247,495 | 25% |
| BIOS `00xxxxxx` | 20,686 | 2% |

- Unique PCs: 5,408 total, **5,041 in ROM**.
- Hottest 256-byte blocks: `080004xx` 551k (the `08000482`-`08000486` Thumb loop), `03001dxx` 192k (IWRAM code `03001d30`-`03001d80`), `081afexx` 43k, `03001axx` 38k, `081afcxx` 25k, `081b00xx` 20k, `000002xx` 19k (BIOS), `03001cxx` 14k.

## Run 2: `emerald-step-1m-run2.log.gz`

1,000,000 instructions, 44 s, started a different point in time (first PC `08000486`).

| Region | Instructions | Share |
|---|---|---|
| ROM `08xxxxxx` | 696,370 | 70% |
| IWRAM `03xxxxxx` | 290,270 | 29% |
| BIOS `00xxxxxx` | 13,360 | 1% |

- Unique PCs: 2,991 total, **2,654 in ROM**.
- Hottest blocks: `080004xx` 530k, `03001dxx` 245k, `081afexx` 43k, `03001axx` 41k, `081afcxx` 27k, `081b00xx` 20k, `0819f4xx` 15k, `081afdxx` 13k.

## Run 1 vs run 2 (ROM PCs)

Run 1 only: 2,925. Run 2 only: 538. Union: 5,579. The runs hit the same hot spots but cover different cold code, so each 1M-instruction window sees only part of what the game executes.

## Run 3: `emerald-step-1m-run3.log.gz`

1,000,000 instructions, 44 s, captured after the game moved to a different part (first PC `08000486`).

| Region | Instructions | Share |
|---|---|---|
| ROM `08xxxxxx` | 699,670 | 70% |
| IWRAM `03xxxxxx` | 286,970 | 29% |
| BIOS `00xxxxxx` | 13,360 | 1% |

- Unique PCs: 2,369 total, **2,068 in ROM**.
- Hottest blocks: `080004xx` 535k, `03001dxx` 242k, `081afexx` 43k, `03001axx` 41k, `081afcxx` 27k, `081b00xx` 20k, `0819f4xx` 15k, `081afdxx` 14k.
- Run 3 vs run 2 (ROM PCs): 2,062 shared, 6 only in run 3, 592 only in run 2. Run 3 vs run 1: 1,630 shared, 438 only in run 3. Union of all three runs: 5,585 unique ROM PCs.
- The BIOS count (13,360) is identical to run 2, and the hot-block counts are within a few percent, so run 3 is essentially a subset of run 2's behavior.
