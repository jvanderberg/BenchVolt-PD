//! AWG screen: waveform configuration rows and the Start/Stop control.

use core::fmt::Write as _;

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};
use heapless::String;

use benchvolt_pd::app::{AppState, AwgStatus, AwgWaveform};
use benchvolt_pd::view_projection::awg_damage;

use super::BenchVoltView;

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_screen();
    view.text20("AWG", 6, 3, Rgb565::WHITE);
    view.fill_rect(0, 27, 320, 1, Rgb565::new(8, 16, 16));
    for index in 0..8 {
        draw_row(view, state, index);
    }
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    let damage = awg_damage(old, new);
    for index in 0..8 {
        if damage.rows & (1 << index) != 0 {
            draw_row(view, new, index);
        } else if damage.values & (1 << index) != 0 {
            draw_value(view, new, index);
        }
    }
}

fn draw_row<D>(view: &mut BenchVoltView<D>, state: &AppState, index: usize)
where
    D: DrawTarget<Color = Rgb565>,
{
    let y = 30 + index as i32 * 17;
    let selected = usize::from(state.menu_selection) == index;
    view.fill_rect(
        4,
        y - 1,
        190,
        16,
        if selected {
            Rgb565::new(0, 18, 24)
        } else {
            Rgb565::BLACK
        },
    );
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
    view.text8(if selected { ">" } else { " " }, 8, y, Rgb565::CYAN);
    view.text8(label, 27, y, Rgb565::WHITE);
    draw_value(view, state, index);
}

fn draw_value<D>(view: &mut BenchVoltView<D>, state: &AppState, index: usize)
where
    D: DrawTarget<Color = Rgb565>,
{
    if index == 7 {
        return;
    }
    let y = 30 + index as i32 * 17;
    let selected = usize::from(state.menu_selection) == index;
    view.fill_rect(
        102,
        y - 1,
        92,
        16,
        if selected {
            Rgb565::new(0, 18, 24)
        } else {
            Rgb565::BLACK
        },
    );
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
    view.text8(value.as_str(), 106, y, color);
}
