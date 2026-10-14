// Renders a full frame from the RAM dumps in simulation/ram_dumps and compares a hash of it
// against a known-good value, so renderer optimisations can be checked for unintended changes.
use common::{registers::*, video::*};
use std::{fs, mem::size_of};

// update this only when an output change is intentional
const GOLDEN_HASH: u64 = 0xee316f3a95681005;

#[repr(C, align(4))]
struct Aligned<T>(T);

fn load<T>(name: &str) -> Box<Aligned<T>> {
    let path = format!(
        "{}/../simulation/ram_dumps/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(bytes.len() >= size_of::<T>(), "{path} is too small");
    let mut out: Box<Aligned<T>> = Box::new(unsafe { core::mem::zeroed() });
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            &mut out.0 as *mut T as *mut u8,
            size_of::<T>(),
        )
    };
    out
}

#[test]
fn pong_frame_matches_golden() {
    let mut registers = load::<DisplayRegisters>("registers");
    let mut palette = load::<Palette>("palette");
    let mut vram = load::<VRAM>("vram");
    let mut oam = load::<OAM>("oam");
    let mut video = Video::new(&mut registers.0, &mut palette.0, &mut vram.0, &mut oam.0);
    video.begin_frame();

    // FNV-1a over every output pixel in minifb format
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut frame = Vec::with_capacity(SCREEN_WIDTH * SCREEN_HEIGHT * 4);
    for y in 0..SCREEN_HEIGHT {
        for colour in video.render_scanline(y) {
            for byte in colour.to_minifb_format().to_le_bytes() {
                hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
                frame.push(byte);
            }
        }
    }
    // set GOLDEN_DUMP=<path> to save the frame as raw 0x00RRGGBB little-endian pixels
    if let Ok(path) = std::env::var("GOLDEN_DUMP") {
        fs::write(path, &frame).unwrap();
    }
    assert_eq!(hash, GOLDEN_HASH, "frame hash changed: {hash:#018x}");
}
