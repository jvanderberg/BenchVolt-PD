//! Profile save/load screens: three slots plus Back, with a status banner.

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};

use benchvolt_pd::app::{AppState, ProfileStatus, Screen};

use super::BenchVoltView;

fn item(state: &AppState, index: usize) -> &'static str {
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

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    let slots = [
        item(state, 0),
        item(state, 1),
        item(state, 2),
        item(state, 3),
    ];
    view.draw_menu(
        if state.screen == Screen::ProfileSave {
            "Save Profile"
        } else {
            "Load Profile"
        },
        &slots,
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
    for index in 0..3 {
        if old.profile_present[index] != new.profile_present[index] {
            view.draw_menu_item(
                item(new, index),
                index,
                usize::from(new.menu_selection) == index,
            );
        }
    }
    if old.profile_status != new.profile_status {
        draw_status(view, new);
    }
}

fn draw_status<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.fill_rect(160, 3, 160, 20, Rgb565::BLACK);
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
        view.text8(status, 160, 8, color);
    }
}
