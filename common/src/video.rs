use crate::registers::*;
use arbitrary_int::prelude::*;
use bilge::*;
use core::{
    ops::{Deref, Index},
    ptr::read_volatile,
    todo, usize,
};

// tiles are 8x8 pixels
const TILE_SIZE_LOG: usize = 3;
const TILE_SIZE: usize = 1 << TILE_SIZE_LOG;
const TILE_MASK: usize = 0b111;
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: usize = 160;

// Final colour generated for the display
#[bitsize(16)]
#[derive(FromBits, Copy, Clone)]
pub struct DisplayColour {
    red: u5,
    green: u5,
    blue: u5,
    opaque: bool,
}

impl DisplayColour {
    pub fn init(r: u8, g: u8, b: u8) -> Self {
        Self::new(u5::new(r), u5::new(g), u5::new(b), false)
    }

    pub fn to_minifb_format(&self) -> u32 {
        self.red().as_u32() << (16 + 3)
            | self.green().as_u32() << (8 + 3)
            | self.blue().as_u32() << 3
    }

    pub fn to_rgb565_format(&self) -> u16 {
        self.red().as_u16() << 11 | self.green().as_u16() << 6 | self.blue().as_u16()
    }

    pub fn is_transparent(&self) -> bool {
        !self.opaque()
    }
}

// One line of pixels in 4-bit colour mode
#[derive(Copy, Clone)]
struct Tile4Line {
    data: u32,
}

impl Tile4Line {
    fn get_pixel(&self, x: usize) -> u4 {
        u4::from_u32((self.data >> (x * 4)) & 0xF)
    }
}

// One tile in 4-bit colour mode
#[derive(Copy, Clone)]
struct Tile4 {
    data: [Tile4Line; 8],
}

impl Tile4 {
    pub fn get_line(&self, y: usize) -> Tile4Line {
        unsafe { read_volatile(&self.data[y]) }
    }
}

// One line of pixels in 8-bit colour mode
#[derive(Copy, Clone)]
struct Tile8Line {
    data: [u8; 8],
}

impl Tile8Line {
    fn get_tile(&self, x: usize) -> u8 {
        self.data[x]
    }
}

// One tile in 8-bit colour mode
#[derive(Copy, Clone)]
struct Tile8 {
    data: [Tile8Line; 8],
}

