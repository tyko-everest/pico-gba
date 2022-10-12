add_subdirectory(screen/config)
add_subdirectory(screen/lcd)
add_subdirectory(screen/font)
add_subdirectory(screen/fatfs)
add_subdirectory(screen/sdcard)

include_directories(./screen/config)
include_directories(./screen/lcd)
include_directories(./screen/font)
include_directories(./screen/fatfs)
include_directories(./screen/sdcard)

add_executable(main
        main.c
        )

# pull in common dependencies
target_link_libraries(main lcd font config pico_stdlib hardware_spi fatfs)

# enable usb output, disable uart output
pico_enable_stdio_usb(main 1)
pico_enable_stdio_uart(main 0)

# create map/bin/hex file etc.
pico_add_extra_outputs(main)

# add url via pico_set_program_url
# example_auto_set_url(main)
