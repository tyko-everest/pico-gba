use crate::profile;
use crate::registers::*;
use arbitrary_int::prelude::*;
use bilge::*;
use core::ops::Deref;

// tiles are 8x8 pixels
const TILE_SIZE_LOG: usize = 3;
const TILE_SIZE: usize = 1 << TILE_SIZE_LOG;
const TILE_MASK: usize = TILE_SIZE - 1;
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: usize = 160;

// Final colour generated for the display
#[bitsize(16)]
#[derive(FromBits, Copy, Clone)]
pub struct DisplayColour {
    red: u5,
    green: u5,
    blue: u5,
    _unused: bool,
}

impl DisplayColour {
    pub fn to_minifb_format(&self) -> u32 {
        self.red().as_u32() << (16 + 3)
            | self.green().as_u32() << (8 + 3)
            | self.blue().as_u32() << 3
    }

    pub fn to_rgb565_format(&self) -> u16 {
        self.red().as_u16() << 11 | self.green().as_u16() << 6 | self.blue().as_u16()
    }
}

// One entry of a text mode BG map
#[bitsize(16)]
#[derive(FromBits, Copy, Clone)]
struct MapTextEntry {
    tile: u10,
    horiz_flip: bool,
    vert_flip: bool,
    palette: u4,
}

// One entry of a rotation/scaling BG map
// todo! not used until rotation/scaling BGs are drawn
#[allow(dead_code)]
struct MapRotScaleEntry(u8);

// align(4) keeps the size the same but lets tile rows be read as whole words
#[repr(C, align(4))]
pub struct VRAM {
    _data: [u8; 96 * 1024],
}

impl VRAM {
    pub const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self._data
    }

    // offset must be 2 byte aligned and inside VRAM
    #[inline(always)]
    fn read_u16(&self, offset: usize) -> u16 {
        debug_assert!(offset % 2 == 0 && offset + 2 <= self._data.len());
        unsafe { (self._data.as_ptr().add(offset) as *const u16).read() }
    }

    // offset must be 4 byte aligned and inside VRAM
    #[inline(always)]
    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset % 4 == 0 && offset + 4 <= self._data.len());
        unsafe { (self._data.as_ptr().add(offset) as *const u32).read() }
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

// size and shape are in the same place for both kinds of sprite
impl ObjAttrNormal {
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
}

impl Deref for OAM {
    type Target = [ObjAttr; 128];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// BG tiles and maps can only use the first 64K of VRAM
const BG_VRAM_SIZE: usize = 0x10000;
// OBJ tiles live in the last 32K of VRAM in BG modes 0-2
const OBJ_VRAM_BASE: usize = 0x10000;
const OBJ_VRAM_MASK: usize = 0x7FFF;

// The palette cache holds the BG palette followed by the OBJ palette
const OBJ_PALETTE: usize = 256;

// Lines are drawn into a buffer with a margin on each side, so tiles that are partly off
// screen can be drawn whole instead of being clipped
const LINE_MARGIN: usize = TILE_SIZE;
const LINE_BUFFER: usize = LINE_MARGIN + (SCREEN_WIDTH / TILE_SIZE + 1) * TILE_SIZE;

const SPRITE_8BPP: u8 = 1 << 0;
const SPRITE_HFLIP: u8 = 1 << 1;
const SPRITE_VFLIP: u8 = 1 << 2;

// A sprite decoded from OAM once per frame, so scanlines don't re-read OAM
#[derive(Copy, Clone)]
struct CachedSprite {
    // screen x of the left edge, negative when partly off the left of the screen
    x: i16,
    y: u8,
    width: u8,
    height: u8,
    // first tile number, in 32 byte units
    tile: u16,
    // palette cache index of colour 0 for this sprite
    palette: u16,
    prio: u8,
    flags: u8,
}

impl CachedSprite {
    const fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

// The sprites that can appear on screen this frame, in OAM order
struct SpriteCache {
    sprites: [CachedSprite; 128],
    count: usize,
    // for each scanline, bit n is set when sprites[n] covers that line
    line_masks: [[u32; 4]; SCREEN_HEIGHT],
}

impl SpriteCache {
    const fn new() -> Self {
        Self {
            sprites: [CachedSprite::zeroed(); 128],
            count: 0,
            line_masks: [[0; 4]; SCREEN_HEIGHT],
        }
    }
}

pub struct Video<'a> {
    pub registers: &'a mut DisplayRegisters,
    pub palette: &'a mut Palette,
    pub vram: &'a mut VRAM,
    pub oam: &'a mut OAM,
    sprite_cache: SpriteCache,
    // palette RAM converted to the output format, BG then OBJ
    palette_cache: [u16; 512],
    // converts a GBA colour (BGR555) to the format render_scanline_into outputs
    colour_format: fn(u16) -> u16,
}

impl<'a> Video<'a> {
    pub fn new(
        registers: &'a mut DisplayRegisters,
        palette: &'a mut Palette,
        vram: &'a mut VRAM,
        oam: &'a mut OAM,
    ) -> Self {
        Self {
            registers,
            palette,
            vram,
            oam,
            sprite_cache: SpriteCache::new(),
            palette_cache: [0; 512],
            colour_format: |colour| colour,
        }
    }

