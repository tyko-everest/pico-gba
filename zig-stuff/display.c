#include <SDL2/SDL.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

#define DISP_WIDTH 240
#define DISP_HEIGHT 160
#define BUFSIZE 1024

static char buf[BUFSIZE];
static bool send_frame = false;

uint32_t timer_callback(uint32_t interval, void *_) {
    send_frame = true;
    return interval;
}

int main(int argc, char *argv[]) {
    int pipe = atoi(argv[0]);

    int scale = 2;
    int width = DISP_WIDTH * scale;
    int height = DISP_HEIGHT * scale;

    SDL_Init(SDL_INIT_VIDEO);
    SDL_Window *window = SDL_CreateWindow("Frame", SDL_WINDOWPOS_UNDEFINED,
                                          SDL_WINDOWPOS_UNDEFINED, width,
                                          height, SDL_WINDOW_SHOWN);
    SDL_Surface *screen = SDL_GetWindowSurface(window);
    SDL_Surface *pixels = SDL_CreateRGBSurfaceWithFormat(
        0, width, height, 16, SDL_PIXELFORMAT_ARGB1555);
    SDL_TimerID timer = SDL_AddTimer(33, timer_callback, NULL);

    bool quit = false;
    int i = 0;
    unsigned short colour = 0xAFF0;

    while (!quit) {
        int read_count = read(pipe, buf, 2);
        uint16_t colour = *(uint16_t *)buf;

        ((unsigned short *)(pixels->pixels))[i] = colour;
        i++;
        if (i >= width * height) {
            i = 0;
        }

        if (send_frame) {
            SDL_BlitSurface(pixels, NULL, screen, NULL);
            SDL_UpdateWindowSurface(window);
            send_frame = false;

            SDL_Event event;
            SDL_PollEvent(&event);
            switch (event.type) {
            case SDL_QUIT:
                quit = true;
                break;
            }
        }
    }

    SDL_RemoveTimer(timer);
    SDL_DestroyWindow(window);
    SDL_Quit();
}
