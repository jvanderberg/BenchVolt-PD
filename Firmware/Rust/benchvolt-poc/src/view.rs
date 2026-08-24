use core::fmt::Write as _;

use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_6X10, FONT_8X13_BOLD},
        MonoTextStyle,
    },
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use heapless::String;
use reducto::View;

use benchvolt_poc::app::{
    AppState, AwgStatus, AwgWaveform, ChannelSnapshot, ControlFocus, Fault, OutputTransition,
    ProfileStatus, RegulationMode, Screen, TemperatureUnit,
};
use benchvolt_poc::ui_content::{
    HELP_MAX_SCROLL, HELP_TEXT, HELP_VISIBLE_LINES, MAIN_MENU_ITEMS,
};
use benchvolt_poc::view_projection::{
    awg_damage, framed_value_damage, seven_segment_mask, sink_projection, FramedValueDamage,
    SinkPdStatus, SinkProjection,
};

const TABLE_TOP: i32 = 24;
const HEADER_BOTTOM: i32 = 45;
const ROW_HEIGHT: i32 = 24;
const TABLE_BOTTOM: i32 = HEADER_BOTTOM + 5 * ROW_HEIGHT;
const COLUMN_EDGES: [i32; 7] = [0, 25, 78, 131, 184, 237, 319];
const COLUMN_TEXT_X: [i32; 6] = [9, 32, 85, 138, 191, 246];

#[derive(Clone, Copy, Eq, PartialEq)]
enum TemperatureProjection {
    Invalid,
    Tenths(i32, TemperatureUnit),
}

