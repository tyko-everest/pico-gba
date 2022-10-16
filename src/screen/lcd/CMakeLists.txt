aux_source_directory(. DIR_LCD_SRCS)

include_directories(../config)

add_library(lcd ${DIR_LCD_SRCS})
target_link_libraries(lcd PUBLIC config pico_stdlib)
