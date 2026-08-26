//! Pure mapping from physical input samples to application actions.

use crate::app::{Action, AppState, ControlFocus, Screen};

const BUTTON_DEBOUNCE_MS: u16 = 50;
const BUTTON_CLICK_MIN_MS: u16 = 30;
const OVERVIEW_HOLD_MS: u16 = 500;
const REBOOT_HOLD_MS: u16 = 3_000;

/// Match the direction observed on the assembled r3 unit: PB13 low on the
/// PB12 rising edge is clockwise/upward. The legacy C callback names the
/// opposite electrical phase `OnRotaryUP`, but traverses menus in reverse.
pub const fn encoder_direction(dt_high: bool) -> i8 {
    if dt_high {
        -1
    } else {
        1
    }
}

/// Detents arriving faster than this reset velocity tracking when idle.
pub const ENCODER_ACCELERATION_IDLE_MS: u16 = 80;

/// Velocity/acceleration state folded over queued encoder detents.
#[derive(Clone, Copy, Default)]
pub struct EncoderAccumulator {
    pub last_tick: u16,
    pub last_direction: i8,
    pub velocity: u8,
}

impl EncoderAccumulator {
    /// Fold one queued detent into the running totals. Returns the
    /// (raw, accelerated) contribution of this event. Direction reversals
    /// and >80 ms gaps reset the velocity ladder; sustained same-direction
    /// spins climb a 1/2/4/8/16 multiplier ladder capped at velocity 16.
    pub fn step(&mut self, direction: i8, tick: u16) -> (i16, i16) {
        let elapsed = tick.wrapping_sub(self.last_tick);
        if direction != self.last_direction || elapsed > ENCODER_ACCELERATION_IDLE_MS {
            self.velocity = 1;
        } else {
            self.velocity = self.velocity.saturating_add(1).min(16);
        }
        self.last_tick = tick;
        self.last_direction = direction;
        let multiplier: i16 = match self.velocity {
            0 | 1 => 1,
            2..=3 => 2,
            4..=5 => 4,
            6..=8 => 8,
            _ => 16,
        };
        (i16::from(direction), i16::from(direction) * multiplier)
    }
}

/// Clamp folded totals into the i8 range carried by `Action::AdjustFocused`.
pub fn clamp_adjustment(raw: i16, accelerated: i16) -> (i8, i8) {
    (
        raw.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
        accelerated.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
    )
}

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
                    | Screen::PdSource
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
                    action = Some(Action::NavigateBack);
                }
            }
        } else if !self.high {
            if let Some(pressed_at) = self.pressed_at.take() {
                let held_ms = tick.wrapping_sub(pressed_at);
                self.last_press_tick = tick;
                if held_ms >= OVERVIEW_HOLD_MS {
                    // Fire only if the in-hold sample did not already: back
                    // navigation is hierarchical, so a duplicate would step
                    // up two levels (the old always-main-menu target made
                    // the duplicate harmlessly idempotent).
                    if !self.overview_hold_fired {
                        action = Some(Action::NavigateBack);
                    }
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
    fn encoder_accumulator_climbs_and_resets_the_multiplier_ladder() {
        let mut acc = EncoderAccumulator::default();
        // First detent after idle: velocity 1, multiplier 1.
        assert_eq!(acc.step(1, 100), (1, 1));
        // Sustained same-direction spin climbs 2/2/4/4/8/8/8/16...
        assert_eq!(acc.step(1, 110), (1, 2));
        assert_eq!(acc.step(1, 120), (1, 2));
        assert_eq!(acc.step(1, 130), (1, 4));
        assert_eq!(acc.step(1, 140), (1, 4));
        assert_eq!(acc.step(1, 150), (1, 8));
        for tick in [160, 170, 180] {
            acc.step(1, tick);
        }
        assert_eq!(acc.step(1, 190), (1, 16));
        // Velocity saturates at 16.
        assert_eq!(acc.step(1, 200), (1, 16));
        // A direction reversal resets to multiplier 1.
        assert_eq!(acc.step(-1, 210), (-1, -1));
        // An idle gap longer than 80 ms resets as well.
        acc.step(-1, 220);
        assert_eq!(acc.step(-1, 301), (-1, -1));
        // Tick wrap-around still measures the true elapsed gap.
        let mut acc = EncoderAccumulator {
            last_tick: u16::MAX - 5,
            last_direction: 1,
            velocity: 4,
        };
        // Velocity 4 -> 5, still in the x4 band; the wrapped 10 ms gap must
        // not read as idle.
        assert_eq!(acc.step(1, 4), (1, 4));
    }

    #[test]
    fn folded_adjustments_clamp_into_the_action_payload_range() {
        assert_eq!(clamp_adjustment(300, -300), (127, -128));
        assert_eq!(clamp_adjustment(-5, 40), (-5, 40));
    }

    #[test]
    fn encoder_routes_navigation_editing_and_output_focus() {
        let mut state = AppState::new(true, None);
        state.screen = Screen::MainMenu;
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
    fn encoder_direction_matches_the_legacy_hardware_mapping() {
        // Device-observed clockwise rotation has PB13 low at PB12's rising edge.
        assert_eq!(encoder_direction(false), 1);
        assert_eq!(encoder_direction(true), -1);
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
            Some(Action::NavigateBack)
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
            Some(Action::NavigateBack)
        ));
        // The legacy duplicate fire on release is gone: back navigation is
        // hierarchical now, so a duplicate would step up two levels.
        assert!(button.sample(760, true).is_none());

        // A release crossing the hold threshold between samples still fires
        // exactly once.
        assert!(button.sample(900, false).is_none());
        assert!(matches!(
            button.sample(1_450, true),
            Some(Action::NavigateBack)
        ));
    }
}
