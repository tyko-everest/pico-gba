const std = @import("std");

const DISP_WIDTH: usize = 240;
const DISP_HEIGHT: usize = 160;
const TILE_WIDTH: usize = 8;
const TILE_HEIGHT: usize = 8;

const SDLState = struct {
    write_pipe: std.os.fd_t,
};

const Display = struct {
    state: SDLState,

    pub fn new() Display {
        const pipes = std.os.pipe() catch unreachable;
        const write_pipe = pipes[1];

        var buf = std.mem.zeroes([4]u8);
        _ = std.fmt.bufPrint(&buf, "{}", .{pipes[0]}) catch unreachable;
        const read_pipe = @ptrCast([*:0]const u8, &buf);

        const arg_list: [:null]const ?[*:0]const u8 = &.{ read_pipe, null };
        const env_list: [:null]const ?[*:0]const u8 = &.{ "XDG_RUNTIME_DIR=/run/user/1000", null };

        const pid = std.os.linux.fork();
        if (pid == 0) {
            const err = std.os.execveZ("./display", arg_list, env_list);
            std.debug.print("{}", .{err});
        }

        return Display{
            .state = .{
                .write_pipe = write_pipe,
            },
        };
    }

    pub fn push_pixel(self: Display, colour: u16) void {
        _ = std.os.write(self.state.write_pipe, @ptrCast(*const [2]u8, &colour)) catch unreachable;
    }
};

// https://problemkaputt.de/gbatek.htm#lcdiodisplaycontrol
const Registers = packed struct {
    disp_ctrl: packed struct {
        bg_mode: u3,
        cgb_mode: u1,
        disp_frame: u1,
        hblank_interval_free: u1,
        obj_char_mapping: u1,
        forced_blank: u1,
        // when zig supports arrays of u1 in packed structs switch this
        screen_disp_bg0: u1,
        screen_disp_bg1: u1,
        screen_disp_bg2: u1,
        screen_disp_bg3: u1,
        screen_disp_obj: u1,
        disp_win0: u1,
        disp_win1: u1,
        disp_obj_win: u1,
    },
    green_swap: u16,
    disp_status: packed struct {
        vblank_flag: u1,
        hblank_flag: u1,
        vcounter_flag: u1,
        vblank_irq_en: u1,
        hblank_irq_en: u1,
        vcounter_irq_en: u1,
        unused: u2,
        vcounter_setting: u8,
    },
    vert_counter: packed struct {
        curr_scanline: u8,
        unused: u8,
    },
    bg_control: [4]packed struct {
        bg_prio: u2,
        tileset_base: u2,
        unused1: u2 = 0,
        mosaic: u1,
        palette_mode: u1,
        tilemap_base: u5,
        disp_area_overflow: u1,
        tilemap_size: u2,
    },
    bg_offset: [4]packed struct {
        horiz: u9,
        unused1: u7 = 8,
        vert: u9,
        unused2: u7 = 8,
    },
    RES: [8]u8,
};

const Tile4 = packed struct {
    // can't use array of u4 here, zig doesn't support arrays of integers that aren't byte multiples
    data: [32]packed struct {
        low: u4,
        high: u4,
    },

    fn get(self: Tile4, x: usize, y: usize) u4 {
        const byte = self.data[(y * TILE_WIDTH + x) >> 1];
        if (x & 1 == 0) {
            return byte.low;
        } else {
            return byte.high;
        }
    }
};

const Tile8 = packed struct {
    data: [64]u8,

    fn get(self: Tile8, x: usize, y: usize) u8 {
        return self.data[y * TILE_WIDTH + x];
    }
};

const TileMapEntry = packed struct {
    index: u10,
    horiz_flip: u1,
    vert_flip: u1,
    palette_bank: u4,
};

const VideoMem = packed struct {
    bg: packed union {
        tile4s: [2 * 1024]Tile4,
        tile8s: [1 * 1024]Tile8,
        map_entries: [32 * 1024]TileMapEntry,
    },
    obj: packed union {
        temp: [32 * 1024]u8,
    },
};

const Colour = packed union {
    raw: u16,
    channels: packed struct {
        r: u5,
        g: u5,
        b: u5,
        x: u1 = 1,
    },

    pub fn is_transparent(self: Colour) bool {
        if (self.channels.r == 0 and self.channels.g == 0 and self.channels.b == 0) {
            return true;
        } else {
            return false;
        }
    }
};

const PaletteMem = packed struct {
    bg: [256]Colour,
    sprite: [256]Colour,
};

const ObjAttr = packed struct {
    y_coord: u8,
};

const ObjAttrMem = packed struct {
    temp: ObjAttr,
};

