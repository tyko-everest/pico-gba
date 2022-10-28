const std = @import("std");

const DISP_WIDTH: u64 = 240;
const DISP_HEIGHT: u64 = 160;

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
        screen_disp_bg: [4]u1,
        screen_disp_obj: u1,
        disp_win: [2]u1,
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

const Tile4 = struct {
    data: [64]u4,
};

const Tile8 = struct {
    data: [64]u8,
};

const TileSet = packed union {
    tile4: [512]Tile4,
    tile8: [256]Tile8,
};

const TileMapEntry = packed struct {
    index: u10,
    horiz_flip: u1,
    vert_flip: u1,
    palette_bank: u4,
};

const VideoMem = packed union {
    raw: [96 * 1024]u8,
    data: packed struct {
        bg: packed union {
            tilesets: [4]TileSet,
            tilemaps: [32][32]TileMapEntry,
        },
        obj: packed union {
            temp: u8,
        },
    },
};

const Colour = packed struct {
    r: u5,
    g: u5,
    b: u5,
    x: u1 = 1,
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
    video_mem: VideoMem,
    palette_mem: PaletteMem,
    obj_attr_mem: ObjAttrMem,

    pub fn get_reg_bg_pixel(self: PPU, x: usize, y: usize, num: usize) Colour {
        const bg_control = self.registers.bg_control[num];
        const tileset = self.video_mem.data.bg.tilesets[bg_control.tileset_base];
        const tilemap = self.video_mem.data.bg.tilemaps[bg_control.tileset_base];

        const bg_offset = self.registers.bg_offset[num];
        const x_bg = x + bg_offset.horiz;
        const y_bg = y + bg_offset.vert;

        const tile_entry = tilemap[x_bg / 32][y_bg / 32];
        const tile_val = tileset[tile_entry];

        if (bg_control.palette_mode == 0) {
            // 4 bit palette
        } else {
            // 8 bit palette
            return self.palette_mem.bg[tile_val];
        }
    }
};

pub fn main() void {
    const display = Display.new();

    var frame: u64 = 0;
    while (true) {
        const start_time = std.time.milliTimestamp();
        var y: usize = 0;
        while (y < DISP_HEIGHT) : (y += 1) {
            var x: usize = 0;
            while (x < DISP_WIDTH) : (x += 1) {
                var colour: u16 = 0xF000;
                if (x == frame) {
                    colour = 0x800F;
                }
                display.push_pixel(colour);
            }
        }
        frame += 1;
        const end_time = std.time.milliTimestamp();
        std.debug.print("frame time: {}\n", .{end_time - start_time});
    }
}
