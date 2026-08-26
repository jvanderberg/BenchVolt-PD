//! Overview screen: the five-channel table with compact output switches.

use core::fmt::Write as _;

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};
use heapless::String;

use benchvolt_pd::app::{AppState, ChannelSnapshot, ControlFocus, RegulationMode};
use benchvolt_pd::view_projection::{
    centered_origin, channel_projection, temperature_projection, ChannelProjection,
    StatusProjection,
};

use super::BenchVoltView;

const TABLE_TOP: i32 = 24;
const HEADER_BOTTOM: i32 = 45;
const ROW_HEIGHT: i32 = 24;
const TABLE_BOTTOM: i32 = HEADER_BOTTOM + 5 * ROW_HEIGHT;
const COLUMN_EDGES: [i32; 7] = [0, 25, 78, 131, 184, 237, 319];
const COLUMN_TEXT_X: [i32; 6] = [9, 32, 85, 138, 191, 246];

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_screen();
    view.text20("BenchVolt PD", 4, 1, Rgb565::WHITE);
    draw_recovery_status(view, state);
    view.draw_temperature(state);
    view.fill_rect(0, TABLE_TOP, 320, 1, Rgb565::WHITE);
    for (column, label) in ["CH", "SET", "LIM", "VOLTS", "AMPS", "STATE"]
        .iter()
        .enumerate()
    {
        view.text8(label, COLUMN_TEXT_X[column], 29, Rgb565::CYAN);
    }
    draw_table_grid(view);
    for (index, channel) in state.channels.iter().enumerate() {
        draw_channel(
            view,
            index,
            channel,
            state.focus == ControlFocus::OverviewOutput(index as u8),
        );
    }
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if old.recovery_armed != new.recovery_armed {
        draw_recovery_status(view, new);
    }
    if temperature_projection(old) != temperature_projection(new) {
        view.draw_temperature(new);
    }
    for index in 0..new.channels.len() {
        let old_focused = old.focus == ControlFocus::OverviewOutput(index as u8);
        let new_focused = new.focus == ControlFocus::OverviewOutput(index as u8);
        let old = channel_projection(&old.channels[index]);
        let new = channel_projection(&new.channels[index]);
        if old.setpoint_centivolts != new.setpoint_centivolts {
            draw_setpoint(view, index, new);
        }
        if old.limit_centiamps != new.limit_centiamps {
            draw_limit(view, index, new);
        }
        if old.measured_centivolts != new.measured_centivolts {
            draw_voltage(view, index, new);
        }
        if old.measured_centiamps != new.measured_centiamps {
            draw_current(view, index, new);
        }
        if old.status != new.status
            || old.regulation_mode != new.regulation_mode
            || old.regulating_current != new.regulating_current
            || old_focused != new_focused
        {
            draw_status(view, index, new, new_focused);
        }
    }
}

pub(super) fn draw_recovery_status<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.fill_rect(134, 2, 86, 20, Rgb565::BLACK);
    view.text8(
        if state.recovery_armed {
            "SAFE"
        } else {
            "RECOVERY!"
        },
        134,
        6,
        if state.recovery_armed {
            Rgb565::GREEN
        } else {
            Rgb565::RED
        },
    );
}

fn channel_row_top(index: usize) -> i32 {
    HEADER_BOTTOM + index as i32 * ROW_HEIGHT
}

// Clear only the interior. The table dividers are never erased by updates.
fn clear_channel_cell<D>(view: &mut BenchVoltView<D>, column: usize, index: usize)
where
    D: DrawTarget<Color = Rgb565>,
{
    let left = COLUMN_EDGES[column];
    let right = COLUMN_EDGES[column + 1];
    let top = channel_row_top(index);
    view.fill_rect(
        left + 1,
        top + 1,
        (right - left - 1) as u32,
        (ROW_HEIGHT - 1) as u32,
        Rgb565::BLACK,
    );
}

fn draw_channel_text<D>(
    view: &mut BenchVoltView<D>,
    text: &str,
    column: usize,
    index: usize,
    color: Rgb565,
) where
    D: DrawTarget<Color = Rgb565>,
{
    view.text8(text, COLUMN_TEXT_X[column], channel_row_top(index) + 5, color);
}

fn draw_channel_number<D>(view: &mut BenchVoltView<D>, index: usize)
where
    D: DrawTarget<Color = Rgb565>,
{
    let mut text: String<32> = String::new();
    write!(&mut text, "{}", index + 1).ok();
    clear_channel_cell(view, 0, index);
    draw_channel_text(view, text.as_str(), 0, index, Rgb565::WHITE);
}