    // Output colours in another format, such as the one a display expects. Colours are converted
    // once per frame in begin_frame, so this costs nothing per pixel. The default is BGR555.
    pub fn set_colour_format(&mut self, colour_format: fn(u16) -> u16) {
        self.colour_format = colour_format;
    }

    // Snapshot the state that only needs reading once per frame; call before rendering scanline 0
    #[inline(never)]
    #[cfg_attr(
        all(target_arch = "arm", target_os = "none"),
        unsafe(link_section = ".data.ram_func")
    )]
    pub fn begin_frame(&mut self) {
        profile::start();

        // bit 15 of a palette entry is unused on the GBA
        let convert = self.colour_format;
        for i in 0..256 {
            self.palette_cache[i] = convert(u16::from(self.palette.bg[i]) & 0x7FFF);
            self.palette_cache[OBJ_PALETTE + i] = convert(u16::from(self.palette.obj[i]) & 0x7FFF);
        }

        let cache = &mut self.sprite_cache;
        cache.count = 0;
        cache.line_masks = [[0; 4]; SCREEN_HEIGHT];
        for obj in self.oam.iter() {
            // the fields both kinds of sprite share are read through the normal layout
            let shared = unsafe { obj.normal };
            let attr0 = shared.attr0;
            let attr1 = shared.attr1;
            let attr2 = shared.attr2;

            // mode 2 is the OBJ window which is never drawn, mode 3 and shape 3 are prohibited
            if attr0.mode().as_u8() >= 2 || attr0.shape().as_u8() == 3 {
                continue;
            }
            let width = shared.width();
            let height = shared.height();

            let mut flags = 0;
            // how far right and down the sprite is drawn from its position
            let (mut offset_x, mut offset_y) = (0, 0);
            if let Some(normal) = obj.get_normal() {
                let (attr0, attr1) = (normal.attr0, normal.attr1);
                if attr0.disable() {
                    continue;
                }
                if attr1.horiz_flip() {
                    flags |= SPRITE_HFLIP;
                }
                if attr1.vert_flip() {
                    flags |= SPRITE_VFLIP;
                }
            } else if let Some(rot_scale) = obj.get_rot_scale() {
                // todo! rotation and scaling are not drawn, these sprites are drawn as if their
                // matrix was the identity. Double size doubles the area the sprite is drawn in,
                // which centres an unscaled sprite inside it.
                let attr0 = rot_scale.attr0;
                if attr0.double_size() {
                    (offset_x, offset_y) = (width / 2, height / 2);
                }
            }

            // y wraps at 256, so a sprite is on a line when (line - y) & 0xFF < height
            let y = attr0.y().as_u8().wrapping_add(offset_y as u8);
            if y as usize >= SCREEN_HEIGHT && y as usize + height <= 256 {
                continue;
            }

            // x is 9 bits and wraps at 512, anything from 240 up starts left of the screen
            let mut x = attr1.x().as_u16() as i16;
            if x >= SCREEN_WIDTH as i16 {
                x -= 512;
            }
            x += offset_x as i16;
            if x + width as i16 <= 0 {
                continue;
            }

            let palette = if attr0.enable_256_colour() {
                flags |= SPRITE_8BPP;
                OBJ_PALETTE
            } else {
                OBJ_PALETTE + attr2.palette().as_usize() * 16
            };

            cache.sprites[cache.count] = CachedSprite {
                x,
                y,
                width: width as u8,
                height: height as u8,
                tile: attr2.tile().as_u16(),
                palette: palette as u16,
                prio: attr2.prio().as_u8(),
                flags,
            };

            // y wraps at 256, so a sprite near the bottom can continue at the top
            let (word, bit) = (cache.count / 32, 1 << (cache.count % 32));
            for row in 0..height {
                let line = (y as usize + row) & 0xFF;
                if line < SCREEN_HEIGHT {
                    cache.line_masks[line][word] |= bit;
                }
            }
            cache.count += 1;
        }
        profile::mark(4);
    }
}

