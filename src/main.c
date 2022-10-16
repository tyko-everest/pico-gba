/**
 * Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
 *
 * SPDX-License-Identifier: BSD-3-Clause
 */

#include "DEV_Config.h"
#include "LCD_Bmp.h"
#include "LCD_Driver.h"
#include "LCD_GUI.h"
#include "LCD_Touch.h"
#include "hardware/watchdog.h"
#include "pico/stdlib.h"
#include <stdio.h>

int main() {
    System_Init();
    LCD_Init(SCAN_DIR_DFT, 800);

    LCD_SetWindow(0, 0, 240, 320);
    LCD_SetColor(0x0000, 240, 320);
    LCD_SetWindow(0, 0, 160, 240);
    LCD_SetColor(0x07e0, 160, 240);

    for (;;)
        ;
    return 0;
}