impl Tile8 {
    pub fn get_line(&self, y: usize) -> Tile8Line {
        self.data[y]
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
    data: [MapTextEntry; 31],
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
    fn get_bg_colour_256(&self, palette_colour: usize) -> DisplayColour {
        let mut colour = self.bg[palette_colour];
        if palette_colour != 0 {
            colour.set_opaque(true);
        };
        colour
    }

    fn get_bg_colour_16(&self, palette_num: usize, palette_colour: usize) -> DisplayColour {
        let mut colour = self.bg[palette_num * 16 + palette_colour];
        if palette_colour != 0 {
            colour.set_opaque(true);
        };
        colour
    }

    fn get_obj_colour_256(&self, palette_colour: usize) -> DisplayColour {
        let mut colour = self.obj[palette_colour];
        if palette_colour != 0 {
            colour.set_opaque(true);
        };
        colour
    }

    fn get_obj_colour_16(&self, palette_num: usize, palette_colour: usize) -> DisplayColour {
        let mut colour = self.obj[palette_num * 16 + palette_colour];
        if palette_colour != 0 {
            colour.set_opaque(true);
        };
        colour
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
    fn get_tileset_base_addr(&self, offset: usize) -> *const u8 {
        const TILESET_OFFSET: usize = 16 * 1024;
        unsafe { self.vram.data().add(offset * TILESET_OFFSET) }
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

    fn get_sprite_tile4_line(&self, index: usize, y: usize) -> Tile4Line {
        // let tiles_ptr = self.get_tileset_base_addr(4) as *const [Tile4; 2048];
        // let tiles = unsafe { &*tiles_ptr };
        // tiles[index].get_line(y)

        let base_ptr = self.get_tileset_base_addr(4) as *const Tile4;
        let ptr = unsafe { base_ptr.add(index) };
        unsafe { ptr.read().get_line(y) }
    }

    fn get_sprite_tile8_line(&self, index: usize, y: usize) -> Tile8Line {
        let tiles_ptr = self.get_tileset_base_addr(4) as *const [Tile8; 1024];
        let tiles = unsafe { &*tiles_ptr };
        tiles[index].get_line(y)
    }

    fn get_bg_tileset_offset(&self, bg: usize) -> usize {
        let register = self.registers.bg_control[bg];
        register.tileset_base().as_usize()
    }

    // Get a specific 4-bit colour depth tile
    fn get_bg_tile4_line(&self, bg: usize, index: usize, y: usize) -> Tile4Line {
        let offset = self.get_bg_tileset_offset(bg);
        let tiles_ptr = self.get_tileset_base_addr(offset) as *const [Tile4; 1024];
        let tiles = unsafe { &*tiles_ptr };
        tiles[index].get_line(y)
    }

    fn get_bg_tile8_line(&self, bg: usize, index: usize, y: usize) -> Tile8Line {
        let offset = self.get_bg_tileset_offset(bg);
        let tiles_ptr = self.get_tileset_base_addr(offset) as *const [Tile8; 1024];
        let tiles = unsafe { &*tiles_ptr };
        tiles[index].get_line(y)
    }

    // Get info about a tile map entry assuming this BG is in text mode
    fn get_map_text_entry(&self, bg: usize, tile_x: usize, tile_y: usize) -> MapTextEntry {
        let bg_control = self.registers.bg_control[bg];
        let index = tile_y * bg_control.width_in_tiles() + tile_x;

        let entries_ptr = self.get_map_base_addr(bg) as *const [MapTextEntry; 4096];
        let entries = unsafe { &*entries_ptr };
        entries[index]
    }

    // For a background in a text-based mode, render that scanline
    fn render_bg_scanline(
        &self,
        bg: usize,
        scanline: usize,
        pixels: &mut [DisplayColour; SCREEN_WIDTH],
    ) {
        let bg_control = self.registers.bg_control[bg];
        let bg_offset = self.registers.bg_offset[bg];

        // coordinate (bg_x) of the background at screen_x = 0
        let mut bg_x = {
            let reg = bg_offset.x;
            reg.offset().as_usize()
        };
        // coordinate (bg_y) of the background at screen_x = 0
        let bg_y = {
            let reg = bg_offset.y;
            reg.offset().as_usize() + scanline
        };

        // keep track of which output screen_x we are generating right now
        let mut screen_x = 0;

        // iterate through the tiles we need to get to render this scanline
        for _ in 0..=31 {
            // x and y index of the tile in the background (not the pixel)
            // i.e. the coordinate in the background divided by tile size of 8
            let bg_tile_x = bg_x >> TILE_SIZE_LOG;
            let bg_tile_y = bg_y >> TILE_SIZE_LOG;

            // y index within that tile we are currently drawing (i.e. 0-7)
            let tile_y = bg_y & TILE_MASK;
            let entry = self.get_map_text_entry(bg, bg_tile_x, bg_tile_y);
            let tile4_line = self.get_bg_tile4_line(bg, entry.tile().as_usize(), tile_y);
            let tile8_line = self.get_bg_tile8_line(bg, entry.tile().as_usize(), tile_y);

            for tile_x in (bg_x & TILE_MASK)..TILE_SIZE {
                if bg_control.palette_mode() {
                    let palette_colour = tile8_line.get_tile(tile_x).as_usize();
                    let colour = self.palette.get_bg_colour_256(palette_colour);
                    pixels[screen_x] = colour;
                } else {
                    let palette_colour = tile4_line.get_pixel(tile_x).as_usize();
                    let colour = self
                        .palette
                        .get_bg_colour_16(entry.palette().as_usize(), palette_colour);
                    pixels[screen_x] = colour;
                }

                bg_x += 1;
                if bg_x >= bg_control.width_in_pixels() {
                    bg_x = bg_control.width_in_pixels();
                }

                screen_x += 1;
                if screen_x >= SCREEN_WIDTH {
                    return;
                }
            }
        }
    }

    pub fn render_scanline(&self, scanline: usize) -> [DisplayColour; SCREEN_WIDTH] {
        let display_control = self.registers.disp_ctrl;
        let bg_control = self.registers.bg_control;

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
            if display_control.screen_disp_bg_at(bg) {
                prio_list_raw[prio_item_count] =
                    Priority::new(bg_control[bg].bg_prio().as_u8(), true, bg as u8);
                prio_item_count += 1;
            }
        }

        let prio_list = &mut prio_list_raw[0..prio_item_count];
        prio_list.sort_unstable();

        // calculate the sprite pixels
        let mut sprite_pixels = [DisplayColour::init(0, 0, 0); SCREEN_WIDTH];
        for item in &mut *prio_list {
            if !item.is_bg {
                let sprite_num = item.num as usize;
                let oam = unsafe { self.oam[sprite_num].normal };
                let attr0 = oam.attr0;
                let attr1 = oam.attr1;
                let attr2 = oam.attr2;

                // location of sprite in pixels in screen-space
                let sprite_x = attr1.x().as_usize();
                let sprite_y = attr0.y().as_usize();
                // sprite dimensions in pixels
                let sprite_width = oam.width();
                let sprite_height = oam.height();

                // y in pixels in sprite-space
                let sprite_tile_y = scanline - sprite_y;

                // x and y in tiles (not pixels) in sprite-space
                let sprite_tile_index_y = sprite_tile_y >> TILE_SIZE_LOG;
                for sprite_tile_index_x in 0..(sprite_width >> TILE_SIZE_LOG) {
                    // linear sprite mapping
                    let tile_index = attr2.tile().as_usize()
                        + sprite_tile_index_y * (sprite_width >> TILE_SIZE_LOG)
                        + sprite_tile_index_x;
                    let tile4_line =
                        self.get_sprite_tile4_line(tile_index, sprite_tile_y & TILE_MASK);
                    for tile_x in 0..TILE_SIZE {
                        let palette_colour = tile4_line.get_pixel(tile_x).as_usize();
                        let colour = self
                            .palette
                            .get_obj_colour_16(attr2.palette().as_usize(), palette_colour);
                        let screen_x = sprite_x + (sprite_tile_index_x << TILE_SIZE_LOG) + tile_x;
                        if screen_x < SCREEN_WIDTH && colour.opaque() {
                            sprite_pixels[screen_x] = colour;
                        }
                    }
                }
            }
        }

        // start every pixel as the default colour if every other layer is transparent
        let default_colour = self.palette.get_bg_colour_16(0, 0);
        let mut pixels = [default_colour; SCREEN_WIDTH];

        let mut bg_pixels = [[DisplayColour::init(0, 0, 0); SCREEN_WIDTH]; 4];
        for bg in 0..4 {
            self.render_bg_scanline(bg, scanline, &mut bg_pixels[bg]);
        }

        'pixel_loop: for x in 0..SCREEN_WIDTH {
            for item in &mut *prio_list {
                if item.is_bg {
                    let colour = bg_pixels[item.num as usize][x];
                    if colour.opaque() {
                        pixels[x] = colour;
                        continue 'pixel_loop;
                    }
                } else {
                    let colour = sprite_pixels[x];
                    if colour.opaque() {
                        pixels[x] = colour;
                        continue 'pixel_loop;
                    }
                }
            }
        }

        pixels
    }
}
