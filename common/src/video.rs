use crate::registers::*;
use arbitrary_int::prelude::*;
use bilge::*;
use core::usize;

// tiles are 8x8 pixels
const TILE_SIZE_LOG: usize = 3;

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

// Used to pack two 4-bit colour values into a type that can be packed properly
#[bitsize(8)]
#[derive(Copy, Clone)]
struct Tile4Entry {
    data: [u4; 2],
}

// One tile in 4-bit colour mode
#[derive(Copy, Clone)]
struct Tile4 {
    data: [[Tile4Entry; 4]; 8],
}

impl Tile4 {
    pub fn get_colour(&self, x: usize, y: usize) -> u4 {
        self.data[y][x >> 1].data_at(x & 1)
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
union ObjAttr {
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
        let register = self.registers.bg_control[bg];
        unsafe {
            self.vram
                .data()
                .add(register.tilemap_base().as_usize() * MAP_OFFSET)
        }
    }

    // Get info about a tile map entry assuming this BG is in text mode
    fn get_map_text_entry(&self, bg: usize, x: usize, y: usize) -> MapTextEntry {
        let register = self.registers.bg_control[bg];

        // get the location of the tile entry in this map
        let tile_x = x >> TILE_SIZE_LOG;
        let tile_y = y >> TILE_SIZE_LOG;

        // finally get the value of the tile entry
        let base_ptr = self.get_map_base_addr(bg) as *const MapTextEntry;
        let ptr = unsafe { base_ptr.add(tile_y * register.width_in_tiles() + tile_x) };
        unsafe { *ptr }
    }

    fn get_bg_pixel_mode_0(&self, bg: usize, x: usize, y: usize) -> Option<DisplayColour> {
        let register = self.registers.bg_control[bg];
        let map_entry = self.get_map_text_entry(bg, x, y);
        if register.palette_mode() {
            let tile = self.get_tile8(bg, map_entry.tile().as_usize());
            let tile_colour = tile.get_colour(x & 0b111, y & 0b111).as_usize();
            if tile_colour == 0 {
                None
            } else {
                Some(self.palette.get_bg_colour_256(tile_colour))
            }
        } else {
            let tile = self.get_tile4(bg, map_entry.tile().as_usize());
            let tile_colour = tile.get_colour(x & 0b111, y & 0b111).as_usize();
            if tile_colour == 0 {
                None
            } else {
                Some(
                    self.palette
                        .get_bg_colour_16(map_entry.palette().as_usize(), tile_colour),
                )
            }
        }
    }

    fn get_bg_pixel(&self, bg: usize, x: usize, y: usize) -> Option<DisplayColour> {
        let register = self.registers.disp_ctrl;
        match register.bg_mode().as_u8() {
            0 => self.get_bg_pixel_mode_0(bg, x, y),
            1 => todo!(),
            2 => todo!(),
            3 => todo!(),
            4 => todo!(),
            5 => todo!(),
            val => panic!("Undefined BG Mode {val}"),
        }
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

        if attr0.enable_256_colour() {
            todo!()
        } else {
            // TODO refactor out
            const TILESET_OFFSET: usize = 64 * 1024;
            let tile_base = unsafe { self.vram.data().add(TILESET_OFFSET) };
            let base_ptr = tile_base as *const Tile4;
            let ptr = unsafe { base_ptr.add(tile_index) };
            let tile4 = unsafe { *ptr };

            let colour_index = tile4.get_colour(sprite_x & 0x7, sprite_y & 0x7).as_usize();
            if colour_index > 0 {
                Some(
                    self.palette
                        .get_obj_colour_16(attr2.palette().as_usize(), colour_index),
                )
            } else {
                None
            }
        }
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

    fn get_sprite_prio(&self, sprite: u8) -> Priority {
        let prio = self.oam.get(sprite as usize).get_prio().as_u8();
        Priority::new(prio, false, sprite)
    }

    fn get_bg_prio(&self, bg: u8) -> Priority {
        let bg_ctrl = self.registers.bg_control[bg as usize];
        let prio = bg_ctrl.bg_prio().as_u8();
        Priority::new(prio, true, bg)
    }

    pub fn get_pixel(&self, x: usize, y: usize) -> DisplayColour {
        let display_control = self.registers.disp_ctrl;
        let bg_regs = self.registers.bg_control;
        let bg_offsets = self.registers.bg_offset;

        let mut curr_prio = Priority::new(4, true, 128);
        let mut colour: Option<DisplayColour> = None;

        for sprite in 0..128 {
            // todo not handling rot scale mode
            if self.oam.get(sprite).get_normal().unwrap().is_disabled() {
                continue;
            }
            if let Some(c) = self.get_sprite_pixel(sprite, x, y) {
                let new_prio = self.get_sprite_prio(sprite as u8);
                if new_prio < curr_prio {
                    curr_prio = new_prio;
                    colour = Some(c)
                }
            }
        }

        for bg in 0..4 {
            if !display_control.screen_disp_bg_at(bg) {
                continue;
            }

            // get the x offset register
            let mut offset_x = {
                let reg = bg_offsets[bg].x;
                reg.offset().as_usize()
            };
            // get the current map's width
            let width = bg_regs[bg].width_in_tiles() << TILE_SIZE_LOG;
            // wrap the offset to within range if needed
            if offset_x >= width {
                offset_x -= width;
            }
            // get the x coordinate in that background map
            let mut bg_x = x + offset_x;
            if bg_x >= width {
                bg_x -= width;
            }

            // get the y offset register
            let mut offset_y = {
                let reg = bg_offsets[bg].y;
                reg.offset().as_usize()
            };
            // get the current map's height
            let height = bg_regs[bg].height_in_tiles() << TILE_SIZE_LOG;
            // wrap the offset to within range if needed
            if offset_y >= height {
                offset_y -= height;
            }
            // get the y coordinate in that background map
            let mut bg_y = y + offset_y;
            if bg_y >= height {
                bg_y -= height;
            }

            if let Some(bg_colour) = self.get_bg_pixel(bg, bg_x, bg_y) {
                let new_prio = self.get_bg_prio(bg as u8);
                if new_prio < curr_prio {
                    curr_prio = new_prio;
                    colour = Some(bg_colour)
                }
            }
        }

        let final_colour: DisplayColour;
        if let Some(c) = colour {
            final_colour = c;
        } else {
            // colour 0 of palette 0 is the default colour if nothing else is opaque
            final_colour = self.palette.get_bg_colour_16(0, 0);
        }
        final_colour
    }
}
