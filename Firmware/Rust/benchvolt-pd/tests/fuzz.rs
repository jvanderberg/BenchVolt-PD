//! Deterministic randomized soak tests: thousands of rapid encoder, USB,
//! protection, and driver-failure events with the full invariant set checked
//! after every dispatch (Harness::dispatch and Harness::tick both do so).

mod common;

use benchvolt_pd::app::{Action, AwgStatus, Fault, RegulationMode, Screen};
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
    match rng.below(25) {
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
                0 => Action::NavigateBack,
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
        17 => {
            harness.dispatch(Action::ToggleOutputRequested {
                channel: rng.below(7) as u8,
            });
        }
        18 => {
            // AWG configuration and start attempts with arbitrary values.
            let action = match rng.below(3) {
                0 => Action::AdjustAwg(rng.accel()),
                1 => Action::ConfigureAwg(benchvolt_pd::app::AwgConfig {
                    channel: rng.below(8) as u8,
                    waveform: match rng.below(4) {
                        0 => benchvolt_pd::app::AwgWaveform::Square,
                        1 => benchvolt_pd::app::AwgWaveform::Triangle,
                        2 => benchvolt_pd::app::AwgWaveform::Ramp,
                        _ => benchvolt_pd::app::AwgWaveform::Sine,
                    },
                    frequency_millihz: rng.below(200_000) as u32,
                    duty_percent: rng.below(120) as u8,
                    low_mv: rng.below(25_000) as u16,
                    high_mv: rng.below(25_000) as u16,
                }),
                _ => Action::RequestAwgStart,
            };
            harness.dispatch(action);
        }
        19 | 20 => {
            // INVARIANT (token discipline): completion-shaped actions with
            // stale operation tokens must never move physical state; only
            // the exact pending transition's token may. The forged token is
            // guaranteed non-matching — a forgery carrying the live token is
            // indistinguishable from the real executor completion, so the
            // reducer legitimately accepts it while the mock hardware never
            // performed the work.
            let channel = rng.below(7) as u8;
            let live = harness
                .state()
                .channels
                .get(usize::from(channel))
                .map_or(0, |output| output.operation);
            let operation = live.wrapping_add(1 + rng.below(8) as u16);
            let action = match rng.below(4) {
                0 => Action::OutputApplied {
                    channel,
                    operation,
                    enabled: rng.below(2) == 0,
                },
                1 => Action::OutputEnergized { channel, operation },
                2 => Action::OutputFailed {
                    channel,
                    operation,
                    fault: Fault::Hardware,
                },
                _ => {
                    // HardwareSettingFailed carries no token; its emitter
                    // contract is that a real channel's hardware was
                    // best-effort shut down before the action was dispatched
                    // (out-of-range channels are refusals that touch no
                    // hardware). Mirror that so the injection stays a
                    // possible reality.
                    if usize::from(channel) < 5 {
                        let state = *harness.state();
                        benchvolt_pd::power::best_effort_shutdown(
                            harness.driver_mut(),
                            &state,
                            channel,
                        );
                    }
                    Action::HardwareSettingFailed {
                        channel,
                        fault: Fault::Hardware,
                    }
                }
            };
            harness.dispatch(action);
        }
        21 => {
            harness.dispatch(Action::BootRecoveryStatus(rng.below(2) == 0));
        }
        22 => {
            // PD Source list loads with arbitrary lengths and rows; combined
            // with the menu/click cases above this fuzzes arm and apply
            // (the harness mirrors main.rs by completing pending applies),
            // which must uphold "no apply while outputs live".
            let mut pdos = [benchvolt_pd::app::NO_PDO; benchvolt_pd::app::PD_SOURCE_MAX_PDOS];
            let count = rng.below(1 + pdos.len() as u64) as u8;
            for (index, pdo) in pdos[..usize::from(count)].iter_mut().enumerate() {
                *pdo = benchvolt_pd::pd::FixedPdo {
                    source_position: index as u8 + 1,
                    millivolts: (1 + rng.below(400) as u16) * 50,
                    milliamps: (1 + rng.below(500) as u16) * 10,
                };
            }
            harness.dispatch(Action::PdSourceListLoaded {
                pdos,
                count,
                error: rng.below(4) == 0,
            });
        }
        23 => {
            harness.dispatch(Action::PdoApplyFinished(rng.below(2) == 0));
        }
        _ => harness.tick(rng.below(50) as u32),
    }
}

/// COVERAGE GUARDRAIL - the pattern to follow when adding an `Action`:
///
/// This match has no wildcard arm, so adding a variant to `Action` fails to
/// compile until you make a decision here:
///   - `Fuzzed`: add it to `random_event` above (directly, or note which
///     harness path emits it) so the invariant checks run against it, or
///   - `Excluded("reason")`: state why random injection is unsafe or
///     meaningless for the harness.
/// Do not reach for `Excluded` to save time: every excluded variant is a
/// blind spot in `assert_invariants`/`assert_bounded_slew` coverage.
#[allow(dead_code)]
enum Coverage {
    Fuzzed,
    Excluded(&'static str),
}

#[allow(dead_code)]
fn action_fuzz_coverage(action: &Action) -> Coverage {
    use Coverage::{Excluded, Fuzzed};
    match action {
        // Directly generated by random_event.
        Action::NextScreen
        | Action::PreviousScreen
        | Action::NavigateBack
        | Action::GoOverview
        | Action::NavigateMenu(_)
        | Action::ActivateMenu
        | Action::NextControl
        | Action::AdjustFocused(_)
        | Action::ToggleOutputRequested { .. }
        | Action::SetVoltage { .. }
        | Action::SetCurrentLimit { .. }
        | Action::SetRegulationMode { .. }
        | Action::SetSinkCurrentLimit(_)
        | Action::SetOutputRequested { .. }
        | Action::AdjustAwg(_)
        | Action::ConfigureAwg(_)
        | Action::RequestAwgStart
        | Action::RequestArbStart { .. }
        | Action::AwgSample(_)
        | Action::BootRecoveryStatus(_)
        | Action::OutputApplied { .. }
        | Action::OutputEnergized { .. }
        | Action::OutputFailed { .. }
        | Action::HardwareSettingFailed { .. }
        | Action::PdSourceListLoaded { .. }
        | Action::PdoApplyFinished(_) => Fuzzed,

        // Emitted by the harness itself while fuzz runs: tick() drives the
        // protection/measurement/AWG service paths exactly like main.rs.
        Action::Temperature(_)
        | Action::Measurements(_)
        | Action::SinkMeasurement(_)
        | Action::RegulateChannel { .. }
        | Action::ProtectionTrip { .. }
        | Action::SinkProtectionTrip(_)
        | Action::SinkProtectionRecovered
        | Action::AwgStartPrepared
        | Action::AwgStopped
        | Action::AwgLoadMeasurement(_)
        | Action::GlobalShutdownApplied
        | Action::GlobalShutdownFailed
        | Action::HardwareSettingApplied
        | Action::PdNegotiated(_)
        | Action::PdNegotiationStarted
        | Action::PdFailed(_) => Fuzzed,

        Action::RequestReboot => {
            Excluded("sets reboot_requested; main.rs resets the MCU, harness has no reboot")
        }
        Action::ProfileOperationFinished(_) => {
            Excluded("status banner only; profile flow is driven by integration tests")
        }
        Action::ApplyProfile(_, _) => Excluded(
            "main.rs only dispatches it after a verified global shutdown; \
             injecting it out of that sequence would fuzz an unreachable path",
        ),
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
                    harness.dispatch(Action::NavigateBack);
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
            harness.dispatch(Action::NavigateBack);
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