fn draw_fixed_value<D>(view: &mut BenchVoltView<D>, value: u16, column: usize, index: usize)
where
    D: DrawTarget<Color = Rgb565>,
{
    let mut text: String<32> = String::new();
    write!(&mut text, "{}.{:02}", value / 100, value % 100).ok();
    clear_channel_cell(view, column, index);
    draw_channel_text(view, text.as_str(), column, index, Rgb565::WHITE);
}

fn draw_measured_value<D>(
    view: &mut BenchVoltView<D>,
    value: Option<u16>,
    column: usize,
    index: usize,
) where
    D: DrawTarget<Color = Rgb565>,
{
    match value {
        Some(value) => draw_fixed_value(view, value, column, index),
        None => {
            clear_channel_cell(view, column, index);
            draw_channel_text(view, "--.--", column, index, Rgb565::WHITE);
        }
    }
}

fn draw_setpoint<D>(view: &mut BenchVoltView<D>, index: usize, projection: ChannelProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    draw_fixed_value(view, projection.setpoint_centivolts, 1, index);
}

fn draw_limit<D>(view: &mut BenchVoltView<D>, index: usize, projection: ChannelProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    draw_fixed_value(view, projection.limit_centiamps, 2, index);
}

fn draw_voltage<D>(view: &mut BenchVoltView<D>, index: usize, projection: ChannelProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    draw_measured_value(view, projection.measured_centivolts, 3, index);
}

fn draw_current<D>(view: &mut BenchVoltView<D>, index: usize, projection: ChannelProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    draw_measured_value(view, projection.measured_centiamps, 4, index);
}

fn draw_status<D>(
    view: &mut BenchVoltView<D>,
    index: usize,
    projection: ChannelProjection,
    focused: bool,
) where
    D: DrawTarget<Color = Rgb565>,
{
    clear_channel_cell(view, 5, index);
    let track_color = match projection.status {
        StatusProjection::On => Rgb565::new(4, 42, 10),
        StatusProjection::Wait => Rgb565::new(24, 38, 4),
        StatusProjection::Off => Rgb565::new(16, 16, 16),
        StatusProjection::Fault => Rgb565::RED,
    };
    let top = channel_row_top(index);
    const TRACK_X: i32 = 248;
    const TRACK_WIDTH: i32 = 27;
    const TRACK_HEIGHT: u32 = 14;
    const KNOB_DIAMETER: i32 = 10;
    const KNOB_INSET: i32 = 2;
    let track_y = top + 5;
    view.fill_capsule(
        Point::new(TRACK_X, track_y),
        TRACK_WIDTH as u32,
        TRACK_HEIGHT,
        track_color,
    );
    let knob_x = match projection.status {
        StatusProjection::On => TRACK_X + TRACK_WIDTH - KNOB_INSET - KNOB_DIAMETER,
        StatusProjection::Wait => TRACK_X + (TRACK_WIDTH - KNOB_DIAMETER) / 2,
        StatusProjection::Off | StatusProjection::Fault => TRACK_X + KNOB_INSET,
    };
    view.fill_circle(
        knob_x,
        centered_origin(track_y, TRACK_HEIGHT, KNOB_DIAMETER as u32),
        KNOB_DIAMETER as u32,
        if focused { Rgb565::CYAN } else { Rgb565::WHITE },
    );
    if projection.regulation_mode == RegulationMode::Cc {
        view.text8(
            "CC",
            298,
            channel_row_top(index) + 5,
            if projection.regulating_current {
                Rgb565::GREEN
            } else {
                Rgb565::CYAN
            },
        );
    }
}

fn draw_channel<D>(
    view: &mut BenchVoltView<D>,
    index: usize,
    channel: &ChannelSnapshot,
    focused: bool,
) where
    D: DrawTarget<Color = Rgb565>,
{
    let projection = channel_projection(channel);
    draw_channel_number(view, index);
    draw_setpoint(view, index, projection);
    draw_limit(view, index, projection);
    draw_voltage(view, index, projection);
    draw_current(view, index, projection);
    draw_status(view, index, projection, focused);
}

fn draw_table_grid<D>(view: &mut BenchVoltView<D>)
where
    D: DrawTarget<Color = Rgb565>,
{
    let color = Rgb565::new(8, 16, 16);
    for x in COLUMN_EDGES {
        view.fill_rect(x, TABLE_TOP, 1, (TABLE_BOTTOM - TABLE_TOP + 1) as u32, color);
    }
    for row in 0..=5 {
        let y = HEADER_BOTTOM + row * ROW_HEIGHT;
        view.fill_rect(0, y, 320, 1, color);
    }
}
