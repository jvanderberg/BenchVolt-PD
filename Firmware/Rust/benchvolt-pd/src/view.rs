//! Rendering core: the shared drawing primitives and the per-screen routing.
//! Each screen lives in its own `view/<screen>.rs` module exposing the same
//! two entry points — `render(view, state)` for a full paint and
//! `transition(view, old, new)` for damage-tracked updates — mirroring the
//! `reducto::View` trait one level down.

mod awg;
mod channel;
mod help;
mod main_menu;
mod overview;
mod pd_source;
mod profiles;
mod settings;
mod system;
mod usb_pd;

use core::fmt::Write as _;

use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_8X13_BOLD},
        MonoTextStyle,
    },
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use heapless::String;
use reducto::View;

use benchvolt_pd::app::{AppState, Screen, TemperatureUnit};
use benchvolt_pd::view_projection::{seven_segment_mask, temperature_projection, TemperatureProjection};

/// Shared palette: the row-selection highlight and the divider/grid tone
/// used identically by every menu-style screen.
pub(crate) const SELECTION_FILL: Rgb565 = Rgb565::new(0, 18, 24);
pub(crate) const DIVIDER: Rgb565 = Rgb565::new(8, 16, 16);

pub struct BenchVoltView<D> {
    display: D,
}

impl<D> BenchVoltView<D>
where
    D: DrawTarget<Color = Rgb565>,
{
    pub fn new(display: D) -> Self {
        Self { display }
    }

    fn clear_screen(&mut self) {
        self.display.clear(Rgb565::BLACK).ok();
    }

    // Outlined drawing primitives: every screen funnels its text and fill
    // calls through these so each call site pays one plain call instead of
    // inline style-struct construction — a measurable flash saving at -Oz
    // across the ~70 call sites.
    #[inline(never)]
    fn text8(&mut self, text: &str, x: i32, y: i32, color: Rgb565) {
        Text::with_baseline(
            text,
            Point::new(x, y),
            MonoTextStyle::new(&FONT_8X13_BOLD, color),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    #[inline(never)]
    fn text20(&mut self, text: &str, x: i32, y: i32, color: Rgb565) {
        Text::with_baseline(
            text,
            Point::new(x, y),
            MonoTextStyle::new(&FONT_10X20, color),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    #[inline(never)]
    fn fill_rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: Rgb565) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(x, y), Size::new(width, height)),
                color,
            )
            .ok();
    }

    #[inline(never)]
    fn fill_circle(&mut self, x: i32, y: i32, diameter: u32, color: Rgb565) {
        Circle::new(Point::new(x, y), diameter)
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(&mut self.display)
            .ok();
    }

    #[inline(never)]
    fn stroke_rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: Rgb565) {
        Rectangle::new(Point::new(x, y), Size::new(width, height))
            .into_styled(PrimitiveStyle::with_stroke(color, 1))
            .draw(&mut self.display)
            .ok();
    }

    fn fill_capsule(&mut self, top_left: Point, width: u32, height: u32, color: Rgb565) {
        debug_assert!(width >= height);
        let radius = height / 2;
        let right = Point::new(top_left.x + (width - height) as i32, top_left.y);

        for origin in [top_left, right] {
            self.fill_circle(origin.x, origin.y, height, color);
        }
        self.fill_rect(
            top_left.x + radius as i32,
            top_left.y,
            width - 2 * radius,
            height,
            color,
        );
    }

    fn draw_temperature(&mut self, state: &AppState) {
        let mut text: String<32> = String::new();
        match temperature_projection(state) {
            TemperatureProjection::Invalid => {
                text.push_str("T:--.-C").ok();
            }
            TemperatureProjection::Tenths(value, unit) => {
                let magnitude = value.abs();
                write!(
                    &mut text,
                    "T:{}{}.{:01}{}",
                    if value < 0 { "-" } else { "" },
                    magnitude / 10,
                    magnitude % 10,
                    if unit == TemperatureUnit::Celsius {
                        'C'
                    } else {
                        'F'
                    }
                )
                .ok();
            }
        }

        self.fill_rect(226, 2, 94, 20, Rgb565::BLACK);
        self.text20(text.as_str(), 226, 1, Rgb565::WHITE);
    }

    fn clear_detail_region(&mut self, x: i32, y: i32, width: u32, height: u32) {
        self.fill_rect(x, y, width, height, Rgb565::BLACK);
    }

    #[inline(never)]
    fn draw_segment_digit(
        &mut self,
        digit: char,
        origin: Point,
        color: Rgb565,
        rectangles: &[(i32, i32, u32, u32); 7],
    ) {
        let Some(segments) = seven_segment_mask(digit) else {
            return;
        };
        for (index, &(x, y, width, height)) in rectangles.iter().enumerate() {
            if segments & (1 << index) != 0 {
                self.fill_rect(origin.x + x, origin.y + y, width, height, color);
            }
        }
    }

    fn draw_hero_digit(&mut self, digit: char, origin: Point, color: Rgb565) {
        const RECTANGLES: [(i32, i32, u32, u32); 7] = [
            (4, 0, 14, 4),
            (0, 4, 4, 13),
            (18, 4, 4, 13),
            (4, 17, 14, 4),
            (0, 21, 4, 13),
            (18, 21, 4, 13),
            (4, 34, 14, 4),
        ];
        self.draw_segment_digit(digit, origin, color, &RECTANGLES);
    }

    fn draw_hero(&mut self, value: Option<u32>, x: i32, suffix: &str) {
        let mut text: String<32> = String::new();
        match value {
            Some(value) => write!(&mut text, "{}.{:02}", value / 100, value % 100).ok(),
            None => text.push_str("--.--").ok(),
        };
        self.clear_detail_region(x - 7, 29, 151, 42);

        let mut cursor = x;
        for character in text.chars() {
            if character == '.' {
                self.fill_circle(cursor + 1, 34 + 29, 5, Rgb565::WHITE);
                cursor += 7;
            } else {
                self.draw_hero_digit(character, Point::new(cursor, 31), Rgb565::WHITE);
                cursor += 25;
            }
        }
        self.text20(suffix, cursor + 2, 48, Rgb565::WHITE);
    }

    fn draw_power_digit(&mut self, digit: char, origin: Point) {
        const RECTANGLES: [(i32, i32, u32, u32); 7] = [
            (3, 0, 9, 3),
            (0, 3, 3, 9),
            (12, 3, 3, 9),
            (3, 12, 9, 3),
            (0, 15, 3, 9),
            (12, 15, 3, 9),
            (3, 24, 9, 3),
        ];
        self.draw_segment_digit(digit, origin, Rgb565::WHITE, &RECTANGLES);
    }

    fn draw_power(&mut self, power_centiwatts: Option<u32>) {
        let mut text: String<32> = String::new();
        match power_centiwatts {
            Some(value) => write!(&mut text, "{}.{:02} W", value / 100, value % 100).ok(),
            None => text.push_str("--.-- W").ok(),
        };
        // Center dynamically: short values like "0.53 W" would sit visibly
        // left of center if drawn from a fixed start column.
        let width: i32 = text
            .chars()
            .map(|character| match character {
                '0'..='9' | '-' => 17,
                '.' => 6,
                'W' => 10,
                _ => 5,
            })
            .sum();
        self.clear_detail_region(85, 88, 150, 35);
        let mut cursor = 160 - width / 2;
        for character in text.chars() {
            match character {
                '0'..='9' | '-' => {
                    self.draw_power_digit(character, Point::new(cursor, 91));
                    cursor += 17;
                }
                '.' => {
                    self.fill_circle(cursor, 114, 4, Rgb565::WHITE);
                    cursor += 6;
                }
                'W' => {
                    self.text20("W", cursor, 95, Rgb565::WHITE);
                    cursor += 10;
                }
                _ => cursor += 5,
            }
        }
    }

    fn draw_detail_setting_frame(&mut self, x: i32, width: u32, focused: bool) {
        self.clear_detail_region(x, 126, width, 37);
        if focused {
            self.stroke_rect(x, 128, width, 31, Rgb565::CYAN);
        }
    }

    fn draw_detail_setting_value(
        &mut self,
        value: u16,
        x: i32,
        width: u32,
        suffix: &str,
        focused: bool,
    ) {
        let mut text: String<32> = String::new();
        write!(&mut text, "{}.{:02}{}", value / 100, value % 100, suffix).ok();
        // Preserve the focus frame; value edits repaint only its interior.
        self.clear_detail_region(x + 2, 130, width - 4, 27);
        self.text20(
            text.as_str(),
            x + 8,
            135,
            if focused { Rgb565::CYAN } else { Rgb565::WHITE },
        );
    }

    /// Voltage with no superfluous decimals: "12", "1.8", "12.34".
    fn write_trimmed_volts(title: &mut String<32>, centivolts: u16) {
        let whole = centivolts / 100;
        let fraction = centivolts % 100;
        if fraction == 0 {
            write!(title, "{whole}").ok();
        } else if fraction.is_multiple_of(10) {
            write!(title, "{whole}.{}", fraction / 10).ok();
        } else {
            write!(title, "{whole}.{fraction:02}").ok();
        }
    }

    fn draw_menu(&mut self, title: &str, items: &[&str], state: &AppState) {
        self.clear_screen();
        self.text20(title, 6, 3, Rgb565::WHITE);
        self.fill_rect(0, 27, 320, 1, DIVIDER);
        for (index, item) in items.iter().enumerate() {
            self.draw_menu_item(item, index, usize::from(state.menu_selection) == index);
        }
    }

    fn draw_menu_item(&mut self, item: &str, index: usize, selected: bool) {
        // 23 px pitch fits the six-row main menu on the 170 px panel.
        let y = 31 + index as i32 * 23;
        self.fill_rect(
            5,
            y - 2,
            310,
            22,
            if selected {
                SELECTION_FILL
            } else {
                Rgb565::BLACK
            },
        );
        self.text8(if selected { ">" } else { " " }, 10, y, Rgb565::CYAN);
        self.text20(item, 30, y, Rgb565::WHITE);
    }
}

