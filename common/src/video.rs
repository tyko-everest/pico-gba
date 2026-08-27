use crate::registers::*;
use arbitrary_int::prelude::*;
use bilge::*;
use core::{ops::Deref, todo, usize};

// tiles are 8x8 pixels
const TILE_SIZE_LOG: usize = 3;
const TILE_SIZE: usize = 1 << TILE_SIZE_LOG;
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: usize = 160;

// Final colour generated for the display
#[bitsize(16)]
#[derive(FromBits, Copy, Clone)]
pub struct DisplayColour {
    red: u5,
    green: u5,
    blue: u5,
    unused: u1,
}

impl DisplayColour {
    pub fn init(r: u8, g: u8, b: u8) -> Self {
        Self::new(u5::new(r), u5::new(g), u5::new(b), u1::new(0))
    }

    pub fn to_minifb_format(&self) -> u32 {
        self.red().as_u32() << (16 + 3)
            | self.green().as_u32() << (8 + 3)
            | self.blue().as_u32() << 3
    }

    pub fn to_rgb565_format(&self) -> u16 {
        self.red().as_u16() << 11 | self.green().as_u16() << 6 | self.blue().as_u16()
    }
}

// One line of pixels in 4-bit colour mode
#[derive(Copy, Clone)]
struct Tile4Line {
    data: u32,
}

// One tile in 4-bit colour mode
#[derive(Copy, Clone)]
struct Tile4 {
    data: [Tile4Line; 8],
}

impl Tile4 {
    pub fn get_line(&self, y: usize) -> Tile4Line {
        self.data[y]
    }
}

// One tile in 8-bit colour mode
#[derive(Copy, Clone)]
struct Tile8 {
    data: [[u8; 8]; 8],
}

impl Tile8 {
    pub fn get_colour(&self, x: usize, y: usize) -> u8 {
        self.data[y][x]
    }
}

#[bitsize(16)]
#[derive(Copy, Clone)]
struct MapTextEntry {
    tile: u10,
    horiz_flip: bool,
    vert_flip: bool,
    palette: u4,
}

impl MapTextEntry {
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

struct MapTextLine {
    data: [MapTextEntry; 31]
}

impl MapTextLine {
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }

    // pub fn get_
}

struct MapRotScaleEntry(u8);

pub struct VRAM {
    _data: [u8; 96 * 1024],
}

impl VRAM {
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }

    pub fn data(&self) -> *mut u8 {
        self._data.as_ptr().cast_mut()
    }
}

#[repr(C, packed)]
pub struct Palette {
    pub bg: [DisplayColour; 256],
    pub obj: [DisplayColour; 256],
}

impl Palette {
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }
    fn get_bg_colour_256(&self, colour: usize) -> DisplayColour {
        self.bg[colour]
    }

    fn get_bg_colour_16(&self, palette: usize, colour: usize) -> DisplayColour {
        self.bg[palette * 16 + colour]
    }

    fn get_obj_colour_256(&self, colour: usize) -> DisplayColour {
        self.obj[colour]
    }

    fn get_obj_colour_16(&self, palette: usize, colour: usize) -> DisplayColour {
        self.obj[palette * 16 + colour]
    }
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
struct ObjAttr0Normal {
    y: u8,
    rot_scale: bool,
    disable: bool,
    mode: u2,
    mosaic: bool,
    enable_256_colour: bool,
    shape: u2,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
struct ObjAttr0RotScale {
    y: u8,
    rot_scale: bool,
    double_size: bool,
    mode: u2,
    mosaic: bool,
    enable_256_colour: bool,
    shape: u2,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
struct ObjAttr1Normal {
    x: u9,
    unused: u3,
    horiz_flip: bool,
    vert_flip: bool,
    size: u2,
}

#[bitsize(16)]
#[derive(FromBits, Clone, Copy)]
struct ObjAttr1RotScale {
    x: u9,
    param_sel: u5,
    size: u2,
}

#[bitsize(16)]
#[derive(Clone, Copy)]
struct ObjAttr2 {
    tile: u10,
    prio: u2,
    palette: u4,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct ObjAttrNormal {
    attr0: ObjAttr0Normal,
    attr1: ObjAttr1Normal,
    attr2: ObjAttr2,
    unused: u16,
}

impl ObjAttrNormal {
    fn is_disabled(&self) -> bool {
        let attr0 = self.attr0;
        attr0.disable()
    }

