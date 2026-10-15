// Translates the ARM code of a memory image and prints the result, for checking the translator
// against a reference emulator (see decoding/jit_diff.py).
//
//   jit_dump <image> <image base> <cache base> <entry>...
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
    let mut work: Vec<u32> = args[4..].iter().map(|a| hex(a)).collect();

    let mut mem = |a: u32| -> Option<u32> {
        let off = a.checked_sub(image_base)? as usize;
        let b = image.get(off..off + 4)?;
        Some(u32::from_le_bytes(b.try_into().unwrap()))
    };
    let mut code = vec![0u16; 1 << 19];
    let mut map = vec![Jit::empty_map_entry(); 16384];
    let mut jit = Jit::new(&mut code, cache_base, &mut map);

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
