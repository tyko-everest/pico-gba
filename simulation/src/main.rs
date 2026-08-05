use common::{registers::*, video::*};
use minifb::{Key, Window, WindowOptions};
use std::{env::home_dir, fs::File, io::Read};

fn main() {
    let mut buffer: Vec<u32> = vec![0; SCREEN_WIDTH * SCREEN_HEIGHT];
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
    let mut window = Window::new("Test - ESC to exit", SCREEN_WIDTH, SCREEN_HEIGHT, opts)
        .unwrap_or_else(|e| {
            panic!("{}", e);
        });
    window.set_target_fps(60);

    let home_path = home_dir().unwrap();
    let home = home_path.to_str().unwrap();
    let dump_base = format!("{home}/Dev/pico-gba/simulation/pong_dumps");

    // load in values from dump from real ram
    let mut registers_file = File::open(format!("{dump_base}/registers")).unwrap();
    let mut registers_mem = [0; 32];
    registers_file.read_exact(&mut registers_mem).unwrap();
    let registers_ptr = registers_mem.as_mut_ptr() as *mut DisplayRegisters;
    let mut registers = unsafe { &mut *registers_ptr };

    let mut vram_file = File::open(format!("{dump_base}/vram")).unwrap();
    let mut vram_mem = [0; 96 * 1024];
    vram_file.read_exact(&mut vram_mem).unwrap();
    let vram_ptr = vram_mem.as_mut_ptr() as *mut VRAM;
    let mut vram = unsafe { &mut *vram_ptr };

    let mut palette_file = File::open(format!("{dump_base}/palette")).unwrap();
    let mut palette_mem = [0; 1024];
    palette_file.read_exact(&mut palette_mem).unwrap();
    let palette_ptr = palette_mem.as_mut_ptr() as *mut Palette;
    let mut palette = unsafe { &mut *palette_ptr };

    let mut oam_file = File::open(format!("{dump_base}/oam")).unwrap();
    let mut oam_mem = [0; 1024];
    oam_file.read_exact(&mut oam_mem).unwrap();
    let oam_ptr = oam_mem.as_mut_ptr() as *mut OAM;
    let mut oam = unsafe { &mut *oam_ptr };

    // create the control struct with the setup mock ram contents
    let video = Video {
        registers: &mut registers,
        palette: &mut palette,
        vram: &mut vram,
        oam: &mut oam,
    };

    let mut frame = 0;
    while window.is_open() && !window.is_key_down(Key::Escape) {
        let mut x = 0;
        let mut y = 0;
        let mut scanline = video.render_scanline(0);

        for i in buffer.iter_mut() {
            *i = scanline[x].to_minifb_format();
            x += 1;
            if x == SCREEN_WIDTH {
                y += 1;
                x = 0;
                scanline = video.render_scanline(y)
            }
            if y == SCREEN_HEIGHT {
                y = 0;
            }
        }

        // We unwrap here as we want this code to exit if it fails. Real applications may want to handle this in a different way
        window
            .update_with_buffer(&buffer, SCREEN_WIDTH, SCREEN_HEIGHT)
            .unwrap();

        println!("frame {frame}");
        frame += 1;
    }
}
