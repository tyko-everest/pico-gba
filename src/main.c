/**
 * Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
 *
 * SPDX-License-Identifier: BSD-3-Clause
 */

#include "DEV_Config.h"
#include "LCD_Driver.h"
#include "LCD_GUI.h"
#include "hardware/spi.h"
#include "ppu.h"
#include <string.h>

#define WIDTH 125
#define HEIGHT 125
#define BUF_SIZE (WIDTH * HEIGHT)

void paint_screen(uint16_t colour) {
    LCD_SetWindow(0, 0, WIDTH, HEIGHT);
    LCD_SetColor(colour, WIDTH, HEIGHT);
}

int main() {
    tile_4bit_t tile = {0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF};
    volatile int pixel = get_pixel_from_tile_4bit(&tile, 1, 1);

    System_Init();
    LCD_Init(L2R_U2D, 800);

    for (;;) {
        paint_screen(0x0000);
        paint_screen(0xFFFF);
    }
    return 0;
}
