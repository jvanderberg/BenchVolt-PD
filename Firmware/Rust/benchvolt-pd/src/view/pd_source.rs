//! PD Source screen: source-advertised PDO rows with cursor/armed/active
//! markers, Apply/Cancel, and the requested-vs-actual apply banner.

use core::fmt::Write as _;

use embedded_graphics::{pixelcolor::Rgb565, prelude::*};
use heapless::String;

use benchvolt_pd::app::AppState;

use super::BenchVoltView;

pub(super) fn render<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.clear_screen();
    view.text20("PD Source", 6, 3, Rgb565::WHITE);
    view.fill_rect(0, 27, 320, 1, super::DIVIDER);
    draw_content(view, state);
}

/// True when the banner shows the outputs-off hint.
fn outputs_hint(state: &AppState) -> bool {
    (state.pd_source_stale || state.pd_source_armed.is_some()) && !state.outputs_inactive()
}

/// The paint queue holds 192 commands and a text row costs dozens, so this
/// screen must repaint per damaged row like the other menus — a whole-content
/// repaint per state change floods the queue (observed as a latched display
/// failure on hardware). Full redraws are reserved for list changes.
pub(super) fn transition<D>(view: &mut BenchVoltView<D>, old: &AppState, new: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    if old.pd_banner_mv != new.pd_banner_mv
        || old.pd_contract != new.pd_contract
        || old.pd_source_stale != new.pd_source_stale
        || old.pd_source_error != new.pd_source_error
        || outputs_hint(old) != outputs_hint(new)
    {
        draw_banner(view, new);
    }
    if old.pd_source_count != new.pd_source_count
        || old.pd_source_pdos != new.pd_source_pdos
        || old.pd_source_error != new.pd_source_error
    {
        draw_rows(view, new);
        return;
    }
    let rows = new.pd_source_rows();
    let mut damage: u16 = 0;
    let mut mark = |row: u8| {
        if row < rows {
            damage |= 1 << row;
        }
    };
    if old.menu_selection != new.menu_selection {
        mark(old.menu_selection);
        mark(new.menu_selection);
    }
    if old.pd_source_armed != new.pd_source_armed {
        for armed in [old.pd_source_armed, new.pd_source_armed].into_iter().flatten() {
            mark(armed);
        }
    }
    if old.pd_contract.map(|contract| contract.source_position)
        != new.pd_contract.map(|contract| contract.source_position)
    {
        // The ACTIVE marker moved; contract changes are rare, so redraw
        // every PDO row rather than tracking which two moved.
        for index in 0..new.pd_source_count {
            mark(index);
        }
    }
    if old.pd_source_apply_ready() != new.pd_source_apply_ready() {
        mark(new.pd_source_count);
    }
    for index in 0..usize::from(rows) {
        if damage & (1 << index) != 0 {
            draw_row(view, new, index);
        }
    }
}

fn draw_content<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    draw_banner(view, state);
    draw_rows(view, state);
}

fn draw_rows<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    // Clear the whole row region so a shrinking list leaves no stale rows.
    view.fill_rect(0, 29, 320, 141, Rgb565::BLACK);
    for index in 0..usize::from(state.pd_source_rows()) {
        draw_row(view, state, index);
    }
}

/// A blank row separates Apply/Cancel from the PDO list; a full seven-PDO
/// list needs that space back to stay on the 170 px panel.
fn row_y(state: &AppState, index: usize) -> i32 {
    let mut y = 30 + index as i32 * 15;
    if index >= usize::from(state.pd_source_count)
        && usize::from(state.pd_source_count) < benchvolt_pd::app::PD_SOURCE_MAX_PDOS
    {
        y += 15;
    }
    y
}

fn draw_row<D>(view: &mut BenchVoltView<D>, state: &AppState, index: usize)
where
    D: DrawTarget<Color = Rgb565>,
{
    let y = row_y(state, index);
    let selected = usize::from(state.menu_selection) == index;
    let count = usize::from(state.pd_source_count);
    view.fill_rect(
        4,
        y - 1,
        230,
        15,
        if selected {
            super::SELECTION_FILL
        } else {
            Rgb565::BLACK
        },
    );
    let mut text: String<32> = String::new();
    text.push_str(if selected { "> " } else { "  " }).ok();
    let color = if index < count {
        let pdo = state.pd_source_pdos[index];
        BenchVoltView::<D>::write_trimmed_volts(&mut text, pdo.millivolts / 10);
        write!(
            &mut text,
            "V {}.{}A {}W",
            pdo.milliamps / 1_000,
            pdo.milliamps % 1_000 / 100,
            u32::from(pdo.millivolts) * u32::from(pdo.milliamps) / 1_000_000
        )
        .ok();
        if state.pd_source_armed == Some(index as u8) {
            text.push_str("  ARMED").ok();
            Rgb565::CYAN
        } else if state.pd_contract.map(|contract| contract.source_position)
            == Some(pdo.source_position)
        {
            text.push_str("  ACTIVE").ok();
            Rgb565::GREEN
        } else {
            Rgb565::WHITE
        }
    } else if index == count {
        text.push_str("Apply").ok();
        if state.pd_source_apply_ready() {
            Rgb565::WHITE
        } else {
            Rgb565::new(12, 24, 12)
        }
    } else {
        text.push_str("Cancel").ok();
        Rgb565::WHITE
    };
    view.text8(text.as_str(), 8, y, color);
}

fn draw_banner<D>(view: &mut BenchVoltView<D>, state: &AppState)
where
    D: DrawTarget<Color = Rgb565>,
{
    view.fill_rect(140, 3, 180, 20, Rgb565::BLACK);
    let mut text: String<32> = String::new();
    let color = if let Some(requested) = state.pd_banner_mv {
        text.push_str("REQ ").ok();
        BenchVoltView::<D>::write_trimmed_volts(&mut text, requested / 10);
        text.push('V').ok();
        match state.pd_contract {
            Some(contract) if contract.millivolts == requested => {
                text.push_str(" OK").ok();
                Rgb565::GREEN
            }
            Some(contract) => {
                text.push_str(" GOT ").ok();
                BenchVoltView::<D>::write_trimmed_volts(&mut text, contract.millivolts / 10);
                text.push('V').ok();
                Rgb565::YELLOW
            }
            None => {
                text.push_str(" WAIT").ok();
                Rgb565::YELLOW
            }
        }
    } else if state.pd_source_error {
        text.push_str("PD ERROR").ok();
        Rgb565::RED
    } else if outputs_hint(state) {
        // Both the capability read and Apply wait for the outputs to be
        // inactive; a stalled list or dead Apply control needs a reason.
        // (With outputs off, the read completes within one loop pass, so
        // no in-progress state is worth painting.)
        text.push_str("OUTPUTS MUST BE OFF").ok();
        Rgb565::RED
    } else {
        return;
    };
    view.text8(text.as_str(), 144, 8, color);
}