impl<D> View for BenchVoltView<D>
where
    D: DrawTarget<Color = Rgb565>,
{
    type State = AppState;

    #[inline(never)]
    fn render(&mut self, state: &Self::State) {
        match state.screen {
            Screen::MainMenu => main_menu::render(self, state),
            Screen::Overview => overview::render(self, state),
            Screen::Channel(_) => channel::render(self, state),
            Screen::UsbPdInput => usb_pd::render(self, state),
            Screen::Awg => awg::render(self, state),
            Screen::Settings => settings::render(self, state),
            Screen::ProfileSave | Screen::ProfileLoad => profiles::render(self, state),
            Screen::PdSource => pd_source::render(self, state),
            Screen::System => system::render(self, state),
            Screen::Help => help::render(self, state),
        }
    }

    #[inline(never)]
    fn render_transition(&mut self, old: &Self::State, new: &Self::State) {
        if old.screen != new.screen {
            self.render(new);
            return;
        }
        match new.screen {
            Screen::MainMenu => main_menu::transition(self, old, new),
            Screen::Overview => overview::transition(self, old, new),
            Screen::Channel(_) => channel::transition(self, old, new),
            Screen::UsbPdInput => usb_pd::transition(self, old, new),
            Screen::Awg => awg::transition(self, old, new),
            Screen::Settings => settings::transition(self, old, new),
            Screen::ProfileSave | Screen::ProfileLoad => profiles::transition(self, old, new),
            Screen::PdSource => pd_source::transition(self, old, new),
            Screen::System => system::transition(self, old, new),
            Screen::Help => help::transition(self, old, new),
        }
    }
}
