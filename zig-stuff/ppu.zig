const std = @import("std");

const DISP_WIDTH: u64 = 240;
const DISP_HEIGHT: u64 = 160;

const SDLState = struct {
    write_pipe: std.os.fd_t,
};

const PPU = struct {
    state: SDLState,

    pub fn new() PPU {
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

        return PPU{
            .state = .{
                .write_pipe = write_pipe,
            },
        };
    }

    pub fn push_pixel(self: PPU, colour: u16) void {
        _ = std.os.write(self.state.write_pipe, @ptrCast(*const [2]u8, &colour)) catch unreachable;
    }
};

pub fn main() !void {
    const ppu = PPU.new();

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
                ppu.push_pixel(colour);
            }
        }
        frame += 1;
        const end_time = std.time.milliTimestamp();
        std.debug.print("frame time: {}\n", .{end_time - start_time});
    }
}
