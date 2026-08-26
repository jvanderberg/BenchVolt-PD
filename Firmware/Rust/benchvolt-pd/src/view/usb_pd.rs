//! USB PD Input screen: measured sink values, the input current protection
//! limit editor, and the PD contract/error status.

use core::fmt::Write as _;

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};
use heapless::String;

use benchvolt_pd::app::{AppState, Fault};
use benchvolt_pd::view_projection::{
    pd_contract_label, sink_projection, temperature_projection, SinkPdStatus, SinkProjection,
};

use super::BenchVoltView;

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    let projection = sink_projection(state);
    view.clear_screen();
    draw_title(view, projection);
    view.draw_temperature(state);
    draw_voltage(view, projection);
    draw_current(view, projection);
    view.draw_power(projection.power_centiwatts);
    draw_limit_frame(view, projection);
    draw_pd_status(view, projection);
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old_state: &AppState, new_state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if temperature_projection(old_state) != temperature_projection(new_state) {
        view.draw_temperature(new_state);
    }
    let old = sink_projection(old_state);
    let new = sink_projection(new_state);
    if old.voltage_centivolts != new.voltage_centivolts {
        draw_voltage(view, new);
    }
    if old.current_centiamps != new.current_centiamps {
        draw_current(view, new);
    }
    if old.power_centiwatts != new.power_centiwatts {
        view.draw_power(new.power_centiwatts);
    }
    if old.focused != new.focused {
        draw_limit_frame(view, new);
    } else if old.limit_centiamps != new.limit_centiamps || old.over_limit != new.over_limit {
        draw_limit_value(view, new);
    }
    if old.pd_status != new.pd_status {
        draw_pd_status(view, new);
        draw_title(view, new);
    }
}

// Same centered ensemble as the channel detail screens.
fn draw_voltage<D>(view: &mut BenchVoltView<D>, projection: SinkProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_hero(projection.voltage_centivolts.map(u32::from), 33, "V");
}

fn draw_current<D>(view: &mut BenchVoltView<D>, projection: SinkProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_hero(projection.current_centiamps.map(u32::from), 192, "A");
}

fn draw_limit_frame<D>(view: &mut BenchVoltView<D>, projection: SinkProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_detail_setting_frame(110, 102, projection.focused);
    draw_limit_value(view, projection);
}

fn draw_limit_value<D>(view: &mut BenchVoltView<D>, projection: SinkProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_detail_region(112, 130, 98, 27);
    let mut text: String<32> = String::new();
    write!(
        &mut text,
        "{}.{:02}A",
        projection.limit_centiamps / 100,
        projection.limit_centiamps % 100
    )
    .ok();
    view.text20(
        text.as_str(),
        118,
        135,
        if projection.over_limit {
            Rgb565::RED
        } else if projection.focused {
            Rgb565::CYAN
        } else {
            Rgb565::WHITE
        },
    );
}

fn draw_pd_status<D>(view: &mut BenchVoltView<D>, projection: SinkProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_detail_region(214, 128, 106, 36);
    let mut text: String<32> = String::new();
    let color = match projection.pd_status {
        SinkPdStatus::Idle => {
            text.push_str("PD IDLE").ok();
            Rgb565::YELLOW
        }
        SinkPdStatus::Negotiating => {
            text.push_str("PD NEGOTIATE").ok();
            Rgb565::YELLOW
        }
        SinkPdStatus::Ready(contract) => {
            text.push_str(pd_contract_label(contract).as_str()).ok();
            Rgb565::GREEN
        }
        SinkPdStatus::Error(error) => {
            text.push_str("ERR ").ok();
            text.push_str(match error {
                benchvolt_pd::pd::PdError::Bus => "BUS",
                benchvolt_pd::pd::PdError::WrongDevice => "DEVICE",
                benchvolt_pd::pd::PdError::Detached => "DETACHED",
                benchvolt_pd::pd::PdError::Timeout => "TIMEOUT",
                benchvolt_pd::pd::PdError::MalformedCapabilities => "CAPS",
                benchvolt_pd::pd::PdError::NoSuitablePdo => "NO PDO",
                benchvolt_pd::pd::PdError::ContractMismatch => "CONTRACT",
            })
            .ok();
            Rgb565::RED
        }
        SinkPdStatus::Fault(fault) => {
            text.push_str("IN ").ok();
            text.push_str(match fault {
                Fault::None => "OK",
                Fault::OverCurrent => "OVERCURR",
                Fault::OverTemperature => "OVERTEMP",
                Fault::Sensor => "SENSOR",
                Fault::Hardware => "HARDWARE",
            })
            .ok();
            Rgb565::RED
        }
    };
    view.text8(text.as_str(), 216, 139, color);
}

/// Header with the negotiated nominal input voltage when a contract is
/// active, e.g. "USB PD Input 20V".
fn draw_title<D>(view: &mut BenchVoltView<D>, projection: SinkProjection)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_detail_region(0, 0, 224, 22);
    let mut title: String<32> = String::new();
    title.push_str("USB PD Input").ok();
    if let SinkPdStatus::Ready(contract) = projection.pd_status {
        write!(&mut title, " {}V", contract.millivolts / 1_000).ok();
    }
    view.text20(title.as_str(), 4, 1, Rgb565::WHITE);
}
