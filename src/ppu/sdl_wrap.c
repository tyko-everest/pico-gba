#include <SDL2/SDL.h>
#include <stdbool.h>
#include <stdio.h>

#define DISP_WIDTH 240
#define DISP_HEIGHT 160

void setup() {
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
}

// int main() { setup(); }
