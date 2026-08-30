#![no_std]
//! Minimal ST7789 banner for the v2 boot chain — enough raw-register SPI to
//! tell the user the updater is alive. Deliberately a SEPARATE crate so the
//! size-capped golden core never links it (cargo feature unification would
//! otherwise drag it into every workspace build): only the worker core and
//! the migrator depend on it.
//!
//! Wiring mirrors the application (startup.rs): SPI1 on PB3/PB5 (AF0, mode
//! 3), DC = PC10, RST = PC11, CS = PD2; 170x320 panel, landscape, inverted
//! colors, 35-pixel window offset.

use core::ptr::{read_volatile, write_volatile};

const RCC_AHBENR: *mut u32 = 0x4002_1014 as *mut u32;
const RCC_APB2ENR: *mut u32 = 0x4002_1018 as *mut u32;

const GPIOB: usize = 0x4800_0400;
const GPIOC: usize = 0x4800_0800;
const GPIOD: usize = 0x4800_0C00;
const MODER: usize = 0x00;
const OSPEEDR: usize = 0x08;
const BSRR: usize = 0x18;

const SPI1: usize = 0x4001_3000;
const SPI_CR1: *mut u32 = (SPI1 + 0x00) as *mut u32;
const SPI_CR2: *mut u32 = (SPI1 + 0x04) as *mut u32;
const SPI_SR: *const u32 = (SPI1 + 0x08) as *const u32;
const SPI_DR8: *mut u8 = (SPI1 + 0x0C) as *mut u8;

const WIDTH: u16 = 320;
const HEIGHT: u16 = 170;
const Y_OFFSET: u16 = 35;

const BG: u16 = 0x0000; // (inverted panel: renders as white? proven on bench)
const FG: u16 = 0xFFFF;

fn spin(count: u32) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}

fn pin(port: usize, offset: usize) -> *mut u32 {
    (port + offset) as *mut u32
}

fn set_output(port: usize, index: u32) {
    unsafe {
        let moder = pin(port, MODER);
        let value = read_volatile(moder) & !(0b11 << (index * 2)) | (0b01 << (index * 2));
        write_volatile(moder, value);
        let ospeedr = pin(port, OSPEEDR);
        write_volatile(ospeedr, read_volatile(ospeedr) | (0b11 << (index * 2)));
    }
}

fn write_pin(port: usize, index: u32, high: bool) {
    unsafe {
        write_volatile(pin(port, BSRR), if high { 1 << index } else { 1 << (index + 16) });
    }
}

fn spi_write(byte: u8) {
    unsafe {
        while read_volatile(SPI_SR) & (1 << 1) == 0 {} // TXE
        write_volatile(SPI_DR8, byte);
    }
}

fn spi_flush() {
    unsafe {
        while read_volatile(SPI_SR) & (1 << 7) != 0 {} // BSY
    }
}

fn command(cmd: u8) {
    spi_flush();
    write_pin(GPIOC, 10, false); // DC low = command
    spi_write(cmd);
    spi_flush();
    write_pin(GPIOC, 10, true);
}

fn data(bytes: &[u8]) {
    for byte in bytes {
        spi_write(*byte);
    }
}

fn window(x0: u16, y0: u16, x1: u16, y1: u16) {
    let (y0, y1) = (y0 + Y_OFFSET, y1 + Y_OFFSET);
    command(0x2A);
    data(&[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8]);
    command(0x2B);
    data(&[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8]);
    command(0x2C);
}

fn fill(x0: u16, y0: u16, x1: u16, y1: u16, color: u16) {
    window(x0, y0, x1, y1);
    let count = (x1 - x0 + 1) as u32 * (y1 - y0 + 1) as u32;
    let bytes = [(color >> 8) as u8, color as u8];
    for _ in 0..count {
        data(&bytes);
    }
    spi_flush();
}

