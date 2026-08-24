//! Pure mapping from physical input samples to application actions.

use crate::app::{Action, AppState, ControlFocus, Screen};

const BUTTON_DEBOUNCE_MS: u16 = 50;
const BUTTON_CLICK_MIN_MS: u16 = 30;
const OVERVIEW_HOLD_MS: u16 = 500;
const REBOOT_HOLD_MS: u16 = 3_000;

pub fn encoder_action(state: &AppState, direction: i8, accelerated: i8) -> Option<Action> {
    if direction == 0 {
        return None;
    }
    Some(match state.focus {
        ControlFocus::None => {
            if state.screen == Screen::Awg && state.awg_editing {
                Action::AdjustAwg(accelerated)
            } else if matches!(
                state.screen,
                Screen::MainMenu
                    | Screen::Awg
                    | Screen::Settings
                    | Screen::ProfileSave
                    | Screen::ProfileLoad
                    | Screen::System
                    | Screen::Help
            ) {
                Action::NavigateMenu(direction)
            } else if direction < 0 {
                Action::PreviousScreen
            } else {
                Action::NextScreen
            }
        }
        ControlFocus::Output => {
            let Screen::Channel(channel) = state.screen else {
                return None;
            };
            Action::ToggleOutputRequested { channel }
        }
        ControlFocus::OverviewOutput(channel) => Action::ToggleOutputRequested { channel },
        _ => Action::AdjustFocused(accelerated),
    })
}

pub struct ButtonTracker {
    high: bool,
    last_press_tick: u16,
    pressed_at: Option<u16>,
    overview_hold_fired: bool,
}

impl ButtonTracker {
    pub const fn new(initial_high: bool) -> Self {
        Self {
            high: initial_high,
            last_press_tick: 0,
            pressed_at: None,
            overview_hold_fired: false,
        }
    }

    pub const fn is_high(&self) -> bool {
        self.high
    }

    pub fn sample(&mut self, tick: u16, high: bool) -> Option<Action> {
        let mut action = None;
        if self.high && !high && tick.wrapping_sub(self.last_press_tick) >= BUTTON_DEBOUNCE_MS {
            self.pressed_at = Some(tick);
            self.overview_hold_fired = false;
        }
        if !high {
            if let Some(pressed_at) = self.pressed_at {
                let held_ms = tick.wrapping_sub(pressed_at);
                if held_ms >= REBOOT_HOLD_MS {
                    self.pressed_at = None;
                    action = Some(Action::RequestReboot);
                } else if held_ms >= OVERVIEW_HOLD_MS && !self.overview_hold_fired {
                    self.overview_hold_fired = true;
                    action = Some(Action::GoMainMenu);
                }
            }
        } else if !self.high {
            if let Some(pressed_at) = self.pressed_at.take() {
                let held_ms = tick.wrapping_sub(pressed_at);
                self.last_press_tick = tick;
                if held_ms >= OVERVIEW_HOLD_MS {
                    action = Some(Action::GoMainMenu);
                } else if held_ms >= BUTTON_CLICK_MIN_MS {
                    action = Some(Action::NextControl);
                }
            }
        }
        self.high = high;
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_routes_navigation_editing_and_output_focus() {
        let mut state = AppState::new(true, None);
        assert!(encoder_action(&state, 0, 8).is_none());
        assert!(matches!(
            encoder_action(&state, 1, 4),
            Some(Action::NavigateMenu(1))
        ));
        state.screen = Screen::Awg;
        state.awg_editing = true;
        assert!(matches!(
            encoder_action(&state, 1, 8),
            Some(Action::AdjustAwg(8))
        ));
        state.screen = Screen::Channel(4);
        state.awg_editing = false;
        assert!(matches!(
            encoder_action(&state, -1, -4),
            Some(Action::PreviousScreen)
        ));
        state.focus = ControlFocus::Output;
        assert!(matches!(
            encoder_action(&state, 1, 4),
            Some(Action::ToggleOutputRequested { channel: 4 })
        ));
        state.focus = ControlFocus::Voltage;
        assert!(matches!(
            encoder_action(&state, -1, -8),
            Some(Action::AdjustFocused(-8))
        ));
    }

    #[test]
    fn button_debounces_clicks_and_emits_bounded_hold_actions() {
        let mut button = ButtonTracker::new(true);
        assert!(button.sample(10, false).is_none());
        assert!(button.sample(20, true).is_none());

        assert!(button.sample(100, false).is_none());
        assert!(matches!(
            button.sample(140, true),
            Some(Action::NextControl)
        ));

        assert!(button.sample(200, false).is_none());
        assert!(matches!(
            button.sample(700, false),
            Some(Action::GoMainMenu)
        ));
        assert!(button.sample(701, false).is_none());
        assert!(matches!(
            button.sample(3_200, false),
            Some(Action::RequestReboot)
        ));
        assert!(button.sample(3_201, false).is_none());
    }

    #[test]
    fn timing_remains_correct_across_tick_wrap() {
        let mut button = ButtonTracker::new(true);
        assert!(button.sample(u16::MAX - 20, false).is_none());
        assert!(matches!(button.sample(20, true), Some(Action::NextControl)));
    }

    #[test]
    fn button_threshold_edges_and_existing_hold_release_behavior_are_preserved() {
        let mut button = ButtonTracker::new(true);
        assert!(button.sample(49, false).is_none());
        assert!(button.sample(50, true).is_none());

        assert!(button.sample(100, false).is_none());
        assert!(button.sample(129, true).is_none());
        assert!(button.sample(178, false).is_none());
        assert!(button.sample(179, true).is_none());
        assert!(button.sample(179, false).is_none());
        assert!(matches!(
            button.sample(209, true),
            Some(Action::NextControl)
        ));

        assert!(button.sample(259, false).is_none());
        assert!(matches!(
            button.sample(759, false),
            Some(Action::GoMainMenu)
        ));
        // This duplicate on release is legacy behavior, captured deliberately
        // so changing it later is an explicit UI decision rather than refactor drift.
        assert!(matches!(button.sample(760, true), Some(Action::GoMainMenu)));
    }
}
