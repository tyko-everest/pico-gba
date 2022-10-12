/**
 * Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
 *
 * SPDX-License-Identifier: BSD-3-Clause
 */

#include "LCD_Driver.h"
#include "LCD_Touch.h"
#include "LCD_GUI.h"
#include "LCD_Bmp.h"
#include "DEV_Config.h"
#include <stdio.h>
#include "hardware/watchdog.h"
#include "pico/stdlib.h"

int lcd_test(void)
{
    uint8_t counter = 0;

    System_Init();
    SD_Init();

    LCD_SCAN_DIR lcd_scan_dir = SCAN_DIR_DFT;
    LCD_Init(lcd_scan_dir, 800);
    TP_Init(lcd_scan_dir);
    GUI_Show();
    Driver_Delay_ms(2000);
    GUI_Clear(0xf800);
    Driver_Delay_ms(1000);
    GUI_Clear(0x07e0);
    Driver_Delay_ms(1000);
    GUI_Clear(0x001f);
    Driver_Delay_ms(1000);
    LCD_SCAN_DIR bmp_scan_dir = D2U_R2L;
    LCD_Show_bmp(bmp_scan_dir, lcd_scan_dir);
    TP_GetAdFac();
    TP_Dialog();
    while (1)
    {
        TP_DrawBoard();
    }
    return 0;
}

int main()
{
    while (1)
    {
        lcd_test();
    }
    return 0;
}