impl Video<'_> {
    // Draw one line of a text mode BG over the line buffer
    fn draw_text_bg(&self, bg: usize, scanline: usize, line: &mut [u16; LINE_BUFFER]) {
        let control = self.registers.bg_control[bg];
        let offset = self.registers.bg_offset[bg];
        let (offset_x, offset_y) = (offset.x, offset.y);

        // text BGs always wrap around at the edges of the map
        let bg_x = offset_x.offset().as_usize() & control.width_mask_pixels();
        let bg_y = (offset_y.offset().as_usize() + scanline) & control.height_mask_pixels();
        let tile_y = bg_y >> TILE_SIZE_LOG;
        let row = bg_y & TILE_MASK;

        // maps are made of 32x32 tile screenblocks of 2K, SC0 SC1 on the top row and SC2 SC3 below
        const SCREENBLOCK: usize = 0x800;
        let mut map_row = control.tilemap_base().as_usize() * SCREENBLOCK + (tile_y & 31) * 64;
        if tile_y >= 32 {
            map_row += if control.is_wide() {
                2 * SCREENBLOCK
            } else {
                SCREENBLOCK
            };
        }
        let tileset = control.tileset_base().as_usize() * 0x4000;
        let width_mask_tiles = control.width_mask_tiles();

        let mut tile_x = bg_x >> TILE_SIZE_LOG;
        // the first tile starts up to 7 pixels left of the screen, inside the margin
        let mut pos = LINE_MARGIN - (bg_x & TILE_MASK);
        let palette_type = control.palette_type();

        // the map entry's tile number is bits 0-9, then hflip, vflip and the palette in bits 12-15
        let row_pixels = row * 4;
        let flipped_row_pixels = (TILE_MASK - row) * 4;
        for _ in 0..(SCREEN_WIDTH / TILE_SIZE + 1) {
            let column = tile_x & width_mask_tiles;
            let mut entry_addr = map_row + (column & 31) * 2;
            if column >= 32 {
                entry_addr += SCREENBLOCK;
            }
            let entry = self.vram.read_u16(entry_addr) as usize;
            let hflip = entry & 0x400 != 0;
            let vflip = entry & 0x800 != 0;
            let tile = entry & 0x3FF;
            // pos stays inside the line buffer: it starts at most 8 in and moves 8 per tile
            let dst: &mut [u16; TILE_SIZE] =
                unsafe { &mut *(line.as_mut_ptr().add(pos) as *mut [u16; TILE_SIZE]) };
            pos += TILE_SIZE;
            tile_x += 1;

            match palette_type {
                PaletteType::P16 => {
                    let tile_row = if vflip {
                        flipped_row_pixels
                    } else {
                        row_pixels
                    };
                    let addr = tileset + tile * 32 + tile_row;
                    if addr < BG_VRAM_SIZE {
                        let pixels = self.vram.read_u32(addr);
                        if pixels != 0 {
                            let base = (entry >> 12) * 16;
                            let palette =
                                (&self.palette_cache[base..base + 16]).try_into().unwrap();
                            draw_tile_row_4bpp(dst, pixels, palette, hflip);
                        }
                    }
                }
                PaletteType::P256 => {
                    let tile_row = if vflip {
                        flipped_row_pixels
                    } else {
                        row_pixels
                    } * 2;
                    let addr = tileset + tile * 64 + tile_row;
                    if addr < BG_VRAM_SIZE {
                        let low = self.vram.read_u32(addr);
                        let high = self.vram.read_u32(addr + 4);
                        let palette = (&self.palette_cache[..256]).try_into().unwrap();
                        draw_tile_row_8bpp(dst, low, high, palette, hflip);
                    }
                }
            }
        }
    }

    // Draw one line of a sprite over the line buffer
    fn draw_sprite(
        &self,
        sprite: &CachedSprite,
        scanline: usize,
        obj_1d: bool,
        line: &mut [u16; LINE_BUFFER],
    ) {
        let mut row = (scanline as u8).wrapping_sub(sprite.y) as usize;
        if sprite.flags & SPRITE_VFLIP != 0 {
            row = sprite.height as usize - 1 - row;
        }
        let tiles_wide = sprite.width as usize >> TILE_SIZE_LOG;
        let p256 = sprite.flags & SPRITE_8BPP != 0;
        let hflip = sprite.flags & SPRITE_HFLIP != 0;
        // 256 colour tiles take two tile numbers
        let tile_step = if p256 { 2 } else { 1 };

        // 1D mapping places a sprite's tiles one after another, 2D uses a 32 tile wide matrix
        let row_stride = if obj_1d { tiles_wide * tile_step } else { 32 };
        let row_tile = sprite.tile as usize + (row >> TILE_SIZE_LOG) * row_stride;
        let tile_row = row & TILE_MASK;

        for tile_index_x in 0..tiles_wide {
            let screen_x = sprite.x as isize + (tile_index_x << TILE_SIZE_LOG) as isize;
            if screen_x <= -(TILE_SIZE as isize) || screen_x >= SCREEN_WIDTH as isize {
                continue;
            }
            let pos = (screen_x + LINE_MARGIN as isize) as usize;
            let dst = (&mut line[pos..pos + TILE_SIZE]).try_into().unwrap();

            let source_x = if hflip {
                tiles_wide - 1 - tile_index_x
            } else {
                tile_index_x
            };
            let tile = row_tile + source_x * tile_step;
            let palette = sprite.palette as usize;

            // tile numbers wrap within the 32K OBJ area
            if p256 {
                let addr = OBJ_VRAM_BASE + ((tile * 32 + tile_row * 8) & OBJ_VRAM_MASK);
                let low = self.vram.read_u32(addr);
                let high = self.vram.read_u32(addr + 4);
                let palette = (&self.palette_cache[palette..palette + 256])
                    .try_into()
                    .unwrap();
                draw_tile_row_8bpp(dst, low, high, palette, hflip);
            } else {
                let addr = OBJ_VRAM_BASE + ((tile * 32 + tile_row * 4) & OBJ_VRAM_MASK);
                let pixels = self.vram.read_u32(addr);
                let palette = (&self.palette_cache[palette..palette + 16])
                    .try_into()
                    .unwrap();
                draw_tile_row_4bpp(dst, pixels, palette, hflip);
            }
        }
    }

    // Render a scanline in the colour format set by set_colour_format (BGR555 by default)
    // on the microcontroller this runs from RAM, flash is slow and its cache is shared with the game
    #[inline(never)]
    #[cfg_attr(
        all(target_arch = "arm", target_os = "none"),
        unsafe(link_section = ".data.ram_func")
    )]
    pub fn render_scanline_into(&self, scanline: usize, out: &mut [u16; SCREEN_WIDTH]) {
        profile::start();
        let display_control = self.registers.disp_ctrl;

        // forced blank shows a white screen
        if display_control.forced_blank() {
            out.fill((self.colour_format)(0x7FFF));
            profile::mark(3);
            return;
        }

        // colour 0 of BG palette 0 shows wherever every layer is transparent
        let mut line = [self.palette_cache[0]; LINE_BUFFER];

        // todo! affine BGs (modes 1 and 2) and bitmap modes (3-5) are not drawn
        let text_bgs: u8 = match display_control.bg_mode().as_u8() {
            0 => 0b1111,
            1 => 0b0011,
            _ => 0,
        };
        let mut bgs = 0u8;
        for bg in 0..4 {
            if display_control.screen_disp_bg_at(bg) {
                bgs |= 1 << bg;
            }
        }
        bgs &= text_bgs;
        let bg_control = self.registers.bg_control;

        // the sprites on this line, from the highest OAM number to the lowest
        let sprites = &self.sprite_cache.sprites;
        let mut line_sprites = [0u8; 128];
        let mut line_sprite_count = 0;
        if display_control.screen_disp_obj() {
            let masks = &self.sprite_cache.line_masks[scanline];
            for word in (0..4).rev() {
                let mut bits = masks[word];
                while bits != 0 {
                    let bit = 31 - bits.leading_zeros() as usize;
                    bits &= !(1 << bit);
                    line_sprites[line_sprite_count] = (word * 32 + bit) as u8;
                    line_sprite_count += 1;
                }
            }
        }
        let line_sprites = &line_sprites[..line_sprite_count];
        let obj_1d = display_control.obj_char_mapping();
        profile::mark(0);

        // draw back to front: at each priority BGs go from BG3 to BG0 then sprites go from the
        // highest OAM number to the lowest, so BG0 beats other BGs and sprites beat BGs on ties
        // todo! the hardware picks between overlapping sprites by OAM number before priority
        for prio in (0..4).rev() {
            for bg in (0..4).rev() {
                if bgs & (1 << bg) != 0 && bg_control[bg].bg_prio().as_u8() == prio {
                    self.draw_text_bg(bg, scanline, &mut line);
                }
            }
            profile::mark(1);
            for &num in line_sprites {
                let sprite = &sprites[num as usize];
                if sprite.prio == prio {
                    self.draw_sprite(sprite, scanline, obj_1d, &mut line);
                }
            }
            profile::mark(2);
        }

        out.copy_from_slice(&line[LINE_MARGIN..LINE_MARGIN + SCREEN_WIDTH]);
        profile::mark(3);
    }

    // Render a scanline as GBA colours, only correct while set_colour_format is left at the default
    pub fn render_scanline(&self, scanline: usize) -> [DisplayColour; SCREEN_WIDTH] {
        let mut line = [0; SCREEN_WIDTH];
        self.render_scanline_into(scanline, &mut line);
        line.map(DisplayColour::from)
    }
}