    fn width(&self) -> usize {
        let map = [[8, 16, 8], [16, 32, 8], [32, 32, 16], [64, 64, 32]];
        let attr0 = self.attr0;
        let attr1 = self.attr1;
        map[attr1.size().as_usize()][attr0.shape().as_usize()]
    }

    fn height(&self) -> usize {
        let map = [[8, 8, 16], [16, 8, 32], [32, 16, 32], [64, 32, 64]];
        let attr0 = self.attr0;
        let attr1 = self.attr1;
        map[attr1.size().as_usize()][attr0.shape().as_usize()]
    }

    fn is_on_scanline(&self, scanline: usize) -> bool {
        if self.is_disabled() {
            return false;
        }
        let attr0 = self.attr0;
        let y = attr0.y().as_usize();
        if scanline >= y && scanline < y + self.height() {
            true
        } else {
            false
        }
    }

    fn is_on_x(&self, screen_x: usize) -> bool {
        let attr1 = self.attr1;
        let x = attr1.x().as_usize();
        if screen_x >= x && screen_x < x + self.width() {
            true
        } else {
            false
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct ObjAttrRotScale {
    attr0: ObjAttr0RotScale,
    attr1: ObjAttr1RotScale,
    attr2: ObjAttr2,
    rot_scale: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub union ObjAttr {
    normal: ObjAttrNormal,
    rot_scale: ObjAttrRotScale,
}

impl ObjAttr {
    fn is_rot_scale(&self) -> bool {
        unsafe {
            let attr0 = self.normal.attr0;
            attr0.rot_scale()
        }
    }

    fn get_prio(&self) -> u2 {
        let attr2 = unsafe { self.normal.attr2 };
        attr2.prio()
    }

    fn get_normal(&self) -> Option<&ObjAttrNormal> {
        if self.is_rot_scale() {
            None
        } else {
            unsafe { Some(&self.normal) }
        }
    }

    fn get_rot_scale(&self) -> Option<&ObjAttrRotScale> {
        if self.is_rot_scale() {
            unsafe { Some(&self.rot_scale) }
        } else {
            None
        }
    }
}

#[repr(C, packed)]
pub struct OAM([ObjAttr; 128]);

impl OAM {
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }

    fn get(&self, index: usize) -> &ObjAttr {
        &self.0[index]
    }
}

impl Deref for OAM {
    type Target = [ObjAttr; 128];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Priority {
    prio: u8,
    is_bg: bool,
    num: u8,
}

impl Priority {
    fn new(prio: u8, is_bg: bool, num: u8) -> Self {
        Self { prio, is_bg, num }
    }
}

pub struct Video<'a> {
    pub registers: &'a mut DisplayRegisters,
    pub palette: &'a mut Palette,
    pub vram: &'a mut VRAM,
    pub oam: &'a mut OAM,
}

impl Video<'_> {
    // Get the base address of a tile set given the current status of the control registers
    fn get_tileset_base_addr(&self, bg: usize) -> *const u8 {
        const TILESET_OFFSET: usize = 16 * 1024;
        let register = self.registers.bg_control[bg];
        unsafe {
            self.vram
                .data()
                .add(register.tileset_base().as_usize() * TILESET_OFFSET)
        }
    }

    // Get a specific 4-bit colour depth tile
    fn get_tile4(&self, bg: usize, index: usize) -> Tile4 {
        let base_ptr = self.get_tileset_base_addr(bg) as *const Tile4;
        let ptr = unsafe { base_ptr.add(index) };
        unsafe { *ptr }
    }

    // Get a specific 8-bit colour depth tile
    fn get_tile8(&self, bg: usize, index: usize) -> Tile8 {
        let base_ptr = self.get_tileset_base_addr(bg) as *const Tile8;
        let ptr = unsafe { base_ptr.add(index) };
        unsafe { *ptr }
    }

    // Get the base address of a tile map given the current status of the control registers
    fn get_map_base_addr(&self, bg: usize) -> *const u8 {
        const MAP_OFFSET: usize = 2 * 1024;
        let bg_control = self.registers.bg_control[bg];
        unsafe {
            self.vram
                .data()
                .add(bg_control.tilemap_base().as_usize() * MAP_OFFSET)
        }
    }

    // Get info about a tile map entry assuming this BG is in text mode
    fn get_map_text_entry(&self, bg: usize, tile_x: usize, tile_y: usize) -> MapTextEntry {
        let bg_control = self.registers.bg_control[bg];
        let base_ptr = self.get_map_base_addr(bg) as *const MapTextEntry;
        let ptr = unsafe { base_ptr.add(tile_y * bg_control.width_in_tiles() + tile_x) };
        unsafe { *ptr }
    }

    // For a background in a text-based mode, fetch the text entries for the given scanline
    fn get_bg_line_text_entries(
        &self,
        bg: usize,
        scanline: usize,
        map_entries: &mut [MapTextEntry; 31],
    ) {
        let bg_control = self.registers.bg_control[bg];
        let bg_offset = self.registers.bg_offset[bg];

        let offset_x = {
            let reg = bg_offset.x;
            reg.offset().as_usize()
        };
        let offset_y = {
            let reg = bg_offset.y;
            reg.offset().as_usize()
        };
        if bg_control.palette_mode() {
            todo!()
        } else {
            for (index, entry) in map_entries.into_iter().enumerate() {
                let tile_x = (offset_x + index) % bg_control.width_in_tiles();
                let tile_y = (offset_y + scanline) % bg_control.height_in_tiles();
                *entry = self.get_map_text_entry(bg, tile_x, tile_y)
            }
        }
    }

    // Fetch the tiles needed to render this sprite on this scanline
    // If the sprite is not present on this scanline it returns 0
    fn get_sprite_line_tile4(
        &self,
        sprite: usize,
        scanline: usize,
        tiles: &mut [Tile4; 8],
    ) -> usize {
        let oam = self.oam.get(sprite).get_normal().unwrap();
        let attr0 = oam.attr0;
        let attr1 = oam.attr1;
        let attr2 = oam.attr2;

        let sprite_base_x = attr1.x().as_usize();
        let sprite_base_y = attr0.y().as_usize();

        // set up the base size to start
        let mut sprite_width: usize = 8 << attr1.size().as_usize();
        let mut sprite_height: usize = 8 << attr1.size().as_usize();

        if attr0.shape().as_usize() == 1 {
            // this is a horizontal sprite
            if attr1.size().as_usize() < 2 {
                sprite_width <<= 1;
                sprite_height = 8;
            } else {
                sprite_height >>= 1;
            }
        } else if attr0.shape().as_usize() == 2 {
            // this is a vertical sprite
            if attr1.size().as_usize() < 2 {
                sprite_height <<= 1;
                sprite_width = 8;
            } else {
                sprite_width >>= 1;
            }
        }

        if scanline < sprite_base_y || scanline >= sprite_base_y + sprite_height {
            // no part of this sprite could overlap with this scanline
            return 0;
        }

        1
    }

    fn get_sprite_pixel_normal(
        &self,
        sprite: usize,
        screen_x: usize,
        screen_y: usize,
    ) -> Option<DisplayColour> {
        let oam = self.oam.get(sprite).get_normal().unwrap();
        let attr0 = oam.attr0;
        let attr1 = oam.attr1;
        let attr2 = oam.attr2;

        let sprite_base_x = attr1.x().as_usize();
        let sprite_base_y = attr0.y() as usize;

        // set up the base size to start
        let mut sprite_width: usize = 8 << attr1.size().as_usize();
        let mut sprite_height: usize = 8 << attr1.size().as_usize();

        if attr0.shape().as_usize() == 1 {
            // this is a horizontal sprite
            if attr1.size().as_usize() < 2 {
                sprite_width <<= 1;
                sprite_height = 8;
            } else {
                sprite_height >>= 1;
            }
        } else if attr0.shape().as_usize() == 2 {
            // this is a vertical sprite
            if attr1.size().as_usize() < 2 {
                sprite_height <<= 1;
                sprite_width = 8;
            } else {
                sprite_width >>= 1;
            }
        }

        if screen_x < sprite_base_x
            || screen_y < sprite_base_y
            || screen_x >= sprite_base_x + sprite_width
            || screen_y >= sprite_base_y + sprite_height
        {
            // no part of this sprite could overlap with this pixel
            return None;
        }

        let sprite_x = {
            let offset = if attr1.horiz_flip() {
                sprite_width - sprite_base_x
            } else {
                sprite_base_x
            };
            screen_x - offset
        };
        let sprite_y = {
            let offset = if attr1.vert_flip() {
                sprite_height - sprite_base_y
            } else {
                sprite_base_y
            };
            screen_y - offset
        };

        let tile_x = sprite_x >> 3;
        let tile_y = sprite_y >> 3;

        let mut tile_index = attr2.tile().as_usize();
        let disp_ctrl = self.registers.disp_ctrl;
        if disp_ctrl.obj_char_mapping() {
            // the linear mapping
            tile_index += tile_x + tile_y * (sprite_width >> 3);
        } else {
            // the 2D mapping, 32x32 tiles
            tile_index += tile_x + tile_y * 32;
        };

        // if attr0.enable_256_colour() {
        //     todo!()
        // } else {
        //     // TODO refactor out
        //     const TILESET_OFFSET: usize = 64 * 1024;
        //     let tile_base = unsafe { self.vram.data().add(TILESET_OFFSET) };
        //     let base_ptr = tile_base as *const Tile4;
        //     let ptr = unsafe { base_ptr.add(tile_index) };
        //     let tile4 = unsafe { *ptr };

        //     let colour_index = tile4.get_colour(sprite_x & 0x7, sprite_y & 0x7).as_usize();
        //     if colour_index > 0 {
        //         Some(
        //             self.palette
        //                 .get_obj_colour_16(attr2.palette().as_usize(), colour_index),
        //         )
        //     } else {
        //         None
        //     }
        // }
        None
    }

    fn get_sprite_pixel(
        &self,
        sprite: usize,
        screen_x: usize,
        screen_y: usize,
    ) -> Option<DisplayColour> {
        if self.oam.get(sprite).is_rot_scale() {
            todo!();
        } else {
            self.get_sprite_pixel_normal(sprite, screen_x, screen_y)
        }
    }

    pub fn render_scanline(&self, scanline: usize) -> [DisplayColour; SCREEN_WIDTH] {
        // let display_control = self.registers.disp_ctrl;
        let bg_control = self.registers.bg_control;

        let mut pixels = [DisplayColour::init(0, 0, 0); SCREEN_WIDTH];
        let mut prio_list_raw = [Priority::new(0, false, 0); 128 + 4];
        let mut prio_item_count = 0;

        let mut sprite_x = [0u16; 240];
        let mut sprite_widths = [0u8; 240];

        for (num, obj) in self.oam.iter().enumerate() {
            unsafe {
                // todo! this does not handle rotscale sprites, it assumes all are normal
                if obj.normal.is_on_scanline(scanline) {
                    prio_list_raw[prio_item_count] =
                        Priority::new(obj.get_prio().as_u8(), false, num as u8);
                    prio_item_count += 1;

                    sprite_widths[num] = obj.normal.width() as u8;
                    let attr1 = obj.normal.attr1;
                    sprite_x[num] = attr1.x().as_u16();
                }
            }
        }
        for bg in 0..4 {
            prio_list_raw[prio_item_count] =
                Priority::new(bg_control[bg].bg_prio().as_u8(), true, bg as u8);
            prio_item_count += 1;
        }

        let prio_list = &mut prio_list_raw[0..prio_item_count];
        prio_list.sort_unstable();

        let mut colour: Option<DisplayColour> = None;

        // this assumes tile4 mode
        let mut bg_lines: [[MapTextEntry; 31]; 4] = [[MapTextEntry::zeroed(); 31]; 4];
        for (bg, line) in &mut bg_lines.iter_mut().enumerate() {
            self.get_bg_line_text_entries(bg, scanline, line);
        }

        for x in 0..SCREEN_WIDTH {
            for item in &mut *prio_list {
                if item.is_bg {
                    let o
                    colour = self.get_bg_pixel(item.num as usize, x, scanline);
                    break;
                }

                // todo! does not handle rotscale, assumes always normal
                let is_on_x = x >= sprite_x[item.num as usize] as usize
                    && x < sprite_widths[item.num as usize] as usize
                        + sprite_x[item.num as usize] as usize;

                if is_on_x {
                    colour = self.get_sprite_pixel(item.num as usize, x, scanline);
                    break;
                }
            }

            let final_colour: DisplayColour;
            if let Some(c) = colour {
                final_colour = c;
            } else {
                // colour 0 of palette 0 is the default colour if nothing else is opaque
                final_colour = self.palette.get_bg_colour_16(0, 0);
            }
            pixels[x] = final_colour;
        }

        pixels
    }
}
