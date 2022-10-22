const std = @import("std");

pub fn main() !void {
    _ = try std.os.pipe();
    const null_list: [:null]const ?[*:0]const u8 = &[:null]?[*:0]const u8{null};
    _ = std.os.execveZ("ls", null_list, null_list);
}
