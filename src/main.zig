const c = @cImport({
    @cInclude("screen/config/DEV_Config.h");
    @cInclude("screen/lcd/LCD_Driver.h");
    @cInclude("screen/lcd/LCD_GUI.h");
});

fn paint_scren(colour: u16) void {
    c.LCD_SetWindow(0, 0, 240, 320);
    c.LCD_SetColor(colour, 240, 320);
}

export fn main() void {
    c.System_Init();
    c.LCD_Init(c.SCAN_DIR_DFT, 800);

    c.paint_screen(0x0000);
    c.paint_screen(0x07e0);

    while (true) {}
}
