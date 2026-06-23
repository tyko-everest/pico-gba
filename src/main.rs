mod registers;
mod video;

use crate::{registers::DisplayRegisters, video::*};
use arbitrary_int::*;
use minifb::{Key, Window, WindowOptions};
use std::{fs::File, io::Read};

const WIDTH: usize = 240;
const HEIGHT: usize = 160;

fn main() {
    let mut buffer: Vec<u32> = vec![0; WIDTH * HEIGHT];
    let opts = WindowOptions {
        borderless: false,
        title: true,
        scale: minifb::Scale::X2,
        scale_mode: minifb::ScaleMode::Stretch,
        resize: false,
        topmost: true,
        transparency: false,
        none: true,
    };
    let mut window = Window::new("Test - ESC to exit", WIDTH, HEIGHT, opts).unwrap_or_else(|e| {
        panic!("{}", e);
    });

    // Limit to max ~60 fps update rate
    window.set_target_fps(60);

    let mut raw_vram = File::open("src/vram.bin").unwrap();
    let mut vram = [0; 64 * 1024];
    raw_vram.read_exact(&mut vram).unwrap();

    let mut video = Video {
        registers: DisplayRegisters::new(),
        palette: Palette {
            bg: [DisplayColour::from(0); 256],
            obj: [DisplayColour::from(0); 256],
        },
        vram: VRAM::init(vram.as_mut_ptr()),
    };

    video.registers.bg_control[0].set_tileset_base(u2::new(2));
    video.registers.bg_control[0].set_tilemap_base(u5::new(30));

    video.palette.bg[13 * 16 + 0] = DisplayColour::init(0, 20, 29);
    video.palette.bg[13 * 16 + 1] = DisplayColour::init(30, 24, 28);
    video.palette.bg[13 * 16 + 2] = DisplayColour::init(27, 21, 30);
    video.palette.bg[13 * 16 + 3] = DisplayColour::init(15, 05, 20);

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let mut x = 0;
        let mut y = 0;
        for i in buffer.iter_mut() {
            *i = video.get_pixel(x, y).to_minifb_format();
            x += 1;
            if x == 240 {
                y += 1;
                x = 0;
            }
            if y == 160 {
                y = 0;
            }
        }

        // We unwrap here as we want this code to exit if it fails. Real applications may want to handle this in a different way
        window.update_with_buffer(&buffer, WIDTH, HEIGHT).unwrap();
    }
}
