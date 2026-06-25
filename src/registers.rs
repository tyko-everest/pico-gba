use arbitrary_int::prelude::*;
use bilge::*;

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct DisplayControl {
    pub bg_mode: u3,
    pub cgb_mode: bool,
    pub disp_frame: bool,
    pub hblank_interval_free: bool,
    pub obj_char_mapping: bool,
    pub forced_blank: bool,
    pub screen_disp_bg: [bool; 4],
    pub screen_disp_obj: bool,
    pub disp_win0: bool,
    pub disp_win1: bool,
    pub disp_obj_win: bool,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct DisplayStatus {
    pub vblank_flag: bool,
    pub hblank_flag: bool,
    pub vcounter_flag: bool,
    pub vblank_irq_en: bool,
    pub hblank_irq_en: bool,
    pub vcounter_irq_en: bool,
    pub unused: u2,
    pub vcounter_setting: u8,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct VertCounter {
    pub curr_scanline: u8,
    pub unused: u8,
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

impl BgControl {
    pub fn width_in_tiles(&self) -> usize {
        let is_wide = self.tilemap_size().value() & 0b1 == 0b1;
        if is_wide { 64 } else { 32 }
    }

    pub fn height_in_tiles(&self) -> usize {
        let is_tall = self.tilemap_size().value() & 0b10 == 0b10;
        if is_tall { 64 } else { 32 }
    }
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
pub struct BgOffset {
    pub offset: u9,
    pub unused: u7,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct BgOffsetPair {
    pub x: BgOffset,
    pub y: BgOffset,
}

// https://problemkaputt.de/gbatek.htm#lcdiodisplaycontrol
#[repr(C, packed)]
pub struct DisplayRegisters {
    pub disp_ctrl: DisplayControl,
    pub green_swap: u16,
    pub disp_status: DisplayStatus,
    pub vert_counter: VertCounter,
    pub bg_control: [BgControl; 4],
    pub bg_offset: [BgOffsetPair; 4],
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
            bg_offset: [BgOffsetPair {
                x: BgOffset::from(0),
                y: BgOffset::from(0),
            }; 4],
        }
    }
}
