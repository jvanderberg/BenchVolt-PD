//! Deterministic randomized soak tests: thousands of rapid encoder, USB,
//! protection, and driver-failure events with the full invariant set checked
//! after every dispatch (Harness::dispatch and Harness::tick both do so).

mod common;

use benchvolt_pd::app::{Action, AwgStatus, RegulationMode, Screen};
use benchvolt_pd::power::DriverOperation;
use common::{FailureMode, Harness};

/// xorshift64* -- small, deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn accel(&mut self) -> i8 {
        let magnitude = 1 + self.below(9) as i8; // 1..=9, >=8 is a fast spin
        if self.below(2) == 0 {
            magnitude
        } else {
            -magnitude
        }
    }
}

fn any_disable(operation: &DriverOperation) -> bool {
    matches!(
        operation,
        DriverOperation::ChannelGate { enabled: false, .. }
            | DriverOperation::RailEnable { enabled: false, .. }
            | DriverOperation::Ch5Enable(false)
            | DriverOperation::Ch5OutputEnable(false)
    )
}

fn random_event(harness: &mut Harness, rng: &mut Rng) {
    match rng.below(18) {
        0 | 1 | 2 => harness.detent(rng.accel()),
        3 => {
            harness.dispatch(Action::NextControl);
        }
        4 => {
            let action = if rng.below(2) == 0 {
                Action::NextScreen
            } else {
                Action::PreviousScreen
            };
            harness.dispatch(action);
        }
        5 => {
            let action = match rng.below(4) {
                0 => Action::GoMainMenu,
                1 => Action::GoOverview,
                2 => Action::NavigateMenu(if rng.below(2) == 0 { 1 } else { -1 }),
                _ => Action::ActivateMenu,
            };
            harness.dispatch(action);
        }
        6 => {
            harness.dispatch(Action::SetVoltage {
                channel: rng.below(8) as u8,
                millivolts: rng.below(30_000) as u16,
            });
        }
        7 => {
            harness.dispatch(Action::SetCurrentLimit {
                channel: rng.below(8) as u8,
                milliamps: rng.below(5_000) as u16,
            });
        }
        8 => {
            harness.dispatch(Action::SetOutputRequested {
                channel: rng.below(7) as u8,
                enabled: rng.below(2) == 0,
            });
        }
        9 => {
            harness.dispatch(Action::SetRegulationMode {
                channel: rng.below(7) as u8,
                mode: if rng.below(2) == 0 {
                    RegulationMode::Cv
                } else {
                    RegulationMode::Cc
                },
            });
        }
        10 => {
            harness.dispatch(Action::SetSinkCurrentLimit(rng.below(7_000) as u16));
        }
        11 => {
            harness.dispatch(Action::AwgSample(rng.below(25_000) as u16));
        }
        12 => {
            harness.dispatch(Action::RequestArbStart {
                channel: rng.below(7) as u8,
                initial_mv: rng.below(25_000) as u16,
                low_mv: rng.below(25_000) as u16,
                high_mv: rng.below(25_000) as u16,
            });
        }
        13 => {
            let channel = rng.below(5) as usize;
            harness.load_ma[channel] = rng.below(4_000) as u16;
        }
        14 => harness.measurements_valid = rng.below(4) != 0,
        15 => {
            harness.temp = match rng.below(8) {
                0 => None,
                1 => Some(80 * 16), // over-temperature trip
                _ => Some(25 * 16),
            };
        }
        16 => {
            harness.driver_mut().failures = match rng.below(4) {
                0 => FailureMode::Intermittent {
                    period: 5 + rng.below(8) as usize,
                },
                1 => FailureMode::FailMatching(any_disable),
                _ => FailureMode::None,
            };
        }
        _ => harness.tick(rng.below(50) as u32),
    }
}

#[test]
fn random_event_storm_holds_invariants_and_recovers_on_every_seed() {
    for seed in [1, 42, 0xdead_beef, 0xbad_c0ffee] {
        let mut rng = Rng::new(seed);
        let mut harness = Harness::new();
        for _ in 0..2_500 {
            random_event(&mut harness, &mut rng);
            harness.tick(rng.below(4) as u32);
        }
        // After the storm the system must always be able to quiesce: nothing
        // energized, no plan pending, all transitions stable.
        harness.quiesce();
    }
}

#[test]
fn rapid_encoder_spins_during_screen_changes_stay_coherent() {
    for seed in [7, 0x5eed] {
        let mut rng = Rng::new(seed);
        let mut harness = Harness::new();
        for _ in 0..1_500 {
            for _ in 0..1 + rng.below(10) {
                harness.detent(rng.accel());
            }
            match rng.below(5) {
                0 => {
                    harness.dispatch(Action::NextScreen);
                }
                1 => {
                    harness.dispatch(Action::PreviousScreen);
                }
                2 => {
                    harness.dispatch(Action::NextControl);
                }
                3 => {
                    harness.dispatch(Action::GoMainMenu);
                }
                _ => harness.tick(rng.below(25) as u32),
            }
        }
        harness.quiesce();
    }
}

#[test]
fn hammering_awg_start_stop_with_outputs_enabled_stays_safe() {
    for seed in [3, 0xa5a5] {
        let mut rng = Rng::new(seed);
        let mut harness = Harness::new();
        harness.enable_channel(0);
        harness.enable_channel(3);

        for _ in 0..250 {
            // Navigate MainMenu -> AWG. Entering the screen is itself a
            // global-shutdown boundary while outputs are enabled.
            harness.dispatch(Action::GoMainMenu);
            harness.dispatch(Action::NavigateMenu(1));
            harness.dispatch(Action::ActivateMenu);
            assert!(harness.state().screen == Screen::Awg);

            // Move to the start/stop item and hammer it with random timing.
            for _ in 0..6 {
                harness.dispatch(Action::NavigateMenu(1));
            }
            harness.dispatch(Action::ActivateMenu); // start (or fault ack)
            harness.tick(rng.below(300) as u32);
            harness.dispatch(Action::ActivateMenu); // stop (or start retry)
            harness.tick(rng.below(120) as u32);

            if rng.below(6) == 0 {
                harness.driver_mut().failures = FailureMode::Intermittent {
                    period: 6 + rng.below(6) as usize,
                };
            } else if rng.below(3) == 0 {
                harness.driver_mut().failures = FailureMode::None;
            }
            if rng.below(4) == 0 {
                harness.dispatch(Action::SetOutputRequested {
                    channel: rng.below(5) as u8,
                    enabled: rng.below(2) == 0,
                });
            }
        }
        harness.quiesce();

        // A cleanly recovered unit can be re-enabled afterwards.
        if matches!(
            harness.state().awg_status,
            AwgStatus::Stopped | AwgStatus::Fault
        ) {
            harness.enable_channel(1);
            assert!(harness.driver().physically_energized());
            harness.quiesce();
        }
    }
}
