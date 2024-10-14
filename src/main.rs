use minifb::{Key, Window, WindowOptions};
use modular_bitfield::prelude::*;

const WIDTH: usize = 240;
const HEIGHT: usize = 160;

#[bitfield]
#[derive(Debug, Clone, Copy)]
pub struct DisplayControl {
    bg_mode: B3,
    cgb_mode: B1,
    disp_frame: B1,
    hblank_interval_free: B1,
    obj_char_mapping: B1,
    forced_blank: B1,
    screen_disp_bg0: B1,
    screen_disp_bg1: B1,
    screen_disp_bg2: B1,
    screen_disp_bg3: B1,
    screen_disp_obj: B1,
    disp_win0: B1,
    disp_win1: B1,
    disp_obj_win: B1,
}

#[bitfield]
#[derive(Debug, Clone, Copy)]
pub struct DisplayStatus {
    vblank_flag: B1,
    hblank_flag: B1,
    vcounter_flag: B1,
    vblank_irq_en: B1,
    hblank_irq_en: B1,
    vcounter_irq_en: B1,
    unused: B2,
    vcounter_setting: B8,
}

#[bitfield]
#[derive(Debug, Clone, Copy)]
pub struct VertCounter {
    curr_scanline: B8,
    unused: B8,
}

#[bitfield]
#[derive(Debug, Clone, Copy)]
pub struct BgControl {
    bg_prio: B2,
    tileset_base: B2,
    unused1: B2,
    mosaic: B1,
    palette_mode: B1,
    tilemap_base: B5,
    disp_area_overflow: B1,
    tilemap_size: B2,
}

#[bitfield]
#[derive(Debug, Clone, Copy)]
pub struct BgOffset {
    horiz: B9,
    unused1: B7,
    vert: B9,
    unused2: B7,
}

// https://problemkaputt.de/gbatek.htm#lcdiodisplaycontrol
#[repr(C)]
pub struct Registers {
    disp_ctrl: DisplayControl,
    green_swap: u16,
    disp_status: DisplayStatus,
    vert_counter: VertCounter,
    bg_control: [BgControl; 4],
    bg_offset: [BgOffset; 4],
    res: [u8; 8],
}

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
        none: true
    };
    let mut window = Window::new(
        "Test - ESC to exit",
        WIDTH,
        HEIGHT,
        opts,
    )
    .unwrap_or_else(|e| {
        panic!("{}", e);
    });

    // Limit to max ~60 fps update rate
    window.limit_update_rate(Some(std::time::Duration::from_micros(16600)));

    while window.is_open() && !window.is_key_down(Key::Escape) {
        for i in buffer.iter_mut() {
            *i = 0; // write something more funny here!
        }

        // We unwrap here as we want this code to exit if it fails. Real applications may want to handle this in a different way
        window.update_with_buffer(&buffer, WIDTH, HEIGHT).unwrap();
    }
}
