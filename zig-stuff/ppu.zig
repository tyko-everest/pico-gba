const std = @import("std");

const DISP_WIDTH: u64 = 240;
const DISP_HEIGHT: u64 = 160;

pub fn main() !void {
    const pipes = try std.os.pipe();
    const write_pipe = pipes[1];

    var buf = std.mem.zeroes([4]u8);
    _ = try std.fmt.bufPrint(&buf, "{}", .{pipes[0]});
    const read_pipe = @ptrCast([*:0]const u8, &buf);

    const arg_list: [:null]const ?[*:0]const u8 = &.{ read_pipe, null };
    const env_list: [:null]const ?[*:0]const u8 = &.{ "XDG_RUNTIME_DIR=/run/user/1000", null };

    const pid = std.os.linux.fork();
    if (pid == 0) {
        const err = std.os.execveZ("./display", arg_list, env_list);
        std.debug.print("{}", .{err});
    }

    while (true) {
        var colour: u16 = 0xF000;
        _ = try std.os.write(write_pipe, @ptrCast(*[2]u8, &colour));
    }
}