// Reverse the order of the 8 nibbles of a 4 bit tile row
#[inline(always)]
fn flip_nibbles(pixels: u32) -> u32 {
    let swapped = pixels.swap_bytes();
    ((swapped & 0x0F0F_0F0F) << 4) | ((swapped >> 4) & 0x0F0F_0F0F)
}

// Draw the 8 pixels of a 4 bit tile row, colour 0 is transparent
#[inline(always)]
fn draw_tile_row_4bpp(dst: &mut [u16; TILE_SIZE], pixels: u32, palette: &[u16; 16], hflip: bool) {
    if pixels == 0 {
        return;
    }
    let pixels = if hflip { flip_nibbles(pixels) } else { pixels };
    // a nibble is zero when the classic haszero trick flags its top bit
    let has_zero = pixels.wrapping_sub(0x1111_1111) & !pixels & 0x8888_8888;
    // indices are masked to 4 bits so the palette lookups need no bounds checks
    let colour = |shift: u32| unsafe { *palette.get_unchecked(((pixels >> shift) & 0xF) as usize) };
    if has_zero == 0 {
        dst[0] = colour(0);
        dst[1] = colour(4);
        dst[2] = colour(8);
        dst[3] = colour(12);
        dst[4] = colour(16);
        dst[5] = colour(20);
        dst[6] = colour(24);
        dst[7] = colour(28);
        return;
    }
    let mut rest = pixels;
    for i in 0..TILE_SIZE {
        if rest & 0xF != 0 {
            dst[i] = palette[(rest & 0xF) as usize];
        }
        rest >>= 4;
    }
}

