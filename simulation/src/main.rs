// The desktop simulation: runs a ROM on the host model of the pico-gba runtime (jitbench's
// machine) in a window, with its sound, and the keyboard as the buttons.
//
//   sim <rom.gba> [--no-thumb-jit] [--untimed] [--mute] [--scale 1|2|4|8]
//
// Arrows are the D-pad, Z is A, X is B, A is L, S is R, Enter is Start, Backspace is Select.
// Without a ROM it shows the RAM dump in ram_dumps, as it did before.
use common::{registers::*, video::*};
use jitbench::machine::{InputScript, Machine};
use jitbench::{player, render};
use minifb::{Key, Window, WindowOptions};
use std::{env::home_dir, fs::File, io::Read};

// the buttons by their bit in KEYINPUT
const BUTTONS: [(Key, u16); 10] = [
    (Key::Z, 0),
    (Key::X, 1),
    (Key::Backspace, 2),
    (Key::Enter, 3),
    (Key::Right, 4),
    (Key::Left, 5),
    (Key::Up, 6),
    (Key::Down, 7),
    (Key::S, 8),
    (Key::A, 9),
];

// KEYINPUT for the keys held: a bit is clear while its button is down
fn keyinput(window: &Window) -> u16 {
    BUTTONS
        .iter()
        .filter(|(k, _)| window.is_key_down(*k))
        .fold(0x03FF, |keys, (_, bit)| keys & !(1 << bit))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(rom_path) = args.iter().find(|a| !a.starts_with("--")).cloned() else {
        view_dump();
        return;
    };
    let flag = |f: &str| args.iter().any(|a| a == f);
    let scale = match args
        .iter()
        .position(|a| a == "--scale")
        .map(|i| args[i + 1].as_str())
    {
        Some("1") => minifb::Scale::X1,
        Some("2") => minifb::Scale::X2,
        Some("8") => minifb::Scale::X8,
        _ => minifb::Scale::X4,
    };
    let rom = std::fs::read(&rom_path).unwrap_or_else(|e| panic!("cannot read {rom_path}: {e}"));

    let no_input = InputScript {
        start: 0,
        every: 0,
        len: 0,
    };
    let mut m = Machine::new(&rom, no_input, !flag("--no-thumb-jit"));
    // frames end after a frame's worth of cycles even if the game does not wait for VBlank
    m.timed = !flag("--untimed");
    m.enable_audio();
    // the sound sets the pace, the window's own limit is for when there is none
    let sound = if flag("--mute") {
        None
    } else {
        player::start(&mut m)
    };

    let opts = WindowOptions {
        scale,
        ..WindowOptions::default()
    };
    let title = format!("{rom_path} - ESC to exit");
    let mut window =
        Window::new(&title, SCREEN_WIDTH, SCREEN_HEIGHT, opts).unwrap_or_else(|e| panic!("{e}"));
    window.set_target_fps(60);

    while window.is_open() && !window.is_key_down(Key::Escape) {
        m.sys.keys = Some(keyinput(&window));
        let next = m.frames_done() + 1;
        m.run(next);
        let pixels = render(&mut m);
        window
            .update_with_buffer(&pixels, SCREEN_WIDTH, SCREEN_HEIGHT)
            .unwrap();
    }
    player::finish(&mut m, sound);
}

fn view_dump() {
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
    let dump_base = format!("{home}/Dev/pico-gba/simulation/ram_dumps");

    // load in values from dump from real ram
    let mut registers_file = File::open(format!("{dump_base}/registers")).unwrap();
    let mut registers_mem = [0; 32];
    registers_file.read_exact(&mut registers_mem).unwrap();
    let registers_ptr = registers_mem.as_mut_ptr() as *mut DisplayRegisters;
    let mut registers = unsafe { &mut *registers_ptr };

    let mut vram_file = File::open(format!("{dump_base}/vram")).unwrap();
    // VRAM is read a word at a time so it needs its own aligned storage
    let mut vram_mem = Box::new(VRAM::zeroed());
    vram_file.read_exact(vram_mem.bytes_mut()).unwrap();
    let mut vram = &mut *vram_mem;

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
    let mut video = Video::new(&mut registers, &mut palette, &mut vram, &mut oam);

    let mut frame = 0;
    while window.is_open() && !window.is_key_down(Key::Escape) {
        let mut x = 0;
        let mut y = 0;
        video.begin_frame();
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
