// Launched by the main zig program to open sdl and display pixels
// Workaround since zig currently won't link properly to sdl and fails to start
//
// Build with: gcc -lSDL2 display.c -o display

#include <SDL2/SDL.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

#define DISP_WIDTH 240
#define DISP_HEIGHT 160
#define BUFSIZE 1024

typedef uint16_t colour_t;

static char buf[BUFSIZE];
static bool send_frame = false;

uint32_t timer_callback(uint32_t interval, void *_) {
    send_frame = true;
    return interval;
}

int main(int argc, char *argv[]) {
    int pipe = atoi(argv[0]);

    int scale = 3;
    int width = DISP_WIDTH * scale;
    int height = DISP_HEIGHT * scale;

    SDL_Init(SDL_INIT_VIDEO);
    SDL_Window *window = SDL_CreateWindow("Frame", SDL_WINDOWPOS_UNDEFINED,
                                          SDL_WINDOWPOS_UNDEFINED, width,
                                          height, SDL_WINDOW_SHOWN);
    SDL_Surface *screen = SDL_GetWindowSurface(window);
    SDL_Surface *pixels = SDL_CreateRGBSurfaceWithFormat(
        0, width, height, 16, SDL_PIXELFORMAT_ABGR1555);
    SDL_TimerID timer = SDL_AddTimer(30, timer_callback, NULL);

    bool quit = false;
    int i = 0;
    int frame = 0;
    while (!quit) {
        int read_count = read(pipe, buf, 2);
        colour_t colour = *(colour_t *)buf;

        int x = i % DISP_WIDTH;
        int y = i / DISP_WIDTH;
        for (int py = 0; py < scale; py++) {
            for (int px = 0; px < scale; px++) {
                int index = (scale * y + py) * width + (scale * x + px);
                ((colour_t *)(pixels->pixels))[index] = colour;
            }
        }

        i++;
        if (i == (DISP_WIDTH * DISP_HEIGHT)) {
            i = 0;
        }

        // printf("colour: 0x%X\n", colour);
        // printf("frame: %d, i: %d, x: %d, y: %d\n", frame, i, x, y);

        if (send_frame) {
            frame++;

            SDL_BlitSurface(pixels, NULL, screen, NULL);
            SDL_UpdateWindowSurface(window);
            send_frame = false;

            SDL_Event event;
            SDL_PollEvent(&event);
            switch (event.type) {
            case SDL_QUIT:
                printf("bye\n");
                quit = true;
                break;
            }
        }
    }

    SDL_RemoveTimer(timer);
    SDL_DestroyWindow(window);
    SDL_Quit();
}