// Draw the 8 pixels of an 8 bit tile row given as two words, colour 0 is transparent
#[inline(always)]
fn draw_tile_row_8bpp(
    dst: &mut [u16; TILE_SIZE],
    low: u32,
    high: u32,
    palette: &[u16; 256],
    hflip: bool,
) {
    if low | high == 0 {
        return;
    }
    let (low, high) = if hflip {
        (high.swap_bytes(), low.swap_bytes())
    } else {
        (low, high)
    };
    for (half, mut pixels) in [low, high].into_iter().enumerate() {
        let base = half * 4;
        let has_zero = pixels.wrapping_sub(0x0101_0101) & !pixels & 0x8080_8080;
        if has_zero == 0 {
            for i in 0..4 {
                dst[base + i] = palette[((pixels >> (8 * i)) & 0xFF) as usize];
            }
            continue;
        }
        for i in 0..4 {
            let colour = (pixels & 0xFF) as usize;
            if colour != 0 {
                dst[base + i] = palette[colour];
            }
            pixels >>= 8;
        }
    }
}

#[cfg(test)]
mod tile_row_tests {
    use super::*;

    fn naive_4bpp(pixels: u32, palette: &[u16; 16], hflip: bool) -> [u16; 8] {
        let mut dst = [0xAAAA; 8];
        for i in 0..8 {
            let colour = ((pixels >> (4 * i)) & 0xF) as usize;
            if colour != 0 {
                dst[if hflip { 7 - i } else { i }] = palette[colour];
            }
        }
        dst
    }