/// 5x7 font, column-major, LSB = top row. Only the glyphs the banners use.
const GLYPHS: &[(u8, [u8; 5])] = &[
    (b'A', [0x7E, 0x09, 0x09, 0x09, 0x7E]),
    (b'B', [0x7F, 0x49, 0x49, 0x49, 0x36]),
    (b'C', [0x3E, 0x41, 0x41, 0x41, 0x22]),
    (b'D', [0x7F, 0x41, 0x41, 0x22, 0x1C]),
    (b'E', [0x7F, 0x49, 0x49, 0x49, 0x41]),
    (b'F', [0x7F, 0x09, 0x09, 0x09, 0x01]),
    (b'G', [0x3E, 0x41, 0x49, 0x49, 0x7A]),
    (b'H', [0x7F, 0x08, 0x08, 0x08, 0x7F]),
    (b'I', [0x00, 0x41, 0x7F, 0x41, 0x00]),
    (b'L', [0x7F, 0x40, 0x40, 0x40, 0x40]),
    (b'M', [0x7F, 0x02, 0x0C, 0x02, 0x7F]),
    (b'N', [0x7F, 0x04, 0x08, 0x10, 0x7F]),
    (b'O', [0x3E, 0x41, 0x41, 0x41, 0x3E]),
    (b'P', [0x7F, 0x09, 0x09, 0x09, 0x06]),
    (b'R', [0x7F, 0x09, 0x19, 0x29, 0x46]),
    (b'S', [0x46, 0x49, 0x49, 0x49, 0x31]),
    (b'T', [0x01, 0x01, 0x7F, 0x01, 0x01]),
    (b'U', [0x3F, 0x40, 0x40, 0x40, 0x3F]),
    (b'V', [0x1F, 0x20, 0x40, 0x20, 0x1F]),
    (b'W', [0x3F, 0x40, 0x38, 0x40, 0x3F]),
    (b'2', [0x42, 0x61, 0x51, 0x49, 0x46]),
    (b' ', [0x00, 0x00, 0x00, 0x00, 0x00]),
];

const SCALE: u16 = 3;
const GLYPH_W: u16 = 6 * SCALE; // 5 columns + 1 space

fn draw_text(text: &str, y: u16) {
    let width = text.len() as u16 * GLYPH_W;
    let mut x = if width < WIDTH { (WIDTH - width) / 2 } else { 0 };
    for ch in text.bytes() {
        let columns = GLYPHS
            .iter()
            .find(|(glyph, _)| *glyph == ch.to_ascii_uppercase())
            .map(|(_, columns)| *columns)
            .unwrap_or([0; 5]);
        for (col, bits) in columns.iter().enumerate() {
            for row in 0..7u16 {
                if bits & (1 << row) != 0 {
                    let px = x + col as u16 * SCALE;
                    let py = y + row * SCALE;
                    fill(px, py, px + SCALE - 1, py + SCALE - 1, FG);
                }
            }
        }
        x += GLYPH_W;
    }
}

/// Bring up SPI1 + the panel and show a two-line banner. Safe to call at
/// any core clock (SPI runs at sysclk/8; delays are sized for 48 MHz and
/// simply stretch at 8 MHz).
pub fn banner(line1: &str, line2: &str) {
    unsafe {
        write_volatile(
            RCC_AHBENR,
            read_volatile(RCC_AHBENR) | (1 << 18) | (1 << 19) | (1 << 20),
        );
        write_volatile(RCC_APB2ENR, read_volatile(RCC_APB2ENR) | (1 << 12));

        // PB3/PB5 to AF0 (SPI1 SCK/MOSI).
        let moder = pin(GPIOB, MODER);
        let mut value = read_volatile(moder);
        value = value & !(0b11 << 6) | (0b10 << 6);
        value = value & !(0b11 << 10) | (0b10 << 10);
        write_volatile(moder, value);
        let ospeedr = pin(GPIOB, OSPEEDR);
        write_volatile(ospeedr, read_volatile(ospeedr) | (0b11 << 6) | (0b11 << 10));

        set_output(GPIOC, 10); // DC
        set_output(GPIOC, 11); // RST
        set_output(GPIOD, 2); // CS

        // SPI1: master, mode 3, sysclk/8, software NSS; 8-bit frames.
        write_volatile(SPI_CR2, 0b0111 << 8 | (1 << 12)); // DS=8bit, FRXTH
        write_volatile(SPI_CR1, (1 << 2) | (1 << 1) | (1 << 0) | (0b010 << 3) | (1 << 9) | (1 << 8) | (1 << 6));
    }

    write_pin(GPIOD, 2, false); // CS low for the whole session
    write_pin(GPIOC, 10, true);

    // Panel reset + init (st7789.c sequence, minimum viable subset).
    write_pin(GPIOC, 11, false);
    spin(500_000);
    write_pin(GPIOC, 11, true);
    spin(1_000_000);
    command(0x01); // SWRESET
    spin(2_000_000);
    command(0x11); // SLPOUT
    spin(2_000_000);
    command(0x3A);
    data(&[0x55]); // 16-bit color
    command(0x36);
    data(&[0x60]); // MADCTL: landscape, matching the app (0xA0 was upside down)
    command(0x21); // INVON (panel is color-inverted, per the app)
    command(0x13); // NORON
    command(0x29); // DISPON
    spin(500_000);

    fill(0, 0, WIDTH - 1, HEIGHT - 1, BG);
    draw_text(line1, 50);
    draw_text(line2, 100);
}