const PPU = struct {
    display: Display,
    registers: Registers,
    video_mem: *const VideoMem,
    palette_mem: *const PaletteMem,
    obj_attr_mem: ObjAttrMem,

    pub fn new() PPU {
        const vram_raw = @embedFile("vram.bin");
        const palette_raw = @embedFile("palette.bin");
        return PPU{
            .display = Display.new(),
            .registers = std.mem.zeroes(Registers),
            .video_mem = @ptrCast(*const VideoMem, vram_raw),
            .palette_mem = @ptrCast(*const PaletteMem, palette_raw),
            .obj_attr_mem = std.mem.zeroes(ObjAttrMem),
        };
    }

    // Return of the given background supports affline transformations (rotation and scaling)
    // given the current state of the registers
    fn is_affline(self: PPU, bg_num: usize) bool {
        const mode = self.registers.disp_ctrl.bg_mode;
        if (mode >= 2) {
            return true;
        } else if (mode == 1) {
            if (bg_num < 2) {
                return false;
            } else {
                return true;
            }
        } else {
            return false;
        }
    }

    // Given coordinates and a background that we know is drawing in regular tiled mode, return the pixel colour
    fn get_reg_bg_pixel(self: PPU, x_screen: usize, y_screen: usize, bg_num: usize) Colour {
        const bg_control = self.registers.bg_control[bg_num];
        const bg_offset = self.registers.bg_offset[bg_num];

        const x_bg = x_screen + bg_offset.horiz;
        const y_bg = y_screen + bg_offset.vert;

        const bg_width: usize = if (bg_control.tilemap_size & 0b01 == 0) 256 else 512;
        // const bg_height = if (bg_control.tilemap_size & 0b10 == 0) 256 else 512;

        const tilemap_index = 1024 * @as(usize, bg_control.tilemap_base) + (y_bg / TILE_HEIGHT) * (bg_width / TILE_WIDTH) + x_bg / TILE_WIDTH;
        const tilemap_entry = self.video_mem.bg.map_entries[tilemap_index];

        const x_tile = if (tilemap_entry.horiz_flip == 0) x_bg % 8 else 7 - x_bg % 8;
        const y_tile = if (tilemap_entry.vert_flip == 0) y_bg % 8 else 7 - y_bg % 8;

        var palette_index: usize = 0;
        if (bg_control.palette_mode == 0) {
            // 4-bit tiles
            const tile = self.video_mem.bg.tile4s[@as(usize, bg_control.tileset_base) * 512 + tilemap_entry.index];
            palette_index = @as(u8, tilemap_entry.palette_bank) << 4 | tile.get(x_tile, y_tile);
        } else {
            // 8-bit tiles
            const tile = self.video_mem.bg.tile8s[@as(usize, bg_control.tileset_base) * 256 + tilemap_entry.index];
            palette_index = tile.get(x_tile, y_tile);
        }
        return self.palette_mem.bg[palette_index];
    }

    // Given coordinates and a background that we know is drawing in regular affline mode, return the pixel colour
    // fn get_affline_bg_pixel(self: PPU) Colour {
    //     return .{ 0, 0, 0, 1 };
    // }

    // Given screen coordinates, return what colour the background pixel should be
    // Used in the main loop as x goes 0 -> 240 and y goes 0 -> 160 each frame
    pub fn get_bg_pixel(self: PPU, _: usize, _: usize) Colour {
        const disp_ctrl = self.registers.disp_ctrl;
        switch (disp_ctrl.mode) {
            0 => {
                // this needs to go through each of the four backgrounds,
                // don't bother with ones that are disabled,
                // go through them based on priority,
                // call get_reg_bg_pixel on each,
                // stop and return the first non-transparent pixel
                if (disp_ctrl.screen_disp_bg0 == 1) {
                    // const colour =
                }
            },
            1 => {},
            2 => {},
            3 => {},
            4 => {},
            5 => {},
            6, 7 => {},
        }
    }

    // Given screen coordinates, return what colour the pixel should be
    // This function first checks if it should draw a sprite, if not goes to backgrounds
    // Used in the main loop as x goes 0 -> 240 and y goes 0 -> 160 each frame
    pub fn get_pixel(_: PPU, _: usize, _: usize) Colour {}
};

pub fn main() void {
    var ppu = PPU.new();

    // test setup based on pokemon emerald start screen
    ppu.registers.bg_control[0].tileset_base = 2;
    ppu.registers.bg_control[0].tilemap_base = 26;

    ppu.registers.bg_control[1].tileset_base = 3;
    ppu.registers.bg_control[1].tilemap_base = 27;
    ppu.registers.bg_control[1].tilemap_size = 0;

    // ppu.registers.bg_control[2].palette_mode = 1;
    // ppu.registers.bg_control[2].tileset_base = 0;
    // ppu.registers.bg_control[2].tilemap_base = 9;
    // ppu.registers.bg_control[2].tilemap_size = 1;
    // const colour_test = ppu.get_reg_bg_pixel(16, 64, 2);

    var frame: u64 = 0;
    while (true) {
        const start_time = std.time.milliTimestamp();
        var y: usize = 0;
        while (y < DISP_HEIGHT) : (y += 1) {
            var x: usize = 0;
            while (x < DISP_WIDTH) : (x += 1) {
                var colour = ppu.get_reg_bg_pixel(x, y, 0);
                colour.channels.x = 1;
                ppu.display.push_pixel(colour.raw);
                std.debug.print("colour raw: {}\n", .{colour.raw});
                std.debug.print("15 bit colour: r: {}, g: {}, b: {}\n", .{ colour.channels.r, colour.channels.g, colour.channels.b });
                // std.debug.print("24 bit colour: r: {}, g: {}, b: {}\n", .{ @as(usize, colour.channels.r) * 8, @as(usize, colour.channels.g) * 8, @as(usize, colour.channels.b) * 8 });
            }
        }
        frame += 1;
        const end_time = std.time.milliTimestamp();

        std.debug.print("frame time: {} ms\n", .{end_time - start_time});
    }
}
