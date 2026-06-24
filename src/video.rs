use std::{cmp::Reverse, usize};

use crate::registers::*;
use arbitrary_int::prelude::*;
use bilge::*;

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
    data: *mut u8,
}

impl VRAM {
    pub fn init(addr: *mut u8) -> Self {
        Self { data: addr }
    }
}

#[repr(C, packed)]
pub struct Palette {
    pub bg: [DisplayColour; 256],
    pub obj: [DisplayColour; 256],
}

impl Palette {
    fn get_bg_colour_256(&self, colour: usize) -> DisplayColour {
        self.bg[colour]
    }

    fn get_bg_colour_16(&self, palette: usize, colour: usize) -> DisplayColour {
        self.bg[palette * 16 + colour]
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct PrioNum {
    prio: usize,
    num: usize,
}

pub struct Video<'a> {
    pub registers: &'a mut DisplayRegisters,
    pub palette: &'a mut Palette,
    pub vram: &'a mut VRAM,
}

impl Video<'_> {
    // Get the base address of a tile set given the current status of the control registers
    fn get_tileset_base_addr(&self, bg: usize) -> *const u8 {
        const TILESET_OFFSET: usize = 16 * 1024;
        let register = self.registers.bg_control[bg];
        unsafe {
            self.vram
                .data
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
                .data
                .add(register.tilemap_base().as_usize() * MAP_OFFSET)
        }
    }

    // Get info about a tile map entry assuming this BG is in text mode
    fn get_map_text_entry(&self, bg: usize, x: usize, y: usize) -> MapTextEntry {
        // tiles are 8x8 pixels
        const TILE_SIZE_LOG: usize = 3;
        let register = self.registers.bg_control[bg];

        // get the width in tiles of this bg's map
        let is_wide = register.tilemap_size().value() & 0b1 == 0b1;
        let map_width = if is_wide {
            512 >> TILE_SIZE_LOG // pixels to num of tiles conversion
        } else {
            256 >> TILE_SIZE_LOG
        };

        // get the location of the tile entry in this map
        let tile_x = x >> TILE_SIZE_LOG;
        let tile_y = y >> TILE_SIZE_LOG;

        // finally get the value of the tile entry
        let base_ptr = self.get_map_base_addr(bg) as *const MapTextEntry;
        let ptr = unsafe { base_ptr.add(tile_y * map_width + tile_x) };
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

    pub fn get_pixel(&self, x: usize, y: usize) -> DisplayColour {
        let display_control = self.registers.disp_ctrl;
        let bg_regs = self.registers.bg_control;

        // figure out the display order, prio first, then if tie go to number
        let mut prio_num_pairs: Vec<PrioNum> = (0..=3)
            .map(|num| PrioNum {
                prio: bg_regs[num].bg_prio().as_usize(),
                num: num,
            })
            .collect();
        prio_num_pairs.sort();

        // go through in prio order and if enabled continue until we find a non-transparent pixel
        let mut colour: Option<DisplayColour> = None;
        for num in prio_num_pairs.iter().map(|pn| pn.num) {
            if let Some(bg_colour) = self.get_bg_pixel(num, x, y)
                && display_control.screen_disp_bg_at(num)
            {
                colour = Some(bg_colour);
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
        final_colour
    }
}
