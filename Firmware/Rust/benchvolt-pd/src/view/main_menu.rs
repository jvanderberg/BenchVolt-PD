//! Main menu screen.

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};

use benchvolt_pd::app::AppState;
use benchvolt_pd::ui_content::MAIN_MENU_ITEMS;

use super::BenchVoltView;

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.draw_menu("BenchVolt PD", &MAIN_MENU_ITEMS, state);
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if old.menu_selection != new.menu_selection {
        let old_index = usize::from(old.menu_selection);
        let new_index = usize::from(new.menu_selection);
        view.draw_menu_item(MAIN_MENU_ITEMS[old_index], old_index, false);
        view.draw_menu_item(MAIN_MENU_ITEMS[new_index], new_index, true);
    }
}
