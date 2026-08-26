//! Channel detail screens: hero voltage/current digits, power row, setpoint
//! and limit editors, CV/CC mode, and the output switch.

use core::fmt::Write as _;

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};
use heapless::String;

use benchvolt_pd::app::{AppState, ControlFocus, RegulationMode, Screen};
use benchvolt_pd::view_projection::{
    centered_origin, detail_projection, framed_value_damage, temperature_projection,
    DetailProjection, FramedValueDamage, StatusProjection,
};

use super::BenchVoltView;

fn screen_index(state: &AppState) -> usize {
    match state.screen {
        Screen::Channel(index) => usize::from(index),
        _ => 0,
    }
}

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    let index = screen_index(state);
    let channel = &state.channels[index];
    let projection = detail_projection(channel, state.focus);
    view.clear_screen();

    draw_title(view, index, projection);
    view.draw_temperature(state);
    if index >= 3 {
        draw_mode(view, projection);
    }
    draw_voltage(view, projection);
    draw_current(view, projection);
    view.draw_power(projection.power_centiwatts);
    draw_setpoint(view, projection);
    draw_limit(view, projection);
    draw_status(view, projection.status, projection.focus == ControlFocus::Output);
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old_state: &AppState, new_state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if temperature_projection(old_state) != temperature_projection(new_state) {
        view.draw_temperature(new_state);
    }
    let index = screen_index(new_state);
    let old = detail_projection(&old_state.channels[index], old_state.focus);
    let new = detail_projection(&new_state.channels[index], new_state.focus);
    if old.voltage_centivolts != new.voltage_centivolts {
        draw_voltage(view, new);
    }
    if old.current_centiamps != new.current_centiamps {
        draw_current(view, new);
    }
    if old.power_centiwatts != new.power_centiwatts {
        view.draw_power(new.power_centiwatts);
    }
    match framed_value_damage(
        old.setpoint_centivolts,
        new.setpoint_centivolts,
        old.focus == ControlFocus::Voltage,
        new.focus == ControlFocus::Voltage,
    ) {
        FramedValueDamage::Frame => draw_setpoint(view, new),
        FramedValueDamage::Value => draw_setpoint_value(view, new),
        FramedValueDamage::None => {}
    }
    match framed_value_damage(
        old.limit_centiamps,
        new.limit_centiamps,
        old.focus == ControlFocus::CurrentLimit,
        new.focus == ControlFocus::CurrentLimit,
    ) {
        FramedValueDamage::Frame => draw_limit(view, new),
        FramedValueDamage::Value => draw_limit_value(view, new),
        FramedValueDamage::None => {}
    }
    if old.status != new.status
        || (old.focus == ControlFocus::Output) != (new.focus == ControlFocus::Output)
    {
        draw_status(view, new.status, new.focus == ControlFocus::Output);
    }
    if index >= 3
        && (old.regulation_mode != new.regulation_mode
            || old.regulating_current != new.regulating_current
            || (old.focus == ControlFocus::RegulationMode)
                != (new.focus == ControlFocus::RegulationMode))
    {
        draw_mode(view, new);
    }
}

// The V column plus the A column form a fixed 253 px ensemble (the A
// column's right edge is constant); x = 33 centers it on the 320 px
// panel with equal margins.
fn draw_voltage<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_hero(projection.voltage_centivolts.map(u32::from), 33, "V");
}

fn draw_current<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_hero(projection.current_centiamps.map(u32::from), 192, "A");
}

fn draw_setpoint<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    let focused = projection.focus == ControlFocus::Voltage;
    view.draw_detail_setting_frame(4, 84, focused);
    view.draw_detail_setting_value(projection.setpoint_centivolts, 4, 84, "V", focused);
}

fn draw_setpoint_value<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_detail_setting_value(
        projection.setpoint_centivolts,
        4,
        84,
        "V",
        projection.focus == ControlFocus::Voltage,
    );
}

fn draw_limit<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    let focused = projection.focus == ControlFocus::CurrentLimit;
    view.draw_detail_setting_frame(144, 84, focused);
    view.draw_detail_setting_value(projection.limit_centiamps, 144, 84, "A", focused);
}

fn draw_limit_value<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_detail_setting_value(
        projection.limit_centiamps,
        144,
        84,
        "A",
        projection.focus == ControlFocus::CurrentLimit,
    );
}

fn draw_status<D>(view: &mut BenchVoltView<D>, status: StatusProjection, focused: bool)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_detail_region(238, 125, 82, 39);
    let track_color = match status {
        StatusProjection::On => Rgb565::new(4, 42, 10),
        StatusProjection::Wait => Rgb565::new(24, 38, 4),
        StatusProjection::Fault => Rgb565::RED,
        StatusProjection::Off => Rgb565::new(24, 5, 5),
    };
    const TRACK_Y: i32 = 130;
    const TRACK_HEIGHT: u32 = 28;
    const KNOB_DIAMETER: u32 = 22;
    view.fill_capsule(Point::new(249, TRACK_Y), 58, TRACK_HEIGHT, track_color);
    let knob_x = if matches!(status, StatusProjection::On) {
        282
    } else {
        252
    };
    view.fill_circle(
        knob_x,
        centered_origin(TRACK_Y, TRACK_HEIGHT, KNOB_DIAMETER),
        KNOB_DIAMETER,
        if focused { Rgb565::CYAN } else { Rgb565::WHITE },
    );
}

fn draw_mode<D>(view: &mut BenchVoltView<D>, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    let focused = projection.focus == ControlFocus::RegulationMode;
    view.clear_detail_region(94, 126, 44, 37);
    if focused {
        view.stroke_rect(94, 128, 44, 31, Rgb565::CYAN);
    }
    view.text20(
        match projection.regulation_mode {
            RegulationMode::Cv => "CV",
            RegulationMode::Cc => "CC",
        },
        107,
        135,
        if projection.regulating_current {
            Rgb565::GREEN
        } else if focused {
            Rgb565::CYAN
        } else {
            Rgb565::WHITE
        },
    );
}

/// Header with the channel's nominal voltage (fixed channels) or its
/// adjustable range: "Channel 3 3.3V", "Channel 5 0.8-22V".
fn draw_title<D>(view: &mut BenchVoltView<D>, index: usize, projection: DetailProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_detail_region(0, 0, 224, 22);
    let mut title: String<32> = String::new();
    write!(&mut title, "Channel {} ", index + 1).ok();
    let range_mv = match index {
        3 => Some((
            benchvolt_pd::limits::CH4_MIN_VOLTAGE_MV,
            benchvolt_pd::limits::CH4_MAX_VOLTAGE_MV,
        )),
        4 => Some((
            benchvolt_pd::limits::CH5_MIN_VOLTAGE_MV,
            benchvolt_pd::limits::CH5_MAX_VOLTAGE_MV,
        )),
        _ => None,
    };
    if let Some((minimum, maximum)) = range_mv {
        BenchVoltView::<D>::write_trimmed_volts(&mut title, minimum / 10);
        title.push('-').ok();
        BenchVoltView::<D>::write_trimmed_volts(&mut title, maximum / 10);
    } else {
        BenchVoltView::<D>::write_trimmed_volts(&mut title, projection.setpoint_centivolts);
    }
    title.push('V').ok();
    view.text20(title.as_str(), 4, 1, Rgb565::WHITE);
}
