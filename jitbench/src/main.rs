// jitbench: runs the Dino ROM on a host model of the pico-gba runtime and reports where the
// Cortex-M0+ cycles go, to drive the ARM to Thumb JIT work.
//
//   jitbench [--frames N] [--png out.png] [--top K] [--rom file] [--translate-bench]
//            [--press-start F] [--press-every F] [--press-len F]
mod cpu;
mod machine;
mod png;

use common::jit::Jit;
use common::registers::DisplayRegisters;
use common::video::{OAM, Palette, SCREEN_HEIGHT, SCREEN_WIDTH, VRAM, Video};
use machine::*;
use std::time::{Duration, Instant};

// 125 MHz / 60
const FRAME_BUDGET: f64 = 2_083_333.0;

struct Args {
    frames: u64,
    png: Option<String>,
    top: usize,
    rom: String,
    translate_bench: bool,
    trace: bool,
    // write the code cache and the cycles spent on every halfword of it
    dump_cache: Option<String>,
    input: InputScript,
}

fn parse_args() -> Args {
    let mut a = Args {
        frames: 1800,
        png: None,
        top: 12,
        rom: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../firmware/src/Google-Dino-Advance.gba"
        )
        .into(),
        translate_bench: false,
        trace: false,
        dump_cache: None,
        // press A for 6 frames every 90 frames, starting at frame 60
        input: InputScript {
            start: 60,
            every: 90,
            len: 6,
        },
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--frames" => a.frames = value().parse().unwrap(),
            "--png" => a.png = Some(value()),
            "--top" => a.top = value().parse().unwrap(),
            "--rom" => a.rom = value(),
            "--trace" => a.trace = true,
            "--dump-cache" => a.dump_cache = Some(value()),
            "--translate-bench" => a.translate_bench = true,
            "--press-start" => a.input.start = value().parse().unwrap(),
            "--press-every" => a.input.every = value().parse().unwrap(),
            "--press-len" => a.input.len = value().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

fn fnv(bytes: &[u8], mut h: u64) -> u64 {
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn state_hash(m: &Machine) -> u64 {
    let mem = &m.sys.mem;
    let mut h = 0xcbf2_9ce4_8422_2325;
    for r in [
        &mem.iwram,
        &mem.ewram,
        &mem.vram,
        &mem.oam,
        &mem.palette,
        &mem.io,
    ] {
        h = fnv(r.bytes(), h);
    }
    fnv(&m.frames_done().to_le_bytes(), h)
}

fn render(m: &mut Machine) -> Vec<u32> {
    let mem = &mut m.sys.mem;
    let (registers, palette, vram, oam) = unsafe {
        (
            &mut *(mem.io.bytes_mut().as_mut_ptr() as *mut DisplayRegisters),
            &mut *(mem.palette.bytes_mut().as_mut_ptr() as *mut Palette),
            &mut *(mem.vram.bytes_mut().as_mut_ptr() as *mut VRAM),
            &mut *(mem.oam.bytes_mut().as_mut_ptr() as *mut OAM),
        )
    };
    let mut video = Video::new(registers, palette, vram, oam);
    video.begin_frame();
    let mut pixels = Vec::with_capacity(SCREEN_WIDTH * SCREEN_HEIGHT);
    for y in 0..SCREEN_HEIGHT {
        for c in video.render_scanline(y) {
            pixels.push(c.to_minifb_format());
        }
    }
    pixels
}

fn pct(cycles: f64, frames: f64) -> f64 {
    cycles / frames / FRAME_BUDGET * 100.0
}

fn row(name: &str, count: u64, cycles: u64, frames: f64) {
    println!(
        "  {name:<24} {count:>12} {cycles:>14} {:>12.0} {:>7.2}%",
        cycles as f64 / frames,
        pct(cycles as f64, frames)
    );
}

fn report(m: &Machine, top: usize) {
    let s = &m.sys.stats;
    let frames = s.frames.len().max(1) as f64;
    let total: u64 = s.frames.iter().sum();
    println!(
        "frames: {}  capped frames: {}",
        s.frames.len(),
        s.capped_frames
    );
    println!(
        "total cycles: {total}  per frame: {:.0}  ({:.2}% of a {FRAME_BUDGET} cycle frame)",
        total as f64 / frames,
        pct(total as f64, frames)
    );
    let max = s.frames.iter().max().copied().unwrap_or(0);
    let over = s
        .frames
        .iter()
        .filter(|&&f| f as f64 > FRAME_BUDGET)
        .count();
    println!(
        "frame cycles: max {max} ({:.1}%), {over} frames over budget",
        max as f64 / FRAME_BUDGET * 100.0
    );
    println!();
    println!(
        "  {:<24} {:>12} {:>14} {:>12} {:>8}",
        "", "count", "cycles", "cycles/frame", "% frame"
    );
    row("native thumb", 0, s.native_cycles, frames);
    row("jit code", 0, s.jit_cycles, frames);
    row(
        "faults, native code",
        s.fault_count[0],
        s.fault_cycles[0],
        frames,
    );
    row(
        "faults, jit code",
        s.fault_count[1],
        s.fault_cycles[1],
        frames,
    );
    let traps: u64 = s.trap_count.iter().sum();
    row("jit traps", traps, s.trap_cycles, frames);
    row("svc", s.svc_count, s.svc_cycles, frames);
    row("irq trampoline", s.tramp_count, s.tramp_cycles, frames);
    println!(
        "  inline accesses: {}",
        (0..9)
            .filter(|&i| s.inline_count[i] != 0)
            .map(|i| format!("{} {}", REGION_NAMES[i], s.inline_count[i]))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  EWRAM pages paged in: {}", s.ewram_pages);
    println!(
        "  traps: {} exit slots, {} jumps to ARM, {} bx pc veneers",
        s.trap_count[0], s.trap_count[1], s.trap_count[2]
    );

    let block_of = |pc: u32| {
        let hw = ((pc - CACHE_BASE) / 2) as usize;
        let i = s.blocks.partition_point(|b| b.hw_start <= hw);
        i.checked_sub(1)
            .map(|i| s.blocks[i])
            .filter(|b| hw < b.hw_end)
    };
    let mut pcs: Vec<(&u32, &FaultPc)> = s.fault_pcs.iter().collect();
    pcs.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
    for (title, jit) in [("native", false), ("jit", true)] {
        println!("\ntop fault pcs, {title} code:");
        for (pc, f) in pcs
            .iter()
            .filter(|(pc, _)| Sys_in_cache(**pc) == jit)
            .take(top)
        {
            let main = (0..9)
                .max_by_key(|&i| (f.regions[i], std::cmp::Reverse(i)))
                .unwrap();
            let mut line = format!(
                "  {pc:#010x} {:>9} faults ({:>6.1}/frame)  mostly {}",
                f.count,
                f.count as f64 / frames,
                REGION_NAMES[main]
            );
            if jit {
                if let Some(b) = block_of(**pc) {
                    line += &format!(
                        "  block arm {:#x}+{:#x}",
                        b.arm,
                        (**pc - CACHE_BASE) / 2 - b.hw_start as u32
                    );
                    line += "hw";
                }
            }
            println!("{line}");
        }
    }

    let mut hot: Vec<(u64, u64, &BlockInfo)> = s
        .blocks
        .iter()
        .map(|b| {
            let sum = |v: &Vec<u64>| v[b.hw_start..b.hw_end].iter().sum::<u64>();
            (sum(&s.cache_instr), sum(&s.cache_other), b)
        })
        .collect();
    hot.sort_by_key(|(i, o, b)| (std::cmp::Reverse(i + o), b.arm));
    println!("\nhottest translated blocks:");
    println!(
        "  {:<12} {:<12} {:<12} {:>6} {:>13} {:>13} {:>7}",
        "arm", "arm end", "thumb", "bytes", "instr cycles", "fault+trap", "% total"
    );
    for (i, o, b) in hot.iter().take(top) {
        println!(
            "  {:#010x}   {:#010x}   {:#010x}   {:>6} {:>13} {:>13} {:>6.2}%",
            b.arm,
            b.arm_end,
            CACHE_BASE + 2 * b.hw_start as u32,
            2 * (b.hw_end - b.hw_start),
            i,
            o,
            (i + o) as f64 / total as f64 * 100.0
        );
    }

    let jit = &m.sys.jit;
    println!(
        "\njit: {} blocks, {} code bytes, {} ARM instrs translated, {:.3} ms translating ({:.0} ns per ARM instr)",
        jit.block_count(),
        jit.code_used_bytes(),
        s.arm_instrs,
        s.translate_ns as f64 / 1e6,
        s.translate_ns as f64 / s.arm_instrs.max(1) as f64
    );
}

#[allow(non_snake_case)]
fn Sys_in_cache(pc: u32) -> bool {
    pc.wrapping_sub(CACHE_BASE) < 2 * CACHE_HW as u32
}

// Translate every ARM block reachable from the blocks a run hit, over and over, so the translator
// can be profiled on its own
fn translate_bench(m: &Machine) {
    let mem = &m.sys.mem;
    let mut code = vec![0u16; 1 << 19];
    let mut map = vec![Jit::empty_map_entry(); 4096];
    let mut read = |a: u32| mem.read(a, 4);

    // work list discovery like jit_dump: follow the unresolved exit slots
    let mut order: Vec<u32> = Vec::new();
    let mut arm_instrs = 0u64;
    {
        let mut jit = Jit::new(&mut code, CACHE_BASE, &mut map, REGIONS_ADDR);
        let mut work: Vec<u32> = m.sys.stats.blocks.iter().map(|b| b.arm).rev().collect();
        let mut failed = std::collections::HashSet::new();
        while let Some(t) = work.pop() {
            if jit.lookup(t).is_some() || failed.contains(&t) {
                continue;
            }
            match jit.translate(t, &mut read) {
                Ok(_) => {
                    order.push(t);
                    arm_instrs += jit.last_arm_end.wrapping_sub(t) as u64 / 4;
                }
                Err(e) => {
                    failed.insert(t);
                    println!("cannot translate {t:#x}: {e:?}");
                }
            }
            for hw in 0..jit.used_halfwords() {
                if jit.is_unresolved_slot(hw) {
                    let c = jit.code();
                    let target = c[hw + 2] as u32 | (c[hw + 3] as u32) << 16;
                    if jit.lookup(target).is_none() && !failed.contains(&target) {
                        work.push(target);
                    }
                }
            }
        }
    }
    println!(
        "translate bench: {} blocks, {} ARM instrs per round",
        order.len(),
        arm_instrs
    );

    let start = Instant::now();
    let mut rounds = 0u64;
    let mut bytes = 0;
    while start.elapsed() < Duration::from_secs(2) {
        let mut jit = Jit::new(&mut code, CACHE_BASE, &mut map, REGIONS_ADDR);
        for &t in &order {
            let _ = std::hint::black_box(jit.translate(t, &mut read));
        }
        bytes = jit.code_used_bytes();
        rounds += 1;
    }
    let ns = start.elapsed().as_nanos() as f64;
    println!(
        "{rounds} rounds, {:.0} ns per round, {:.1} ns per ARM instr ({bytes} code bytes)",
        ns / rounds as f64,
        ns / (rounds * arm_instrs) as f64
    );
}

fn main() {
    let args = parse_args();
    let rom = std::fs::read(&args.rom).unwrap_or_else(|e| panic!("cannot read {}: {e}", args.rom));
    let mut m = Machine::new(&rom, args.input);
    m.trace = args.trace;
    let t0 = Instant::now();
    m.run(args.frames);
    let host = t0.elapsed();

    report(&m, args.top);
    println!(
        "\nstate hash: {:016x} after {} frames (host time {:.2?})",
        state_hash(&m),
        m.frames_done(),
        host
    );

    if let Some(path) = &args.dump_cache {
        // one line per used halfword: address, value, instruction cycles
        let mut out = String::new();
        for (i, h) in m.sys.jit.code().iter().enumerate() {
            let a = CACHE_BASE + 2 * i as u32;
            out += &format!("{a:x} {h:04x} {}\n", m.sys.stats.cache_instr[i]);
        }
        std::fs::write(path, out).unwrap();
    }
    if let Some(path) = &args.png {
        let pixels = render(&mut m);
        std::fs::write(path, png::encode(SCREEN_WIDTH, SCREEN_HEIGHT, &pixels)).unwrap();
        println!("wrote {path}");
    }
    if args.translate_bench {
        translate_bench(&m);
    }
}
