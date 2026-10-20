// Translates the ARM code of a memory image and prints the result, for checking the translator
// against a reference emulator (see decoding/jit_diff.py).
//
//   jit_dump <image> <image base> <cache base> <region table> <entry>...
use common::jit::Jit;
use std::{collections::HashSet, env, fs};

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let image = fs::read(&args[1]).unwrap();
    let image_base = hex(&args[2]);
    let cache_base = hex(&args[3]);
    // native address of the region offset table the translated code reads, 0 for none
    let regions = hex(&args[4]);
    let mut work: Vec<u32> = args[5..].iter().map(|a| hex(a)).collect();

    // The region table jit_diff.py maps for the translated code (its default "alias" mode), which
    // the translator reads through the memory callback. JIT_REGIONS=zero leaves it all zero.
    const ALIAS: u32 = 0x4000_0000;
    let mut table = vec![0u32; 256 + 1024];
    if env::var("JIT_REGIONS").as_deref() != Ok("zero") {
        for top in 4..9 {
            table[top] = ALIAS;
        }
        table[2] = regions + 0x400 | 1;
        table[3] = regions + 0xC00 | 1;
        for i in 0..512u32 {
            table[256 + i as usize] = 0x0200_0000 + i * 512;
            table[256 + 512 + i as usize] = ALIAS + 0x0300_0000 + (i % 64) * 512;
        }
    }
    let mut mem = |a: u32| -> Option<u32> {
        if regions != 0 && a >= regions && a < regions + 4 * table.len() as u32 {
            return Some(table[((a - regions) / 4) as usize]);
        }
        let off = a.checked_sub(image_base)? as usize;
        let b = image.get(off..off + 4)?;
        Some(u32::from_le_bytes(b.try_into().unwrap()))
    };
    let mut code = vec![0u16; 1 << 19];
    let mut map = vec![Jit::empty_map_entry(); 16384];
    let mut jit = Jit::new(&mut code, cache_base, &mut map, regions, 0);

    let mut failed: HashSet<u32> = HashSet::new();
    let mut blocks = Vec::new();
    while let Some(t) = work.pop() {
        if jit.lookup(t).is_some() || failed.contains(&t) {
            continue;
        }
        match jit.translate(t, &mut mem) {
            Ok(th) => blocks.push((t, th, jit.last_arm_end)),
            Err(e) => {
                failed.insert(t);
                println!("ERR {t:#x} {e:?}");
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
    for (arm, th, end) in &blocks {
        println!("BLOCK {arm:#x} {th:#x} {end:#x}");
    }
    let dump = |jit: &Jit| {
        let mut s = String::new();
        for h in jit.code() {
            s += &format!("{:02x}{:02x}", h & 0xFF, h >> 8);
        }
        s
    };
    println!("UNPATCHED {}", dump(&jit));
    for hw in 0..jit.used_halfwords() {
        if jit.is_unresolved_slot(hw) {
            let a = jit.slot_addr(hw);
            let _ = jit.resolve_slot(a, &mut mem);
        }
    }
    println!("PATCHED {}", dump(&jit));
    println!("USED {}", jit.code_used_bytes());
}
