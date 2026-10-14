// Touchscreen controls drawn on the half of the display below the GBA screen.
// Touch is read from the XPT2046 resistive touch controller, which shares SPI1 with the display.
// Resistive touch only reports one point, so only one button (or one D-pad diagonal) at a time.
use crate::pac;
use embedded_hal::digital::{InputPin, OutputPin};

// KEYINPUT bits, a bit is 0 while the key is held
pub const KEY_A: u16 = 1 << 0;
pub const KEY_B: u16 = 1 << 1;
pub const KEY_RIGHT: u16 = 1 << 4;
pub const KEY_LEFT: u16 = 1 << 5;
pub const KEY_UP: u16 = 1 << 6;
pub const KEY_DOWN: u16 = 1 << 7;
pub const KEYS_RELEASED: u16 = 0x03FF;

#[derive(Copy, Clone)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self { x, y, width, height }
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub const fn inset(&self, by: u16) -> Rect {
        Rect::new(self.x + by, self.y + by, self.width - 2 * by, self.height - 2 * by)
    }
}

// The area below the GBA screen
pub const PANEL: Rect = Rect::new(0, 160, 240, 160);

// The D-pad is a 3x3 grid, its corners press two directions at once
const DPAD_CELL: u16 = 44;
const DPAD: Rect = Rect::new(6, 174, DPAD_CELL * 3, DPAD_CELL * 3);
const fn dpad_cell(column: u16, row: u16) -> Rect {
    Rect::new(DPAD.x + column * DPAD_CELL, DPAD.y + row * DPAD_CELL, DPAD_CELL, DPAD_CELL)
}
pub const DPAD_CENTRE: Rect = dpad_cell(1, 1);

// buttons drawn on the panel and the key each one shows
pub const BUTTONS: [(Rect, u16); 6] = [
    (dpad_cell(1, 0), KEY_UP),
    (dpad_cell(1, 2), KEY_DOWN),
    (dpad_cell(0, 1), KEY_LEFT),
    (dpad_cell(2, 1), KEY_RIGHT),
    (Rect::new(192, 196, 46, 46), KEY_A),
    (Rect::new(146, 246, 46, 46), KEY_B),
];

// 5x7 pixel letters for the button labels, one byte per row with the leftmost pixel in bit 4
pub const GLYPH_WIDTH: u16 = 5;
pub const GLYPH_HEIGHT: u16 = 7;
pub type Glyph = [u8; GLYPH_HEIGHT as usize];

const GLYPH_A: Glyph = [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001];
const GLYPH_B: Glyph = [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110];

// The letter drawn on a button, if it has one
pub fn label(key: u16) -> Option<&'static Glyph> {
    match key {
        KEY_A => Some(&GLYPH_A),
        KEY_B => Some(&GLYPH_B),
        _ => None,
    }
}

// The keys held by a touch at this screen position, as a mask of KEY_ bits
pub fn keys_at(x: u16, y: u16) -> u16 {
    if DPAD.contains(x, y) {
        let column = (x - DPAD.x) / DPAD_CELL;
        let row = (y - DPAD.y) / DPAD_CELL;
        let mut keys = 0;
        match column {
            0 => keys |= KEY_LEFT,
            2 => keys |= KEY_RIGHT,
            _ => {}
        }
        match row {
            0 => keys |= KEY_UP,
            2 => keys |= KEY_DOWN,
            _ => {}
        }
        return keys;
    }
    for (rect, key) in &BUTTONS[4..] {
        if rect.contains(x, y) {
            return *key;
        }
    }
    0
}

// Raw touch readings at the edges of the display, measured by touching its corners. Both axes run
// the opposite way to the screen's.
const RAW_X_LEFT: i32 = 3528;
const RAW_X_RIGHT: i32 = 585;
const RAW_Y_TOP: i32 = 3556;
const RAW_Y_BOTTOM: i32 = 622;

// Convert a raw touch reading to a screen position
pub fn raw_to_screen(raw_x: u16, raw_y: u16) -> (u16, u16) {
    let scale = |raw: u16, low: i32, high: i32, size: i32| {
        ((raw as i32 - low) * size / (high - low)).clamp(0, size - 1) as u16
    };
    (
        scale(raw_x, RAW_X_LEFT, RAW_X_RIGHT, 240),
        scale(raw_y, RAW_Y_TOP, RAW_Y_BOTTOM, 320),
    )
}

// XPT2046 commands: start bit, channel, 12 bit differential conversion, powered down between
// conversions so the pen interrupt stays enabled
const READ_X: u8 = 0b1101_0000;
const READ_Y: u8 = 0b1001_0000;
const SAMPLES: usize = 5;

// SPI1 runs at clk_peri / (2 * (1 + SCR))
const DISPLAY_SCR: u8 = 0; // 62.5 MHz
const TOUCH_SCR: u8 = 31; // 1.95 MHz, the XPT2046 allows up to 2.5 MHz

fn spi1_configure(scr: u8, cpol: bool, cpha: bool) {
    let spi = unsafe { &*pac::SPI1::PTR };
    spi.sspcr1().modify(|_, w| w.sse().clear_bit());
    spi.sspcr0()
        .modify(|_, w| unsafe { w.scr().bits(scr).spo().bit(cpol).sph().bit(cpha) });
    spi.sspcr1().modify(|_, w| w.sse().set_bit());
}

fn spi1_transfer(byte: u8) -> u8 {
    let spi = unsafe { &*pac::SPI1::PTR };
    while spi.sspsr().read().tnf().bit_is_clear() {}
    spi.sspdr().write(|w| unsafe { w.data().bits(byte as u16) });
    while spi.sspsr().read().rne().bit_is_clear() {}
    spi.sspdr().read().data().bits() as u8
}

fn read_channel(command: u8) -> u16 {
    spi1_transfer(command);
    let high = spi1_transfer(0) as u16;
    let low = spi1_transfer(0) as u16;
    ((high << 8) | low) >> 3
}

fn median(mut values: [u16; SAMPLES]) -> u16 {
    values.sort_unstable();
    values[SAMPLES / 2]
}

pub struct Touch<TpCs, TpIrq, LcdCs> {
    pub tp_cs: TpCs,
    pub tp_irq: TpIrq,
    pub lcd_cs: LcdCs,
}

impl<TpCs: OutputPin, TpIrq: InputPin, LcdCs: OutputPin> Touch<TpCs, TpIrq, LcdCs> {
    // Read the raw touch position, or None when nothing is touching the screen.
    // SPI1 must be idle, and is left set up for the display again afterwards.
    pub fn read_raw(&mut self) -> Option<(u16, u16)> {
        if self.tp_irq.is_high().unwrap_or(true) {
            return None;
        }

        // the display would take these bytes as pixel data if it was still selected
        self.lcd_cs.set_high().ok();
        spi1_configure(TOUCH_SCR, false, false);
        self.tp_cs.set_low().ok();

        let mut xs = [0; SAMPLES];
        let mut ys = [0; SAMPLES];
        for i in 0..SAMPLES {
            xs[i] = read_channel(READ_X);
            ys[i] = read_channel(READ_Y);
        }

        self.tp_cs.set_high().ok();
        spi1_configure(DISPLAY_SCR, true, true);
        self.lcd_cs.set_low().ok();

        // readings taken while the pen was lifting are not reliable
        if self.tp_irq.is_high().unwrap_or(true) {
            return None;
        }
        Some((median(xs), median(ys)))
    }
}
