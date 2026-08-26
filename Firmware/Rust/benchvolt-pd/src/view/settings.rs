//! Settings screen: temperature unit, profile entry points, factory defaults.

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};

use benchvolt_pd::app::{AppState, ProfileStatus, TemperatureUnit};

use super::BenchVoltView;

fn item(state: &AppState, index: usize) -> &'static str {
    match index {
        0 if state.temperature_unit == TemperatureUnit::Celsius => "Temperature       C",
        0 => "Temperature       F",
        1 => "Save Profile",
        2 => "Load Profile",
        3 => "Factory Defaults",
        _ => "Back",
    }
}

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    let unit = item(state, 0);
    view.draw_menu(
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
    draw_status(view, state);
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if old.menu_selection != new.menu_selection {
        let old_index = usize::from(old.menu_selection);
        let new_index = usize::from(new.menu_selection);
        view.draw_menu_item(item(new, old_index), old_index, false);
        view.draw_menu_item(item(new, new_index), new_index, true);
    }
    if old.temperature_unit != new.temperature_unit {
        view.draw_menu_item(item(new, 0), 0, new.menu_selection == 0);
    }
    if old.profile_status != new.profile_status {
        draw_status(view, new);
    }
}

fn draw_status<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.fill_rect(112, 3, 208, 20, Rgb565::BLACK);
    let status = match state.profile_status {
        ProfileStatus::ConfirmDefaults => Some(("CLICK TO CONFIRM", Rgb565::YELLOW)),
        ProfileStatus::DefaultsLoaded => Some(("DEFAULTS LOADED / OUT OFF", Rgb565::GREEN)),
        ProfileStatus::Failed => Some(("FAILED", Rgb565::RED)),
        _ => None,
    };
    if let Some((status, color)) = status {
        view.text8(status, 112, 8, color);
    }
}
