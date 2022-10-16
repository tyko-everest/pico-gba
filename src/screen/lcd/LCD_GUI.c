/*****************************************************************************
 * | File      	:	LCD_GUI.c
 * | Author      :   Waveshare team
 * | Function    :	Achieve drawing: draw points, lines, boxes, circles and
 *                   their size, solid dotted line, solid rectangle hollow
 *					rectangle, solid circle hollow circle.
 * | Info        :
 *   Achieve display characters: Display a single character, string, number
 *   Achieve time display: adaptive size display time minutes and seconds
 *----------------
 * |	This version:   V1.0
 * | Date        :   2017-08-16
 * | Info        :   Basic version
 *
 ******************************************************************************/
#include "LCD_GUI.h"

extern LCD_DIS sLCD_DIS;
extern uint8_t id;
/******************************************************************************
function:	Coordinate conversion
******************************************************************************/
void GUI_Swop(POINT Point1, POINT Point2) {
    POINT Temp;
    Temp = Point1;
    Point1 = Point2;
    Point2 = Temp;
}

/******************************************************************************
function:	Coordinate conversion
******************************************************************************/
void GUI_Clear(COLOR Color) { LCD_Clear(Color); }

/******************************************************************************
function:	Draw Point(Xpoint, Ypoint) Fill the color
parameter:
    Xpoint		:   The x coordinate of the point
    Ypoint		:   The y coordinate of the point
    Color		:   Set color
    Dot_Pixel	:	point size
******************************************************************************/
void GUI_DrawPoint(POINT Xpoint, POINT Ypoint, COLOR Color, DOT_PIXEL Dot_Pixel,
                   DOT_STYLE DOT_STYLE) {
    if (Xpoint > sLCD_DIS.LCD_Dis_Column || Ypoint > sLCD_DIS.LCD_Dis_Page) {
        // DEBUG("GUI_DrawPoint Input exceeds the normal display range\r\n");
        return;
    }

    uint16_t XDir_Num, YDir_Num;
    if (DOT_STYLE == DOT_STYLE_DFT) {
        for (XDir_Num = 0; XDir_Num < 2 * Dot_Pixel - 1; XDir_Num++) {
            for (YDir_Num = 0; YDir_Num < 2 * Dot_Pixel - 1; YDir_Num++) {
                LCD_SetPointlColor(Xpoint + XDir_Num - Dot_Pixel,
                                   Ypoint + YDir_Num - Dot_Pixel, Color);
            }
        }
    } else {
        for (XDir_Num = 0; XDir_Num < Dot_Pixel; XDir_Num++) {
            for (YDir_Num = 0; YDir_Num < Dot_Pixel; YDir_Num++) {
                LCD_SetPointlColor(Xpoint + XDir_Num - 1, Ypoint + YDir_Num - 1,
                                   Color);
            }
        }
    }
}

/******************************************************************************
function:	Draw a line of arbitrary slope
parameter:
    Xstart ：Starting x point coordinates
    Ystart ：Starting x point coordinates
    Xend   ：End point x coordinate
    Yend   ：End point y coordinate
    Color  ：The color of the line segment
******************************************************************************/
void GUI_DrawLine(POINT Xstart, POINT Ystart, POINT Xend, POINT Yend,
                  COLOR Color, LINE_STYLE Line_Style, DOT_PIXEL Dot_Pixel) {
    if (Xstart > sLCD_DIS.LCD_Dis_Column || Ystart > sLCD_DIS.LCD_Dis_Page ||
        Xend > sLCD_DIS.LCD_Dis_Column || Yend > sLCD_DIS.LCD_Dis_Page) {
        // DEBUG("GUI_DrawLine Input exceeds the normal display range\r\n");
        return;
    }

    if (Xstart > Xend)
        GUI_Swop(Xstart, Xend);
    if (Ystart > Yend)
        GUI_Swop(Ystart, Yend);

    POINT Xpoint = Xstart;
    POINT Ypoint = Ystart;
    int32_t dx =
        (int32_t)Xend - (int32_t)Xstart >= 0 ? Xend - Xstart : Xstart - Xend;
    int32_t dy =
        (int32_t)Yend - (int32_t)Ystart <= 0 ? Yend - Ystart : Ystart - Yend;

    // Increment direction, 1 is positive, -1 is counter;
    int32_t XAddway = Xstart < Xend ? 1 : -1;
    int32_t YAddway = Ystart < Yend ? 1 : -1;

    // Cumulative error
    int32_t Esp = dx + dy;
    int8_t Line_Style_Temp = 0;

    for (;;) {
        Line_Style_Temp++;
        // Painted dotted line, 2 point is really virtual
        if (Line_Style == LINE_DOTTED && Line_Style_Temp % 3 == 0) {
            // DEBUG("LINE_DOTTED\r\n");
            GUI_DrawPoint(Xpoint, Ypoint, LCD_BACKGROUND, Dot_Pixel,
                          DOT_STYLE_DFT);
            Line_Style_Temp = 0;
        } else {
            GUI_DrawPoint(Xpoint, Ypoint, Color, Dot_Pixel, DOT_STYLE_DFT);
        }
        if (2 * Esp >= dy) {
            if (Xpoint == Xend)
                break;
            Esp += dy;
            Xpoint += XAddway;
        }
        if (2 * Esp <= dx) {
            if (Ypoint == Yend)
                break;
            Esp += dx;
            Ypoint += YAddway;
        }
    }
}

