#ifndef PPU_H
#define PPU_H

#include <SDL2/SDL.h>
#include <stdbool.h>
#include <stdint.h>

#define DISP_WIDTH 240
#define DISP_HEIGHT 160
#define TILE_WIDTH 8
#define TILE_HEIGHT 8

typedef uint16_t screen_colour_t;

typedef struct __attribute__((__packed__)) {
    uint8_t data[32];
} tile_4bit_t;

typedef struct __attribute__((__packed__)) {
    uint8_t data[64];
} tile_8bit_t;

typedef struct __attribute__((__packed__)) {
    uint8_t data[16 * 1024];
} charblock_t;

typedef struct {
    uint16_t DISPCNT;
    uint16_t GRN_SWP;
    uint16_t DISPSTAT;
    uint16_t VCOUNT;
    uint16_t BG0CNT;
    uint16_t BG1CNT;
    uint16_t BG2CNT;
    uint16_t BG3CNT;
    uint16_t BG0HOFS;
    uint16_t BG0VOFS;
    uint16_t BG1HOFS;
    uint16_t BG1VOFS;
    uint16_t BG2HOFS;
    uint16_t BG2VOFS;
    uint16_t BG3HOFS;
    uint16_t BG3VOFS;
    uint8_t RES[8];
} ppu_registers_t;

void paint_screen(uint16_t colour);

/**
 * @brief Get the pixel colour at screen location (x, y)
 *
 * @param x
 * @param y
 * @return screen_colour_t
 */
screen_colour_t get_pixel(int x, int y);

/**
 * @brief Get the colour of a pixel from the background
 *
 * @param x
 * @param y
 * @return screen_colour_t
 */
screen_colour_t get_bg_pixel(int x, int y);

/**
 * @brief Get the pixel from a 4bit tile
 *
 * @param tile
 * @param x
 * @param y
 * @return int
 */
int get_pixel_from_tile_4bit(tile_4bit_t *tile, int x, int y);

/**
 * @brief Get the pixel from a 8bit tile
 *
 * @param tile
 * @param x
 * @param y
 * @return int
 */
int get_pixel_from_tile_8bit(tile_8bit_t *tile, int x, int y);

#endif
