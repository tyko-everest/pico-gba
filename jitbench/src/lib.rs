// The pico-gba runtime on the host, for jitbench and the desktop simulation
pub mod cpu;
pub mod machine;
pub mod player;
pub mod png;
pub mod wav;

use common::registers::DisplayRegisters;
use common::video::{OAM, Palette, SCREEN_HEIGHT, SCREEN_WIDTH, VRAM, Video};
use machine::Machine;

// The screen as the game has it now, in minifb's 0RGB format
pub fn render(m: &mut Machine) -> Vec<u32> {
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
