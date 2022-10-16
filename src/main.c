/**
 * Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
 *
 * SPDX-License-Identifier: BSD-3-Clause
 */

#include "DEV_Config.h"
#include "LCD_Driver.h"
#include "LCD_GUI.h"

void paint_screen(uint16_t colour) {
    LCD_SetWindow(0, 0, 240, 320);
    LCD_SetColor(colour, 240, 320);
}

int main() {
    System_Init();
    LCD_Init(SCAN_DIR_DFT, 800);

    paint_screen(0x0000);
    paint_screen(0x07e0);

    for (;;)
        ;
    return 0;
}