fn temperature_projection(state: &AppState) -> TemperatureProjection {
    if !state.temp_valid {
        return TemperatureProjection::Invalid;
    }
    let tenths_c = i32::from(state.temp_sixteenths_c) * 10 / 16;
    match state.temperature_unit {
        TemperatureUnit::Celsius => {
            TemperatureProjection::Tenths(tenths_c, TemperatureUnit::Celsius)
        }
        TemperatureUnit::Fahrenheit => {
            TemperatureProjection::Tenths(tenths_c * 9 / 5 + 320, TemperatureUnit::Fahrenheit)
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StatusProjection {
    Fault,
    On,
    Wait,
    Off,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ChannelProjection {
    setpoint_centivolts: u16,
    limit_centiamps: u16,
    measured_centivolts: Option<u16>,
    measured_centiamps: Option<u16>,
    status: StatusProjection,
    regulation_mode: RegulationMode,
    regulating_current: bool,
}

fn channel_projection(channel: &ChannelSnapshot) -> ChannelProjection {
    let status = match channel.fault {
        Fault::None if channel.transition != OutputTransition::Stable => StatusProjection::Wait,
        Fault::None if channel.physical_enabled => StatusProjection::On,
        Fault::None if channel.requested_enabled => StatusProjection::Wait,
        Fault::None => StatusProjection::Off,
        _ => StatusProjection::Fault,
    };
    ChannelProjection {
        setpoint_centivolts: channel.setpoint_mv / 10,
        limit_centiamps: channel.current_limit_ma / 10,
        measured_centivolts: channel
            .measurement
            .valid
            .then_some(channel.measurement.millivolts / 10),
        measured_centiamps: channel
            .measurement
            .valid
            .then_some(channel.measurement.milliamps / 10),
        status,
        regulation_mode: channel.regulation_mode,
        regulating_current: channel.regulation_mode == RegulationMode::Cc
            && channel.physical_enabled
            && channel.drive_mv < channel.setpoint_mv,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct DetailProjection {
    voltage_centivolts: Option<u16>,
    current_centiamps: Option<u16>,
    power_centiwatts: Option<u32>,
    setpoint_centivolts: u16,
    limit_centiamps: u16,
    status: StatusProjection,
    focus: ControlFocus,
    regulation_mode: RegulationMode,
    regulating_current: bool,
}

fn detail_projection(channel: &ChannelSnapshot, focus: ControlFocus) -> DetailProjection {
    let row = channel_projection(channel);
    DetailProjection {
        voltage_centivolts: row.measured_centivolts,
        current_centiamps: row.measured_centiamps,
        power_centiwatts: channel.measurement.valid.then_some(
            u32::from(channel.measurement.millivolts) * u32::from(channel.measurement.milliamps)
                / 10_000,
        ),
        setpoint_centivolts: row.setpoint_centivolts,
        limit_centiamps: row.limit_centiamps,
        status: row.status,
        focus,
        regulation_mode: channel.regulation_mode,
        regulating_current: row.regulating_current,
    }
}

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

    fn fill_capsule(&mut self, top_left: Point, width: u32, height: u32, color: Rgb565) {
        debug_assert!(width >= height);
        let radius = height / 2;
        let right = Point::new(top_left.x + (width - height) as i32, top_left.y);

        for origin in [top_left, right] {
            Circle::new(origin, height)
                .into_styled(PrimitiveStyle::with_fill(color))
                .draw(&mut self.display)
                .ok();
        }
        self.display
            .fill_solid(
                &Rectangle::new(
                    Point::new(top_left.x + radius as i32, top_left.y),
                    Size::new(width - 2 * radius, height),
                ),
                color,
            )
            .ok();
    }

    fn draw_temperature(&mut self, state: &AppState) {
        let mut text: String<32> = String::new();
        match temperature_projection(state) {
            TemperatureProjection::Invalid => {
                text.push_str("T:--.-C").ok();
            }
            TemperatureProjection::Tenths(value, unit) if value < 0 => {
                let magnitude = value.abs();
                write!(
                    &mut text,
                    "T:-{}.{:01}{}",
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
            TemperatureProjection::Tenths(value, unit) => {
                write!(
                    &mut text,
                    "T:{}.{:01}{}",
                    value / 10,
                    value % 10,
                    if unit == TemperatureUnit::Celsius {
                        'C'
                    } else {
                        'F'
                    }
                )
                .ok();
            }
        }

        self.display
            .fill_solid(
                &Rectangle::new(Point::new(226, 2), Size::new(94, 20)),
                Rgb565::BLACK,
            )
            .ok();
        Text::with_baseline(
            text.as_str(),
            Point::new(226, 1),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn channel_row_top(index: usize) -> i32 {
        HEADER_BOTTOM + index as i32 * ROW_HEIGHT
    }

    // Clear only the interior. The table dividers are never erased by updates.
    fn clear_channel_cell(&mut self, column: usize, index: usize) {
        let left = COLUMN_EDGES[column];
        let right = COLUMN_EDGES[column + 1];
        let top = Self::channel_row_top(index);
        self.display
            .fill_solid(
                &Rectangle::new(
                    Point::new(left + 1, top + 1),
                    Size::new((right - left - 1) as u32, (ROW_HEIGHT - 1) as u32),
                ),
                Rgb565::BLACK,
            )
            .ok();
    }

    fn draw_channel_text(&mut self, text: &str, column: usize, index: usize, color: Rgb565) {
        Text::with_baseline(
            text,
            Point::new(COLUMN_TEXT_X[column], Self::channel_row_top(index) + 5),
            MonoTextStyle::new(&FONT_8X13_BOLD, color),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_channel_number(&mut self, index: usize) {
        let mut text: String<32> = String::new();
        write!(&mut text, "{}", index + 1).ok();
        self.clear_channel_cell(0, index);
        self.draw_channel_text(text.as_str(), 0, index, Rgb565::WHITE);
    }

    fn draw_fixed_value(&mut self, value: u16, column: usize, index: usize) {
        let mut text: String<32> = String::new();
        write!(&mut text, "{}.{:02}", value / 100, value % 100).ok();
        self.clear_channel_cell(column, index);
        self.draw_channel_text(text.as_str(), column, index, Rgb565::WHITE);
    }

    fn draw_measured_value(&mut self, value: Option<u16>, column: usize, index: usize) {
        match value {
            Some(value) => self.draw_fixed_value(value, column, index),
            None => {
                self.clear_channel_cell(column, index);
                self.draw_channel_text("--.--", column, index, Rgb565::WHITE);
            }
        }
    }

    fn draw_setpoint(&mut self, index: usize, projection: ChannelProjection) {
        self.draw_fixed_value(projection.setpoint_centivolts, 1, index);
    }

    fn draw_limit(&mut self, index: usize, projection: ChannelProjection) {
        self.draw_fixed_value(projection.limit_centiamps, 2, index);
    }

    fn draw_voltage(&mut self, index: usize, projection: ChannelProjection) {
        self.draw_measured_value(projection.measured_centivolts, 3, index);
    }

    fn draw_current(&mut self, index: usize, projection: ChannelProjection) {
        self.draw_measured_value(projection.measured_centiamps, 4, index);
    }

    fn draw_status(&mut self, index: usize, projection: ChannelProjection, focused: bool) {
        self.clear_channel_cell(5, index);
        let track_color = match projection.status {
            StatusProjection::On => Rgb565::new(4, 42, 10),
            StatusProjection::Wait => Rgb565::new(24, 38, 4),
            StatusProjection::Off => Rgb565::new(16, 16, 16),
            StatusProjection::Fault => Rgb565::RED,
        };
        let top = Self::channel_row_top(index);
        const TRACK_X: i32 = 248;
        const TRACK_WIDTH: i32 = 27;
        const KNOB_DIAMETER: i32 = 9;
        const KNOB_INSET: i32 = 2;
        self.fill_capsule(
            Point::new(TRACK_X, top + 6),
            TRACK_WIDTH as u32,
            13,
            track_color,
        );
        let knob_x = match projection.status {
            StatusProjection::On => TRACK_X + TRACK_WIDTH - KNOB_INSET - KNOB_DIAMETER,
            StatusProjection::Wait => TRACK_X + (TRACK_WIDTH - KNOB_DIAMETER) / 2,
            StatusProjection::Off | StatusProjection::Fault => TRACK_X + KNOB_INSET,
        };
        Circle::new(Point::new(knob_x, top + 8), KNOB_DIAMETER as u32)
            .into_styled(PrimitiveStyle::with_fill(if focused {
                Rgb565::CYAN
            } else {
                Rgb565::WHITE
            }))
            .draw(&mut self.display)
            .ok();
        if projection.regulation_mode == RegulationMode::Cc {
            Text::with_baseline(
                "CC",
                Point::new(298, Self::channel_row_top(index) + 5),
                MonoTextStyle::new(
                    &FONT_8X13_BOLD,
                    if projection.regulating_current {
                        Rgb565::GREEN
                    } else {
                        Rgb565::CYAN
                    },
                ),
                Baseline::Top,
            )
            .draw(&mut self.display)
            .ok();
        }
    }

    fn draw_channel(&mut self, index: usize, channel: &ChannelSnapshot, focused: bool) {
        let projection = channel_projection(channel);
        self.draw_channel_number(index);
        self.draw_setpoint(index, projection);
        self.draw_limit(index, projection);
        self.draw_voltage(index, projection);
        self.draw_current(index, projection);
        self.draw_status(index, projection, focused);
    }

    fn draw_channels(&mut self, state: &AppState) {
        for (index, channel) in state.channels.iter().enumerate() {
            self.draw_channel(
                index,
                channel,
                state.focus == ControlFocus::OverviewOutput(index as u8),
            );
        }
    }

    fn draw_table_grid(&mut self) {
        let color = Rgb565::new(8, 16, 16);
        for x in COLUMN_EDGES {
            self.display
                .fill_solid(
                    &Rectangle::new(
                        Point::new(x, TABLE_TOP),
                        Size::new(1, (TABLE_BOTTOM - TABLE_TOP + 1) as u32),
                    ),
                    color,
                )
                .ok();
        }
        for row in 0..=5 {
            let y = HEADER_BOTTOM + row * ROW_HEIGHT;
            self.display
                .fill_solid(&Rectangle::new(Point::new(0, y), Size::new(320, 1)), color)
                .ok();
        }
    }

    fn clear_detail_region(&mut self, x: i32, y: i32, width: u32, height: u32) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(x, y), Size::new(width, height)),
                Rgb565::BLACK,
            )
            .ok();
    }

    fn draw_hero_digit(&mut self, digit: char, origin: Point, color: Rgb565) {
        const RECTS: [(i32, i32, u32, u32); 7] = [
            (4, 0, 14, 4),
            (0, 4, 4, 13),
            (18, 4, 4, 13),
            (4, 17, 14, 4),
            (0, 21, 4, 13),
            (18, 21, 4, 13),
            (4, 34, 14, 4),
        ];

        let Some(segments) = seven_segment_mask(digit) else {
            return;
        };
        for (index, &(x, y, width, height)) in RECTS.iter().enumerate() {
            if segments & (1 << index) != 0 {
                Rectangle::new(origin + Point::new(x, y), Size::new(width, height))
                    .into_styled(PrimitiveStyle::with_fill(color))
                    .draw(&mut self.display)
                    .ok();
            }
        }
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
                Circle::new(Point::new(cursor + 1, 34 + 29), 5)
                    .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
                    .draw(&mut self.display)
                    .ok();
                cursor += 7;
            } else {
                self.draw_hero_digit(character, Point::new(cursor, 31), Rgb565::WHITE);
                cursor += 25;
            }
        }
        Text::with_baseline(
            suffix,
            Point::new(cursor + 2, 48),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_detail_voltage(&mut self, projection: DetailProjection) {
        self.draw_hero(projection.voltage_centivolts.map(u32::from), 5, "V");
    }

    fn draw_detail_current(&mut self, projection: DetailProjection) {
        self.draw_hero(projection.current_centiamps.map(u32::from), 164, "A");
    }

    fn draw_detail_power(&mut self, projection: DetailProjection) {
        self.draw_power(projection.power_centiwatts);
    }

    fn draw_power_digit(&mut self, digit: char, origin: Point) {
        const RECTS: [(i32, i32, u32, u32); 7] = [
            (3, 0, 9, 3),
            (0, 3, 3, 9),
            (12, 3, 3, 9),
            (3, 12, 9, 3),
            (0, 15, 3, 9),
            (12, 15, 3, 9),
            (3, 24, 9, 3),
        ];

        let Some(segments) = seven_segment_mask(digit) else {
            return;
        };
        for (index, &(x, y, width, height)) in RECTS.iter().enumerate() {
            if segments & (1 << index) != 0 {
                Rectangle::new(origin + Point::new(x, y), Size::new(width, height))
                    .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
                    .draw(&mut self.display)
                    .ok();
            }
        }
    }

    fn draw_power(&mut self, power_centiwatts: Option<u32>) {
        let mut text: String<32> = String::new();
        match power_centiwatts {
            Some(value) => write!(&mut text, "{}.{:02} W", value / 100, value % 100).ok(),
            None => text.push_str("--.-- W").ok(),
        };
        self.clear_detail_region(90, 88, 150, 35);
        let mut cursor = 100;
        for character in text.chars() {
            match character {
                '0'..='9' | '-' => {
                    self.draw_power_digit(character, Point::new(cursor, 91));
                    cursor += 17;
                }
                '.' => {
                    Circle::new(Point::new(cursor, 114), 4)
                        .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
                        .draw(&mut self.display)
                        .ok();
                    cursor += 6;
                }
                'W' => {
                    Text::with_baseline(
                        "W",
                        Point::new(cursor, 95),
                        MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
                        Baseline::Top,
                    )
                    .draw(&mut self.display)
                    .ok();
                    cursor += 10;
                }
                _ => cursor += 5,
            }
        }
    }

    fn draw_detail_setting_frame(&mut self, x: i32, width: u32, focused: bool) {
        self.clear_detail_region(x, 126, width, 37);
        if focused {
            Rectangle::new(Point::new(x, 128), Size::new(width, 31))
                .into_styled(PrimitiveStyle::with_stroke(Rgb565::CYAN, 1))
                .draw(&mut self.display)
                .ok();
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
        Text::with_baseline(
            text.as_str(),
            Point::new(x + 8, 135),
            MonoTextStyle::new(
                &FONT_10X20,
                if focused { Rgb565::CYAN } else { Rgb565::WHITE },
            ),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_detail_setpoint(&mut self, projection: DetailProjection) {
        let focused = projection.focus == ControlFocus::Voltage;
        self.draw_detail_setting_frame(4, 84, focused);
        self.draw_detail_setting_value(projection.setpoint_centivolts, 4, 84, "V", focused);
    }

    fn draw_detail_setpoint_value(&mut self, projection: DetailProjection) {
        self.draw_detail_setting_value(
            projection.setpoint_centivolts,
            4,
            84,
            "V",
            projection.focus == ControlFocus::Voltage,
        );
    }

    fn draw_detail_limit(&mut self, projection: DetailProjection) {
        let focused = projection.focus == ControlFocus::CurrentLimit;
        self.draw_detail_setting_frame(144, 84, focused);
        self.draw_detail_setting_value(projection.limit_centiamps, 144, 84, "A", focused);
    }

    fn draw_detail_limit_value(&mut self, projection: DetailProjection) {
        self.draw_detail_setting_value(
            projection.limit_centiamps,
            144,
            84,
            "A",
            projection.focus == ControlFocus::CurrentLimit,
        );
    }

    fn draw_detail_status(&mut self, status: StatusProjection, focused: bool) {
        self.clear_detail_region(238, 125, 82, 39);
        let track_color = match status {
            StatusProjection::On => Rgb565::new(4, 42, 10),
            StatusProjection::Wait => Rgb565::new(24, 38, 4),
            StatusProjection::Off | StatusProjection::Fault => Rgb565::new(24, 5, 5),
        };
        self.fill_capsule(Point::new(249, 130), 58, 28, track_color);
        let knob_x = if matches!(status, StatusProjection::On) {
            282
        } else {
            252
        };
        Circle::new(Point::new(knob_x, 132), 22)
            .into_styled(PrimitiveStyle::with_fill(if focused {
                Rgb565::CYAN
            } else {
                Rgb565::WHITE
            }))
            .draw(&mut self.display)
            .ok();
    }

    fn draw_detail_mode(&mut self, projection: DetailProjection) {
        let focused = projection.focus == ControlFocus::RegulationMode;
        self.clear_detail_region(94, 126, 44, 37);
        if focused {
            Rectangle::new(Point::new(94, 128), Size::new(44, 31))
                .into_styled(PrimitiveStyle::with_stroke(Rgb565::CYAN, 1))
                .draw(&mut self.display)
                .ok();
        }
        Text::with_baseline(
            match projection.regulation_mode {
                RegulationMode::Cv => "CV",
                RegulationMode::Cc => "CC",
            },
            Point::new(107, 135),
            MonoTextStyle::new(
                &FONT_10X20,
                if projection.regulating_current {
                    Rgb565::GREEN
                } else if focused {
                    Rgb565::CYAN
                } else {
                    Rgb565::WHITE
                },
            ),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_detail_screen(&mut self, state: &AppState, index: usize) {
        let channel = &state.channels[index];
        let projection = detail_projection(channel, state.focus);
        self.display.clear(Rgb565::BLACK).ok();

        let mut title: String<32> = String::new();
        write!(&mut title, "Channel {}", index + 1).ok();
        Text::with_baseline(
            title.as_str(),
            Point::new(4, 1),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.draw_temperature(state);
        if index >= 3 {
            self.draw_detail_mode(projection);
        }
        self.draw_detail_voltage(projection);
        self.draw_detail_current(projection);
        self.draw_detail_power(projection);
        self.draw_detail_setpoint(projection);
        self.draw_detail_limit(projection);
        self.draw_detail_status(projection.status, projection.focus == ControlFocus::Output);
    }

    fn draw_sink_voltage(&mut self, projection: SinkProjection) {
        self.draw_hero(projection.voltage_centivolts.map(u32::from), 5, "V");
    }

    fn draw_sink_current(&mut self, projection: SinkProjection) {
        self.draw_hero(projection.current_centiamps.map(u32::from), 164, "A");
    }

    fn draw_sink_power(&mut self, projection: SinkProjection) {
        self.draw_power(projection.power_centiwatts);
    }

    fn draw_sink_limit_frame(&mut self, projection: SinkProjection) {
        self.draw_detail_setting_frame(110, 102, projection.focused);
        self.draw_sink_limit_value(projection);
    }

    fn draw_sink_limit_value(&mut self, projection: SinkProjection) {
        self.clear_detail_region(112, 130, 98, 27);
        let mut text: String<32> = String::new();
        write!(
            &mut text,
            "{}.{:02}A",
            projection.limit_centiamps / 100,
            projection.limit_centiamps % 100
        )
        .ok();
        Text::with_baseline(
            text.as_str(),
            Point::new(118, 135),
            MonoTextStyle::new(
                &FONT_10X20,
                if projection.over_limit {
                    Rgb565::RED
                } else if projection.focused {
                    Rgb565::CYAN
                } else {
                    Rgb565::WHITE
                },
            ),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_sink_pd_status(&mut self, projection: SinkProjection) {
        self.clear_detail_region(214, 128, 106, 36);
        let mut text: String<32> = String::new();
        let color = match projection.pd_status {
            SinkPdStatus::Idle => {
                text.push_str("PD IDLE").ok();
                Rgb565::YELLOW
            }
            SinkPdStatus::Negotiating => {
                text.push_str("PD NEGOTIATING").ok();
                Rgb565::YELLOW
            }
            SinkPdStatus::Ready(contract) => {
                write!(
                    &mut text,
                    "PD{} {}.{}V {}.{}A",
                    contract.source_position,
                    contract.millivolts / 1_000,
                    contract.millivolts % 1_000 / 100,
                    contract.operating_milliamps / 1_000,
                    contract.operating_milliamps % 1_000 / 100,
                )
                .ok();
                Rgb565::GREEN
            }
            SinkPdStatus::Error(error) => {
                text.push_str("PD ERR:").ok();
                text.push_str(match error {
                    benchvolt_poc::pd::PdError::Bus => "BUS",
                    benchvolt_poc::pd::PdError::WrongDevice => "DEVICE",
                    benchvolt_poc::pd::PdError::Detached => "DETACHED",
                    benchvolt_poc::pd::PdError::Timeout => "TIMEOUT",
                    benchvolt_poc::pd::PdError::MalformedCapabilities => "CAPS",
                    benchvolt_poc::pd::PdError::NoSuitablePdo => "NO PDO",
                    benchvolt_poc::pd::PdError::ContractMismatch => "CONTRACT",
                })
                .ok();
                Rgb565::RED
            }
            SinkPdStatus::Fault(fault) => {
                text.push_str("INPUT:").ok();
                text.push_str(match fault {
                    Fault::None => "OK",
                    Fault::OverCurrent => "OVERCURRENT",
                    Fault::OverTemperature => "OVERTEMP",
                    Fault::Sensor => "SENSOR",
                    Fault::Hardware => "HARDWARE",
                })
                .ok();
                Rgb565::RED
            }
        };
        Text::with_baseline(
            text.as_str(),
            Point::new(216, 139),
            MonoTextStyle::new(&FONT_6X10, color),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_usb_pd_input(&mut self, state: &AppState) {
        let projection = sink_projection(state);
        self.display.clear(Rgb565::BLACK).ok();
        Text::with_baseline(
            "USB PD Input",
            Point::new(4, 1),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.draw_temperature(state);
        self.draw_sink_voltage(projection);
        self.draw_sink_current(projection);
        self.draw_sink_power(projection);
        self.draw_sink_limit_frame(projection);
        self.draw_sink_pd_status(projection);
    }

    fn draw_recovery_status(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(112, 2), Size::new(108, 20)),
                Rgb565::BLACK,
            )
            .ok();
        Text::with_baseline(
            if state.recovery_armed {
                "SAFE"
            } else {
                "RECOVERY!"
            },
            Point::new(112, 6),
            MonoTextStyle::new(
                &FONT_6X10,
                if state.recovery_armed {
                    Rgb565::GREEN
                } else {
                    Rgb565::RED
                },
            ),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_overview(&mut self, state: &AppState) {
        self.display.clear(Rgb565::BLACK).ok();
        Text::with_baseline(
            "BenchVolt",
            Point::new(4, 1),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.draw_recovery_status(state);
        self.draw_temperature(state);
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(0, TABLE_TOP), Size::new(320, 1)),
                Rgb565::WHITE,
            )
            .ok();
        for (column, label) in ["CH", "SET", "LIM", "VOLTS", "AMPS", "STATE"]
            .iter()
            .enumerate()
        {
            Text::with_baseline(
                label,
                Point::new(COLUMN_TEXT_X[column], 29),
                MonoTextStyle::new(&FONT_8X13_BOLD, Rgb565::CYAN),
                Baseline::Top,
            )
            .draw(&mut self.display)
            .ok();
        }
        self.draw_table_grid();
        self.draw_channels(state);
    }

    fn draw_menu(&mut self, title: &str, items: &[&str], state: &AppState) {
        self.display.clear(Rgb565::BLACK).ok();
        Text::with_baseline(
            title,
            Point::new(6, 3),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(0, 27), Size::new(320, 1)),
                Rgb565::new(8, 16, 16),
            )
            .ok();
        for (index, item) in items.iter().enumerate() {
            self.draw_menu_item(item, index, usize::from(state.menu_selection) == index);
        }
    }

    fn draw_menu_item(&mut self, item: &str, index: usize, selected: bool) {
        let y = 34 + index as i32 * 25;
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(5, y - 2), Size::new(310, 22)),
                Rgb565::BLACK,
            )
            .ok();
        if selected {
            Rectangle::new(Point::new(5, y - 2), Size::new(310, 22))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::new(0, 18, 24)))
                .draw(&mut self.display)
                .ok();
        }
        Text::with_baseline(
            if selected { ">" } else { " " },
            Point::new(10, y),
            MonoTextStyle::new(&FONT_8X13_BOLD, Rgb565::CYAN),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        Text::with_baseline(
            item,
            Point::new(30, y),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn main_menu_item(index: usize) -> &'static str {
        MAIN_MENU_ITEMS[index]
    }

    fn settings_item(state: &AppState, index: usize) -> &'static str {
        match index {
            0 if state.temperature_unit == TemperatureUnit::Celsius => "Temperature       C",
            0 => "Temperature       F",
            1 => "Save Profile",
            2 => "Load Profile",
            3 => "Factory Defaults",
            _ => "Back",
        }
    }

    fn profile_item(state: &AppState, index: usize) -> &'static str {
        match index {
            0 if state.profile_present[0] => "Slot 1          SAVED",
            0 => "Slot 1          EMPTY",
            1 if state.profile_present[1] => "Slot 2          SAVED",
            1 => "Slot 2          EMPTY",
            2 if state.profile_present[2] => "Slot 3          SAVED",
            2 => "Slot 3          EMPTY",
            _ => "Back",
        }
    }

    fn draw_main_menu(&mut self, state: &AppState) {
        self.draw_menu("BenchVolt", &MAIN_MENU_ITEMS, state);
    }

    fn draw_help(&mut self, state: &AppState) {
        self.display.clear(Rgb565::BLACK).ok();
        Text::with_baseline(
            "Help",
            Point::new(6, 3),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.draw_help_content(state);
    }

    fn draw_help_content(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(0, 28), Size::new(320, 142)),
                Rgb565::BLACK,
            )
            .ok();
        let start = usize::from(state.help_scroll);
        for (row, text) in HELP_TEXT
            .split('\n')
            .skip(start)
            .take(usize::from(HELP_VISIBLE_LINES))
            .enumerate()
        {
            let heading = matches!(
                text,
                "MAIN MENU" | "NAVIGATION" | "POWER SCREENS" | "CV / CC" | "AWG"
            );
            Text::with_baseline(
                text,
                Point::new(12, 32 + row as i32 * 16),
                MonoTextStyle::new(
                    &FONT_8X13_BOLD,
                    if heading { Rgb565::CYAN } else { Rgb565::WHITE },
                ),
                Baseline::Top,
            )
            .draw(&mut self.display)
            .ok();
        }
        let mut footer: String<32> = String::new();
        write!(
            &mut footer,
            "TURN scroll  CLICK back  {}-{}/{}",
            state.help_scroll + 1,
            (state.help_scroll + HELP_VISIBLE_LINES).min(HELP_MAX_SCROLL + HELP_VISIBLE_LINES),
            HELP_MAX_SCROLL + HELP_VISIBLE_LINES,
        )
        .ok();
        Text::with_baseline(
            footer.as_str(),
            Point::new(48, 151),
            MonoTextStyle::new(&FONT_6X10, Rgb565::GREEN),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_settings(&mut self, state: &AppState) {
        let unit = Self::settings_item(state, 0);
        self.draw_menu(
            "Settings",
            &[
                unit,
                "Save Profile",
                "Load Profile",
                "Factory Defaults",
                "Back",
            ],
            state,
        );
        self.draw_settings_status(state);
    }

    fn draw_settings_status(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(112, 3), Size::new(208, 20)),
                Rgb565::BLACK,
            )
            .ok();
        let status = match state.profile_status {
            ProfileStatus::ConfirmDefaults => Some(("CLICK TO CONFIRM", Rgb565::YELLOW)),
            ProfileStatus::DefaultsLoaded => Some(("DEFAULTS LOADED - OUTPUTS OFF", Rgb565::GREEN)),
            ProfileStatus::Failed => Some(("FAILED", Rgb565::RED)),
            _ => None,
        };
        if let Some((status, color)) = status {
            Text::with_baseline(
                status,
                Point::new(112, 8),
                MonoTextStyle::new(&FONT_6X10, color),
                Baseline::Top,
            )
            .draw(&mut self.display)
            .ok();
        }
    }

    fn draw_profiles(&mut self, state: &AppState, saving: bool) {
        let slots = [
            Self::profile_item(state, 0),
            Self::profile_item(state, 1),
            Self::profile_item(state, 2),
            Self::profile_item(state, 3),
        ];
        self.draw_menu(
            if saving {
                "Save Profile"
            } else {
                "Load Profile"
            },
            &slots,
            state,
        );
        self.draw_profile_status(state);
    }

    fn draw_profile_status(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(160, 3), Size::new(160, 20)),
                Rgb565::BLACK,
            )
            .ok();
        let status = match state.profile_status {
            ProfileStatus::ConfirmSave(_) | ProfileStatus::ConfirmLoad(_) => {
                Some(("CLICK TO CONFIRM", Rgb565::YELLOW))
            }
            ProfileStatus::Working => Some(("WORKING", Rgb565::CYAN)),
            ProfileStatus::Saved(_) => Some(("SAVED", Rgb565::GREEN)),
            ProfileStatus::Loaded(_) => Some(("LOADED - OUTPUTS OFF", Rgb565::GREEN)),
            ProfileStatus::Empty(_) => Some(("EMPTY SLOT", Rgb565::YELLOW)),
            ProfileStatus::Failed => Some(("FAILED", Rgb565::RED)),
            _ => None,
        };
        if let Some((status, color)) = status {
            Text::with_baseline(
                status,
                Point::new(160, 8),
                MonoTextStyle::new(&FONT_8X13_BOLD, color),
                Baseline::Top,
            )
            .draw(&mut self.display)
            .ok();
        }
    }

    fn draw_awg(&mut self, state: &AppState) {
        self.display.clear(Rgb565::BLACK).ok();
        Text::with_baseline(
            "AWG",
            Point::new(6, 3),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(0, 27), Size::new(320, 1)),
                Rgb565::new(8, 16, 16),
            )
            .ok();
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(198, 27), Size::new(1, 143)),
                Rgb565::new(8, 16, 16),
            )
            .ok();
        for index in 0..8 {
            self.draw_awg_row(state, index);
        }
        self.draw_awg_load_panel(state);
    }

    fn draw_awg_row(&mut self, state: &AppState, index: usize) {
        let y = 30 + index as i32 * 17;
        let selected = usize::from(state.menu_selection) == index;
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(4, y - 1), Size::new(190, 16)),
                if selected {
                    Rgb565::new(0, 18, 24)
                } else {
                    Rgb565::BLACK
                },
            )
            .ok();
        let label = [
            "Channel",
            "Waveform",
            "Frequency",
            "Duty",
            "Low",
            "High",
            "Output",
            "Back",
        ][index];
        Text::with_baseline(
            if selected { ">" } else { " " },
            Point::new(8, y),
            MonoTextStyle::new(&FONT_8X13_BOLD, Rgb565::CYAN),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        Text::with_baseline(
            label,
            Point::new(27, y),
            MonoTextStyle::new(&FONT_8X13_BOLD, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.draw_awg_value(state, index);
    }

    fn draw_awg_value(&mut self, state: &AppState, index: usize) {
        if index == 7 {
            return;
        }
        let y = 30 + index as i32 * 17;
        let selected = usize::from(state.menu_selection) == index;
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(102, y - 1), Size::new(92, 16)),
                if selected {
                    Rgb565::new(0, 18, 24)
                } else {
                    Rgb565::BLACK
                },
            )
            .ok();
        let mut value: String<32> = String::new();
        match index {
            0 => write!(&mut value, "CH{}", state.awg.channel + 1).ok(),
            1 => value
                .push_str(match state.awg.waveform {
                    AwgWaveform::Square => "SQUARE",
                    AwgWaveform::Triangle => "TRIANGLE",
                    AwgWaveform::Ramp => "RAMP",
                    AwgWaveform::Sine => "SINE",
                })
                .ok(),
            2 => write!(
                &mut value,
                "{}.{:01} Hz",
                state.awg.frequency_millihz / 1_000,
                state.awg.frequency_millihz % 1_000 / 100
            )
            .ok(),
            3 => {
                if state.awg.waveform == AwgWaveform::Square {
                    write!(&mut value, "{}%", state.awg.duty_percent).ok()
                } else {
                    value.push_str("--").ok()
                }
            }
            4 => write!(
                &mut value,
                "{}.{:02} V",
                state.awg.low_mv / 1_000,
                state.awg.low_mv % 1_000 / 10
            )
            .ok(),
            5 => write!(
                &mut value,
                "{}.{:02} V",
                state.awg.high_mv / 1_000,
                state.awg.high_mv % 1_000 / 10
            )
            .ok(),
            6 => value
                .push_str(match state.awg_status {
                    AwgStatus::Stopped => "START",
                    AwgStatus::StartRequested | AwgStatus::Starting => "STARTING",
                    AwgStatus::Running => "STOP",
                    AwgStatus::StopRequested => "STOPPING",
                    AwgStatus::Fault => "FAULT",
                })
                .ok(),
            _ => None,
        };
        let color = if index == 6 {
            match state.awg_status {
                AwgStatus::Running => Rgb565::GREEN,
                AwgStatus::Fault => Rgb565::RED,
                AwgStatus::StartRequested | AwgStatus::Starting | AwgStatus::StopRequested => {
                    Rgb565::YELLOW
                }
                AwgStatus::Stopped => Rgb565::CYAN,
            }
        } else if selected && state.awg_editing {
            Rgb565::CYAN
        } else {
            Rgb565::WHITE
        };
        Text::with_baseline(
            value.as_str(),
            Point::new(106, y),
            MonoTextStyle::new(&FONT_8X13_BOLD, color),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_awg_load_panel(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(202, 29), Size::new(118, 140)),
                Rgb565::BLACK,
            )
            .ok();
        self.draw_awg_load_heading(state);
        Text::with_baseline(
            "CURRENT RMS",
            Point::new(205, 52),
            MonoTextStyle::new(&FONT_6X10, Rgb565::new(18, 36, 24)),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        Text::with_baseline(
            "POWER AVG",
            Point::new(205, 101),
            MonoTextStyle::new(&FONT_6X10, Rgb565::new(18, 36, 24)),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        self.draw_awg_load_current(state);
        self.draw_awg_load_power(state);
    }

    fn draw_awg_load_heading(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(202, 29), Size::new(118, 18)),
                Rgb565::BLACK,
            )
            .ok();
        let channel = if state.awg_status == AwgStatus::Running {
            state.active_awg_channel() + 1
        } else {
            state.awg.channel + 1
        };
        let mut heading: String<32> = String::new();
        write!(&mut heading, "CH{} LOAD", channel).ok();
        Text::with_baseline(
            heading.as_str(),
            Point::new(205, 32),
            MonoTextStyle::new(&FONT_8X13_BOLD, Rgb565::CYAN),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_awg_load_current(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(202, 64), Size::new(118, 22)),
                Rgb565::BLACK,
            )
            .ok();
        let mut current: String<32> = String::new();
        if state.awg_load.valid {
            write!(
                &mut current,
                "{}.{:03} A",
                state.awg_load.milliamps_rms / 1_000,
                state.awg_load.milliamps_rms % 1_000
            )
            .ok();
        } else {
            current.push_str("-.--- A").ok();
        }
        Text::with_baseline(
            current.as_str(),
            Point::new(205, 65),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_awg_load_power(&mut self, state: &AppState) {
        self.display
            .fill_solid(
                &Rectangle::new(Point::new(202, 113), Size::new(118, 22)),
                Rgb565::BLACK,
            )
            .ok();
        let mut power: String<32> = String::new();
        if state.awg_load.valid {
            write!(
                &mut power,
                "{}.{:02} W",
                state.awg_load.milliwatts_average / 1_000,
                state.awg_load.milliwatts_average % 1_000 / 10
            )
            .ok();
        } else {
            power.push_str("--.-- W").ok();
        }
        Text::with_baseline(
            power.as_str(),
            Point::new(205, 114),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }

    fn draw_system(&mut self, state: &AppState) {
        self.draw_menu("System", &["Back"], state);
        Text::with_baseline(
            "BenchVolt Rust POC",
            Point::new(65, 72),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        Text::with_baseline(
            "USB: RUST-POC-01",
            Point::new(65, 98),
            MonoTextStyle::new(&FONT_8X13_BOLD, Rgb565::CYAN),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
        Text::with_baseline(
            if state.recovery_armed {
                "Recovery: SAFE"
            } else {
                "Recovery: NOT ARMED"
            },
            Point::new(65, 122),
            MonoTextStyle::new(
                &FONT_8X13_BOLD,
                if state.recovery_armed {
                    Rgb565::GREEN
                } else {
                    Rgb565::RED
                },
            ),
            Baseline::Top,
        )
        .draw(&mut self.display)
        .ok();
    }
}

impl<D> View for BenchVoltView<D>
where
    D: DrawTarget<Color = Rgb565>,
{
    type State = AppState;

    fn render(&mut self, state: &Self::State) {
        match state.screen {
            Screen::MainMenu => self.draw_main_menu(state),
            Screen::Overview => self.draw_overview(state),
            Screen::Channel(index) => self.draw_detail_screen(state, usize::from(index)),
            Screen::UsbPdInput => self.draw_usb_pd_input(state),
            Screen::Awg => self.draw_awg(state),
            Screen::Settings => self.draw_settings(state),
            Screen::ProfileSave => self.draw_profiles(state, true),
            Screen::ProfileLoad => self.draw_profiles(state, false),
            Screen::System => self.draw_system(state),
            Screen::Help => self.draw_help(state),
        }
    }

    fn render_transition(&mut self, old: &Self::State, new: &Self::State) {
        if old.screen != new.screen {
            self.render(new);
            return;
        }
        if old.recovery_armed != new.recovery_armed && new.screen == Screen::Overview {
            self.draw_recovery_status(new);
        }
        if temperature_projection(old) != temperature_projection(new)
            && matches!(
                new.screen,
                Screen::Overview | Screen::Channel(_) | Screen::UsbPdInput
            )
        {
            self.draw_temperature(new);
        }
        match new.screen {
            Screen::MainMenu => {
                if old.menu_selection != new.menu_selection {
                    let old_index = usize::from(old.menu_selection);
                    let new_index = usize::from(new.menu_selection);
                    self.draw_menu_item(Self::main_menu_item(old_index), old_index, false);
                    self.draw_menu_item(Self::main_menu_item(new_index), new_index, true);
                }
            }
            Screen::Awg => {
                let damage = awg_damage(old, new);
                for index in 0..8 {
                    if damage.rows & (1 << index) != 0 {
                        self.draw_awg_row(new, index);
                    } else if damage.values & (1 << index) != 0 {
                        self.draw_awg_value(new, index);
                    }
                }
                if damage.load_heading {
                    self.draw_awg_load_heading(new);
                }
                if damage.load_current {
                    self.draw_awg_load_current(new);
                }
                if damage.load_power {
                    self.draw_awg_load_power(new);
                }
            }
            Screen::Settings => {
                if old.menu_selection != new.menu_selection {
                    let old_index = usize::from(old.menu_selection);
                    let new_index = usize::from(new.menu_selection);
                    self.draw_menu_item(Self::settings_item(new, old_index), old_index, false);
                    self.draw_menu_item(Self::settings_item(new, new_index), new_index, true);
                }
                if old.temperature_unit != new.temperature_unit {
                    self.draw_menu_item(Self::settings_item(new, 0), 0, new.menu_selection == 0);
                }
                if old.profile_status != new.profile_status {
                    self.draw_settings_status(new);
                }
            }
            Screen::ProfileSave | Screen::ProfileLoad => {
                if old.menu_selection != new.menu_selection {
                    let old_index = usize::from(old.menu_selection);
                    let new_index = usize::from(new.menu_selection);
                    self.draw_menu_item(Self::profile_item(new, old_index), old_index, false);
                    self.draw_menu_item(Self::profile_item(new, new_index), new_index, true);
                }
                for index in 0..3 {
                    if old.profile_present[index] != new.profile_present[index] {
                        self.draw_menu_item(
                            Self::profile_item(new, index),
                            index,
                            usize::from(new.menu_selection) == index,
                        );
                    }
                }
                if old.profile_status != new.profile_status {
                    self.draw_profile_status(new);
                }
            }
            Screen::System => {
                if old.recovery_armed != new.recovery_armed {
                    self.draw_system(new);
                }
            }
            Screen::Help => {
                if old.help_scroll != new.help_scroll {
                    self.draw_help_content(new);
                }
            }
            Screen::Overview => {
                for index in 0..new.channels.len() {
                    let old_focused = old.focus == ControlFocus::OverviewOutput(index as u8);
                    let new_focused = new.focus == ControlFocus::OverviewOutput(index as u8);
                    let old = channel_projection(&old.channels[index]);
                    let new = channel_projection(&new.channels[index]);
                    if old.setpoint_centivolts != new.setpoint_centivolts {
                        self.draw_setpoint(index, new);
                    }
                    if old.limit_centiamps != new.limit_centiamps {
                        self.draw_limit(index, new);
                    }
                    if old.measured_centivolts != new.measured_centivolts {
                        self.draw_voltage(index, new);
                    }
                    if old.measured_centiamps != new.measured_centiamps {
                        self.draw_current(index, new);
                    }
                    if old.status != new.status
                        || old.regulation_mode != new.regulation_mode
                        || old.regulating_current != new.regulating_current
                        || old_focused != new_focused
                    {
                        self.draw_status(index, new, new_focused);
                    }
                }
            }
            Screen::Channel(index) => {
                let index = usize::from(index);
                let old = detail_projection(&old.channels[index], old.focus);
                let new = detail_projection(&new.channels[index], new.focus);
                if old.voltage_centivolts != new.voltage_centivolts {
                    self.draw_detail_voltage(new);
                }
                if old.current_centiamps != new.current_centiamps {
                    self.draw_detail_current(new);
                }
                if old.power_centiwatts != new.power_centiwatts {
                    self.draw_detail_power(new);
                }
                match framed_value_damage(
                    old.setpoint_centivolts,
                    new.setpoint_centivolts,
                    old.focus == ControlFocus::Voltage,
                    new.focus == ControlFocus::Voltage,
                ) {
                    FramedValueDamage::Frame => self.draw_detail_setpoint(new),
                    FramedValueDamage::Value => self.draw_detail_setpoint_value(new),
                    FramedValueDamage::None => {}
                }
                match framed_value_damage(
                    old.limit_centiamps,
                    new.limit_centiamps,
                    old.focus == ControlFocus::CurrentLimit,
                    new.focus == ControlFocus::CurrentLimit,
                ) {
                    FramedValueDamage::Frame => self.draw_detail_limit(new),
                    FramedValueDamage::Value => self.draw_detail_limit_value(new),
                    FramedValueDamage::None => {}
                }
                if old.status != new.status
                    || (old.focus == ControlFocus::Output) != (new.focus == ControlFocus::Output)
                {
                    self.draw_detail_status(new.status, new.focus == ControlFocus::Output);
                }
                if index >= 3
                    && (old.regulation_mode != new.regulation_mode
                        || old.regulating_current != new.regulating_current
                        || (old.focus == ControlFocus::RegulationMode)
                            != (new.focus == ControlFocus::RegulationMode))
                {
                    self.draw_detail_mode(new);
                }
            }
            Screen::UsbPdInput => {
                let old = sink_projection(old);
                let new = sink_projection(new);
                if old.voltage_centivolts != new.voltage_centivolts {
                    self.draw_sink_voltage(new);
                }
                if old.current_centiamps != new.current_centiamps {
                    self.draw_sink_current(new);
                }
                if old.power_centiwatts != new.power_centiwatts {
                    self.draw_sink_power(new);
                }
                if old.focused != new.focused {
                    self.draw_sink_limit_frame(new);
                } else if old.limit_centiamps != new.limit_centiamps
                    || old.over_limit != new.over_limit
                {
                    self.draw_sink_limit_value(new);
                }
                if old.pd_status != new.pd_status {
                    self.draw_sink_pd_status(new);
                }
            }
        }
    }
}
