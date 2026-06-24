use arbitrary_int::prelude::*;
use bilge::*;

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct DisplayControl {
    bg_mode: u3,
    cgb_mode: bool,
    disp_frame: bool,
    hblank_interval_free: bool,
    obj_char_mapping: bool,
    forced_blank: bool,
    screen_disp_bg0: bool,
    screen_disp_bg1: bool,
    screen_disp_bg2: bool,
    screen_disp_bg3: bool,
    screen_disp_obj: bool,
    disp_win0: bool,
    disp_win1: bool,
    disp_obj_win: bool,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct DisplayStatus {
    vblank_flag: bool,
    hblank_flag: bool,
    vcounter_flag: bool,
    vblank_irq_en: bool,
    hblank_irq_en: bool,
    vcounter_irq_en: bool,
    unused: u2,
    vcounter_setting: u8,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct VertCounter {
    curr_scanline: u8,
    unused: u8,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct BgControl {
    pub bg_prio: u2,
    pub tileset_base: u2,
    pub unused1: u2,
    pub mosaic: bool,
    pub palette_mode: bool,
    pub tilemap_base: u5,
    pub disp_area_overflow: bool,
    pub tilemap_size: u2,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct BgOffset {
    offset: u9,
    unused: u7,
}

// https://problemkaputt.de/gbatek.htm#lcdiodisplaycontrol
#[repr(C, packed)]
pub struct DisplayRegisters {
    pub disp_ctrl: DisplayControl,
    pub green_swap: u16,
    pub disp_status: DisplayStatus,
    pub vert_counter: VertCounter,
    pub bg_control: [BgControl; 4],
    // bg_offset: [BgOffset; 8],
    // and the rest at some point...
}

impl DisplayRegisters {
    pub fn new() -> Self {
        Self {
            disp_ctrl: DisplayControl::from(0),
            green_swap: 0,
            disp_status: DisplayStatus::from(0),
            vert_counter: VertCounter::from(0),
            bg_control: [BgControl::from(0); 4],
        }
    }
}
