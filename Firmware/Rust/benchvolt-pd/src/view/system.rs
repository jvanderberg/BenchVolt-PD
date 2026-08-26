//! System screen: firmware identity and recovery status.

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};

use benchvolt_pd::app::AppState;

use super::BenchVoltView;

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_menu("System", &["Back"], state);
    view.text20("BenchVolt PD", 65, 72, Rgb565::WHITE);
    view.text8(benchvolt_pd::FIRMWARE_BUILD, 65, 98, Rgb565::CYAN);
    view.text8(
        if state.recovery_armed {
            "Recovery: SAFE"
        } else {
            "Recovery: NOT ARMED"
        },
        65,
        122,
        if state.recovery_armed {
            Rgb565::GREEN
        } else {
            Rgb565::RED
        },
    );
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if old.recovery_armed != new.recovery_armed {
        render(view, new);
    }
}
