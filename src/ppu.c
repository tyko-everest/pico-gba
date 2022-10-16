#include "ppu.h"

void paint_screen(uint16_t colour) {
    LCD_SetWindow(0, 0, 240, 320);
    LCD_SetColor(colour, 240, 320);
}