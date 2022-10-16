const std = @import("std");

pub fn build(b: *std.build.Builder) !void {
    var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena.deinit();
    const alloc = arena.allocator();

    const pico_sdk_path = b.env_map.get("PICO_SDK_PATH") orelse "not found";
    var pico_sdk_build_path = try alloc.alloc(u8, pico_sdk_path.len + "/build".len);
    std.mem.copy(u8, pico_sdk_build_path[0..], pico_sdk_path);
    std.mem.copy(u8, pico_sdk_build_path[pico_sdk_path.len..], "/build");

    const mode = b.standardReleaseOptions();
    const target = b.standardTargetOptions(.{});

    const pico_sdk_setup = b.addSystemCommand(&[_][]const u8{ "cmake", "-B", pico_sdk_build_path, "-S", pico_sdk_path, "-DCMAKE_BUILD_TYPE=Debug" });
    try pico_sdk_setup.step.make();
    const pico_sdk_build = b.addSystemCommand(&[_][]const u8{ "cmake", "--build", pico_sdk_build_path, "-j" });
    try pico_sdk_build.step.make();

    const exe = b.addExecutable("main", "src/main.zig");
    exe.setBuildMode(mode);
    exe.setTarget(target);
    // exe.addLibPath("deps/dotherside/build/lib");
    exe.linkLibC();
    exe.install();

}
