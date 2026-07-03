use common::{registers::*, video::*};
use minifb::{Key, Window, WindowOptions};
use std::{fmt::format, fs::File, io::Read};

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
    window.set_target_fps(60);

    let dump_base = "ram_dumps";

    // load in values from dump from real ram
    let mut registers_file = File::open(format!("simulation/{dump_base}/registers")).unwrap();
    let mut registers_mem = [0; 32];
    registers_file.read_exact(&mut registers_mem).unwrap();
    let registers_ptr = registers_mem.as_mut_ptr() as *mut DisplayRegisters;
    let mut registers = unsafe { &mut *registers_ptr };

    let mut vram_file = File::open(format!("simulation/{dump_base}/vram")).unwrap();
    let mut vram_mem = [0; 96 * 1024];
    vram_file.read_exact(&mut vram_mem).unwrap();
    let mut vram = VRAM::init(vram_mem.as_mut_ptr());

    let mut palette_file = File::open(format!("simulation/{dump_base}/palette")).unwrap();
    let mut palette_mem = [0; 1024];
    palette_file.read_exact(&mut palette_mem).unwrap();
    let palette_ptr = palette_mem.as_mut_ptr() as *mut Palette;
    let mut palette = unsafe { &mut *palette_ptr };

    let mut oam_file = File::open(format!("simulation/{dump_base}/oam")).unwrap();
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

        println!("frame {frame}");
        frame += 1;
    }
}
