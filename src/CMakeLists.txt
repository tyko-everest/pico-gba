add_subdirectory(screen/config)
add_subdirectory(screen/lcd)

include_directories(./screen/config)
include_directories(./screen/lcd)

add_executable(main
        main.c
        ppu.c
        )

# pull in common dependencies
target_link_libraries(main lcd config pico_stdlib hardware_spi)

# enable usb output, disable uart output
pico_enable_stdio_usb(main 1)
pico_enable_stdio_uart(main 0)

# create map/bin/hex file etc.
pico_add_extra_outputs(main)

# add url via pico_set_program_url
# example_auto_set_url(main)