    fn naive_8bpp(low: u32, high: u32, palette: &[u16; 256], hflip: bool) -> [u16; 8] {
        let mut dst = [0xAAAA; 8];
        let bytes = [low.to_le_bytes(), high.to_le_bytes()].concat();
        for (i, &colour) in bytes.iter().enumerate() {
            if colour != 0 {
                dst[if hflip { 7 - i } else { i }] = palette[colour as usize];
            }
        }
        dst
    }

    #[test]
    fn tile_rows_match_the_simple_version() {
        let palette4: [u16; 16] = core::array::from_fn(|i| 0x100 + i as u16);
        let palette8: [u16; 256] = core::array::from_fn(|i| 0x100 + i as u16);
        let mut seed = 12345u32;
        let mut next = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for round in 0..20000 {
            // mix fully opaque, sparse and random rows
            let mut a = next();
            let mut b = next();
            match round % 3 {
                0 => {
                    a |= 0x1111_1111;
                    b |= 0x0101_0101;
                }
                1 => {
                    a &= next();
                    b &= next() & next();
                }
                _ => {}
            }
            for hflip in [false, true] {
                let mut dst = [0xAAAA; 8];
                draw_tile_row_4bpp(&mut dst, a, &palette4, hflip);
                assert_eq!(dst, naive_4bpp(a, &palette4, hflip), "4bpp {a:#x} {hflip}");
                let mut dst = [0xAAAA; 8];
                draw_tile_row_8bpp(&mut dst, a, b, &palette8, hflip);
                assert_eq!(
                    dst,
                    naive_8bpp(a, b, &palette8, hflip),
                    "8bpp {a:#x} {b:#x}"
                );
            }
        }
    }
}
