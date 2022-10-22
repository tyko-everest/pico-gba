#include "ppu.h"

static ppu_registers_t regs;

screen_colour_t get_bg_pixel(int x, int y) {}

int get_pixel_from_tile_4bit(tile_4bit_t *tile, int x, int y) {
    return (tile->data[x / 2 + y * (TILE_WIDTH / 2)] >> ((x & 1) * 4)) & 0xF;
}

int get_pixel_from_tile_8bit(tile_8bit_t *tile, int x, int y) {
    return tile->data[x + y * TILE_WIDTH] & 0xFF;
}

int main(int argc, char **argv) {

    int scale = 2;
    int width = DISP_WIDTH * scale;
    int height = DISP_HEIGHT * scale;

    SDL_Init(SDL_INIT_VIDEO);
    SDL_Window *window = SDL_CreateWindow("Frame", SDL_WINDOWPOS_UNDEFINED,
                                          SDL_WINDOWPOS_UNDEFINED, width,
                                          height, SDL_WINDOW_SHOWN);
    SDL_Surface *screen = SDL_GetWindowSurface(window);
    SDL_Surface *pixels = SDL_CreateRGBSurfaceWithFormat(
        0, width, height, 32, SDL_PIXELFORMAT_RGBX8888);

    bool quit = false;
    int i = 0;
    while (!quit) {
        SDL_Event event;
        SDL_PollEvent(&event);
        switch (event.type) {
        case SDL_QUIT:
            quit = true;
            break;
        }

        // SDL_FillRect(pixels, NULL, 0);

        unsigned char byte = 0xFF;
        ((char *)(pixels->pixels))[i] = byte;
        i++;
        if (i >= width * height) {
            i = 0;
        }

        SDL_BlitSurface(pixels, NULL, screen, NULL);
        SDL_UpdateWindowSurface(window);
    }

    SDL_DestroyWindow(window);
    SDL_Quit();
    return 0;
}