//! Help screen: scrollable usage text.

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};

use benchvolt_pd::app::AppState;
use benchvolt_pd::ui_content::{help_footer, is_help_heading, HELP_TEXT, HELP_VISIBLE_LINES};

use super::BenchVoltView;

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_screen();
    view.text20("Help", 6, 3, Rgb565::WHITE);
    draw_content(view, state);
}

pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if old.help_scroll != new.help_scroll {
        draw_content(view, new);
    }
}

fn draw_content<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.fill_rect(0, 28, 320, 142, Rgb565::BLACK);
    let start = usize::from(state.help_scroll);
    for (row, text) in HELP_TEXT
        .split('\n')
        .skip(start)
        .take(usize::from(HELP_VISIBLE_LINES))
        .enumerate()
    {
        let heading = is_help_heading(text);
        view.text8(
            text,
            12,
            32 + row as i32 * 16,
            if heading { Rgb565::CYAN } else { Rgb565::WHITE },
        );
    }
    let footer = help_footer(state.help_scroll);
    view.text8(footer.as_str(), 48, 151, Rgb565::GREEN);
}