/******************************************************************************
function:	Draw a rectangle
parameter:
    Xstart ：Rectangular  Starting x point coordinates
    Ystart ：Rectangular  Starting x point coordinates
    Xend   ：Rectangular  End point x coordinate
    Yend   ：Rectangular  End point y coordinate
    Color  ：The color of the Rectangular segment
    Filled : Whether it is filled--- 1 solid 0：empty
******************************************************************************/
void GUI_DrawRectangle(POINT Xstart, POINT Ystart, POINT Xend, POINT Yend,
                       COLOR Color, DRAW_FILL Filled, DOT_PIXEL Dot_Pixel) {
    if (Xstart > sLCD_DIS.LCD_Dis_Column || Ystart > sLCD_DIS.LCD_Dis_Page ||
        Xend > sLCD_DIS.LCD_Dis_Column || Yend > sLCD_DIS.LCD_Dis_Page) {
        // DEBUG("Input exceeds the normal display range\r\n");
        return;
    }

    if (Xstart > Xend)
        GUI_Swop(Xstart, Xend);
    if (Ystart > Yend)
        GUI_Swop(Ystart, Yend);

    if (Filled) {
#if LOW_Speed_Show
        POINT Ypoint;
        for (Ypoint = Ystart; Ypoint < Yend; Ypoint++) {
            GUI_DrawLine(Xstart, Ypoint, Xend, Ypoint, Color, LINE_SOLID,
                         Dot_Pixel);
        }
#elif HIGH_Speed_Show
        LCD_SetArealColor(Xstart, Ystart, Xend, Yend, Color);
#endif
    } else {
        GUI_DrawLine(Xstart, Ystart, Xend, Ystart, Color, LINE_SOLID,
                     Dot_Pixel);
        GUI_DrawLine(Xstart, Ystart, Xstart, Yend, Color, LINE_SOLID,
                     Dot_Pixel);
        GUI_DrawLine(Xend, Yend, Xend, Ystart, Color, LINE_SOLID, Dot_Pixel);
        GUI_DrawLine(Xend, Yend, Xstart, Yend, Color, LINE_SOLID, Dot_Pixel);
    }
}

/******************************************************************************
function:	Use the 8-point method to draw a circle of the
                specified size at the specified position.
parameter:
    X_Center  ：Center X coordinate
    Y_Center  ：Center Y coordinate
    Radius    ：circle Radius
    Color     ：The color of the ：circle segment
    Filled    : Whether it is filled: 1 filling 0：Do not
******************************************************************************/
void GUI_DrawCircle(POINT X_Center, POINT Y_Center, LENGTH Radius, COLOR Color,
                    DRAW_FILL Draw_Fill, DOT_PIXEL Dot_Pixel) {
    if (X_Center > sLCD_DIS.LCD_Dis_Column ||
        Y_Center >= sLCD_DIS.LCD_Dis_Page) {
        // DEBUG("GUI_DrawCircle Input exceeds the normal display range\r\n");
        return;
    }

    // Draw a circle from(0, R) as a starting point
    int16_t XCurrent, YCurrent;
    XCurrent = 0;
    YCurrent = Radius;

    // Cumulative error,judge the next point of the logo
    int16_t Esp = 3 - (Radius << 1);

    int16_t sCountY;
    if (Draw_Fill == DRAW_FULL) {
        while (XCurrent <= YCurrent) { // Realistic circles
            for (sCountY = XCurrent; sCountY <= YCurrent; sCountY++) {
                GUI_DrawPoint(X_Center + XCurrent, Y_Center + sCountY, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 1
                GUI_DrawPoint(X_Center - XCurrent, Y_Center + sCountY, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 2
                GUI_DrawPoint(X_Center - sCountY, Y_Center + XCurrent, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 3
                GUI_DrawPoint(X_Center - sCountY, Y_Center - XCurrent, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 4
                GUI_DrawPoint(X_Center - XCurrent, Y_Center - sCountY, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 5
                GUI_DrawPoint(X_Center + XCurrent, Y_Center - sCountY, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 6
                GUI_DrawPoint(X_Center + sCountY, Y_Center - XCurrent, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT); // 7
                GUI_DrawPoint(X_Center + sCountY, Y_Center + XCurrent, Color,
                              DOT_PIXEL_DFT, DOT_STYLE_DFT);
            }
            if (Esp < 0)
                Esp += 4 * XCurrent + 6;
            else {
                Esp += 10 + 4 * (XCurrent - YCurrent);
                YCurrent--;
            }
            XCurrent++;
        }
    } else { // Draw a hollow circle
        while (XCurrent <= YCurrent) {
            GUI_DrawPoint(X_Center + XCurrent, Y_Center + YCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 1
            GUI_DrawPoint(X_Center - XCurrent, Y_Center + YCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 2
            GUI_DrawPoint(X_Center - YCurrent, Y_Center + XCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 3
            GUI_DrawPoint(X_Center - YCurrent, Y_Center - XCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 4
            GUI_DrawPoint(X_Center - XCurrent, Y_Center - YCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 5
            GUI_DrawPoint(X_Center + XCurrent, Y_Center - YCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 6
            GUI_DrawPoint(X_Center + YCurrent, Y_Center - XCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 7
            GUI_DrawPoint(X_Center + YCurrent, Y_Center + XCurrent, Color,
                          Dot_Pixel, DOT_STYLE_DFT); // 0

            if (Esp < 0)
                Esp += 4 * XCurrent + 6;
            else {
                Esp += 10 + 4 * (XCurrent - YCurrent);
                YCurrent--;
            }
            XCurrent++;
        }
    }
}
