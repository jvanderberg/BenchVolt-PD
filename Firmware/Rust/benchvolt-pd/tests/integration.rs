//! Host-run integration tests: timed user flows over the full reactive stack
//! (reducer, effect planner, staged power executor, protection cadence).

mod common;

use benchvolt_pd::app::{
    Action, AppState, AwgConfig, AwgStatus, AwgWaveform, ControlFocus, Fault, RegulationMode,
    Screen, TemperatureUnit,
};
use benchvolt_pd::limits::{
    CH4_MAX_VOLTAGE_MV, CH4_MIN_VOLTAGE_MV, CH5_MAX_VOLTAGE_MV, CH5_MIN_VOLTAGE_MV,
};
use benchvolt_pd::power::DriverOperation;
use benchvolt_pd::settings::PersistentSettings;
use common::{assert_invariants, FailureMode, Harness};

fn de_energizing(operation: &DriverOperation) -> bool {
    matches!(
        operation,
        DriverOperation::ChannelGate { enabled: false, .. }
            | DriverOperation::RailEnable { enabled: false, .. }
            | DriverOperation::Ch5Enable(false)
            | DriverOperation::Ch5OutputEnable(false)
    )
}

#[test]
fn encoder_flow_enables_a_channel_through_the_staged_executor() {
    let mut harness = Harness::new();
    assert!(harness.state().screen == Screen::MainMenu);

    // Click activates "Overview", a detent moves to CH1, a click focuses the
    // output control, and a detent toggles the enable request.
    harness.click();
    assert!(harness.state().screen == Screen::Overview);
    harness.detent(1);
    assert!(harness.state().screen == Screen::Channel(0));
    harness.click();
    assert!(harness.state().focus == ControlFocus::Output);
    harness.detent(1);
    assert!(harness.state().channels[0].requested_enabled);
    assert!(!harness.state().channels[0].physical_enabled);

    // The staged plan needs two 50 ms settle windows before OutputApplied.
    harness.tick(200);
    let output = &harness.state().channels[0];
    assert!(output.physical_enabled);
    assert!(output.transition == benchvolt_pd::app::OutputTransition::Stable);
    assert!(output.fault == Fault::None);
    assert!(harness.driver().gates[0]);
    assert!(harness.driver().rail_enabled[0]);
    assert!(harness.driver().physically_energized());
    assert!(harness.driver().safe());
    assert_invariants(&harness);
}

#[test]
fn voltage_edit_while_enabled_slews_in_bounded_steps() {
    let mut harness = Harness::new();
    harness.enable_channel(3);
    assert_eq!(harness.state().channels[3].drive_mv, 5_000);

    // Downward edit: the drive must walk to the setpoint through
    // RegulateChannel ticks, never jumping more than 200 mV per DAC write.
    harness.dispatch(Action::SetVoltage {
        channel: 3,
        millivolts: 3_000,
    });
    assert_eq!(harness.state().channels[3].drive_mv, 5_000);
    harness.tick(400);
    assert_eq!(harness.state().channels[3].drive_mv, 3_000);

    // Upward CV edit slews the same way.
    harness.dispatch(Action::SetVoltage {
        channel: 3,
        millivolts: 4_200,
    });
    harness.tick(400);
    assert_eq!(harness.state().channels[3].drive_mv, 4_200);

    let writes = harness.driver().dac_writes();
    assert!(
        writes.len() > 15,
        "expected many slew steps, got {writes:?}"
    );
    for pair in writes.windows(2) {
        let step = pair[1].abs_diff(pair[0]);
        assert!(
            step <= 200,
            "hardware voltage write jumped {step} mV in {writes:?}"
        );
    }
    assert_eq!(*writes.last().unwrap(), 4_200);
}

#[test]
fn regulation_mode_toggle_while_enabled_does_not_jump_the_drive() {
    let mut harness = Harness::new();
    harness.enable_channel(3);
    // Freeze mid-slew: setpoint far below the current drive.
    harness.dispatch(Action::SetVoltage {
        channel: 3,
        millivolts: 3_000,
    });
    assert_eq!(harness.state().channels[3].drive_mv, 5_000);
    let dac_writes_before = harness.driver().dac_writes().len();

    harness.focus_channel_control(3, 3);
    assert!(harness.state().focus == ControlFocus::RegulationMode);
    harness.detent(1);
    assert!(harness.state().channels[3].regulation_mode == RegulationMode::Cc);

    // Regression for the reducer fix: the toggle must not snap drive_mv to
    // the setpoint while physically enabled, and must not emit any hardware
    // voltage write of its own.
    assert_eq!(harness.state().channels[3].drive_mv, 5_000);
    assert_eq!(harness.driver().dac_writes().len(), dac_writes_before);

    harness.detent(1);
    assert!(harness.state().channels[3].regulation_mode == RegulationMode::Cv);
    assert_eq!(harness.state().channels[3].drive_mv, 5_000);
    assert_eq!(harness.driver().dac_writes().len(), dac_writes_before);
}

#[test]
fn overcurrent_trips_only_after_three_confirmed_samples() {
    let mut harness = Harness::new();
    harness.enable_channel(0);
    // Move well past the 10-sample startup grace window.
    harness.tick(300);

    // Above the 3 A limit but below the 3.3 A hard ceiling.
    harness.load_ma[0] = 3_200;
    harness.tick(40); // two 20 ms protection samples
    assert!(harness.state().channels[0].fault == Fault::None);
    assert!(harness.state().channels[0].physical_enabled);

    harness.tick(20); // third consecutive violating sample
    let output = &harness.state().channels[0];
    assert!(output.fault == Fault::OverCurrent);
    assert!(!output.requested_enabled);
    harness.load_ma[0] = 0;
    harness.tick(100);
    assert!(!harness.state().channels[0].physical_enabled);
    assert!(!harness.driver().physically_energized());
    assert!(harness.driver().safe());
}

#[test]
fn overcurrent_below_confirmation_count_does_not_trip() {
    let mut harness = Harness::new();
    harness.enable_channel(0);
    harness.tick(300);

    // Two violating samples, then back in range: the counter must reset.
    harness.load_ma[0] = 3_200;
    harness.tick(40);
    harness.load_ma[0] = 1_000;
    harness.tick(100);
    harness.load_ma[0] = 3_200;
    harness.tick(40);
    assert!(harness.state().channels[0].fault == Fault::None);
    assert!(harness.state().channels[0].physical_enabled);
}

#[test]
fn startup_grace_suppresses_moderate_but_not_gross_overcurrent() {
    let mut harness = Harness::new();
    harness.dispatch(Action::SetOutputRequested {
        channel: 0,
        enabled: true,
    });
    harness.tick(120);
    assert!(harness.state().channels[0].physical_enabled);

    // Moderate overcurrent within the grace window is tolerated.
    harness.load_ma[0] = 3_200;
    harness.tick(100); // five samples, still inside the 10-sample grace
    assert!(harness.state().channels[0].fault == Fault::None);

    // A gross excursion beyond the physical envelope trips immediately.
    harness.load_ma[0] = 3_400;
    harness.tick(20);
    assert!(harness.state().channels[0].fault == Fault::OverCurrent);
    harness.load_ma[0] = 0;
    harness.tick(100);
    assert!(!harness.state().channels[0].physical_enabled);
    assert!(!harness.driver().physically_energized());
}

#[test]
fn failed_global_shutdown_never_claims_outputs_are_off() {
    let mut harness = Harness::new();
    harness.enable_channel(0);
    assert!(harness.driver().physically_energized());

    // Every de-energizing driver command fails, then entering the AWG screen
    // triggers the planner's global-shutdown boundary.
    harness.driver_mut().failures = FailureMode::FailMatching(de_energizing);
    harness.dispatch(Action::NavigateMenu(1));
    harness.dispatch(Action::ActivateMenu);
    assert!(harness.state().screen == Screen::Awg);

    // The fix under test: GlobalShutdownFailed must not clear
    // physical_enabled while the hardware is still energized.
    assert!(harness.driver().physically_energized());
    assert!(!harness.state().outputs_physically_off());
    assert!(harness.state().channels[0].physical_enabled);
    for output in &harness.state().channels {
        assert!(output.fault == Fault::Hardware);
        assert!(!output.requested_enabled);
    }
    assert!(harness.state().awg_status == AwgStatus::Fault);

    // Once the driver recovers, the system reaches a fully-off state.
    harness.quiesce();
}

#[test]
fn pd_source_apply_flow_arms_guards_and_reports_the_outcome() {
    use benchvolt_pd::app::{PdoApply, PD_SOURCE_MAX_PDOS};
    use benchvolt_pd::pd::FixedPdo;

    let mut harness = Harness::new();
    // Main-menu row 3 opens the PD Source screen and marks the list stale.
    for _ in 0..3 {
        harness.dispatch(Action::NavigateMenu(1));
    }
    harness.dispatch(Action::ActivateMenu);
    assert!(harness.state().screen == Screen::PdSource);
    assert!(harness.state().pd_source_stale);

    // The main.rs-shaped list load clears the stale marker.
    let mut pdos = [benchvolt_pd::app::NO_PDO; PD_SOURCE_MAX_PDOS];
    pdos[0] = FixedPdo {
        source_position: 1,
        millivolts: 5_000,
        milliamps: 3_000,
    };
    pdos[1] = FixedPdo {
        source_position: 2,
        millivolts: 12_000,
        milliamps: 3_000,
    };
    harness.dispatch(Action::PdSourceListLoaded {
        pdos,
        count: 2,
        error: false,
    });
    assert!(!harness.state().pd_source_stale);

    // Click on the 12 V row arms it; nothing is applied yet.
    harness.dispatch(Action::NavigateMenu(1));
    harness.dispatch(Action::ActivateMenu);
    assert_eq!(harness.state().pd_source_armed, Some(1));
    assert!(harness.state().pd_apply_request.is_none());

    // Apply while an output is live must be a rejected no-op.
    harness.enable_channel(0);
    harness.dispatch(Action::NavigateMenu(1));
    harness.dispatch(Action::ActivateMenu);
    assert!(harness.state().pd_apply_request.is_none());
    assert_eq!(harness.state().pdo_apply_pending_mv, 0);

    // With outputs inactive again, Apply arms the request and pending flag.
    harness.dispatch(Action::SetOutputRequested {
        channel: 0,
        enabled: false,
    });
    harness.tick(200);
    assert!(harness.state().outputs_inactive());
    harness.dispatch(Action::ActivateMenu);
    assert_eq!(
        harness.state().pd_apply_request,
        Some(PdoApply {
            millivolts: 12_000,
            milliamps: 3_000,
        })
    );
    assert_eq!(harness.state().pdo_apply_pending_mv, 12_000);
    assert_eq!(harness.state().pd_banner_mv, Some(12_000));

    // While the apply is in flight, no output may be enabled.
    harness.dispatch(Action::SetOutputRequested {
        channel: 0,
        enabled: true,
    });
    assert!(!harness.state().channels[0].requested_enabled);

    // The mirrored main-loop servicing completes the request; the in-place
    // renegotiation outcome then clears the journal flag and keeps the
    // banner. It must NOT mark the list stale: the capability read itself
    // provokes a renegotiation event, so re-reading on contract changes is a
    // self-sustaining loop.
    harness.tick(1);
    assert!(harness.state().pd_apply_request.is_none());
    harness.dispatch(Action::PdNegotiated(benchvolt_pd::pd::Contract {
        source_position: 2,
        millivolts: 12_000,
        operating_milliamps: 3_000,
        maximum_milliamps: 3_000,
    }));
    assert_eq!(harness.state().pdo_apply_pending_mv, 0);
    assert_eq!(harness.state().pd_banner_mv, Some(12_000));
    assert!(!harness.state().pd_source_stale);

    // The back gesture discards the armed choice and the banner.
    harness.dispatch(Action::NavigateBack);
    assert!(harness.state().pd_source_armed.is_none());
    assert!(harness.state().pd_banner_mv.is_none());
}

#[test]
fn hostile_persisted_settings_are_sanitized_before_reaching_state() {
    let mut state = AppState::new(true, Some(25 * 16));
    let hostile = PersistentSettings {
        current_limits_ma: [u16::MAX; 5],
        ch4_voltage_mv: 60_000,
        ch5_voltage_mv: 1,
        ch4_regulation_mode: RegulationMode::Cc,
        ch5_regulation_mode: RegulationMode::Cc,
        sink_current_limit_ma: u16::MAX,
        temperature_unit: TemperatureUnit::Fahrenheit,
        awg: AwgConfig {
            channel: 7, // would index out of bounds if used raw
            waveform: AwgWaveform::Square,
            frequency_millihz: u32::MAX,
            duty_percent: 0,
            low_mv: 60_000,
            high_mv: 0,
        },
        pdo_apply_pending_mv: 60_000,
    };
    hostile.apply_to(&mut state);

    for output in &state.channels {
        assert!(output.current_limit_ma <= 3_000);
    }
    assert!((CH4_MIN_VOLTAGE_MV..=CH4_MAX_VOLTAGE_MV).contains(&state.channels[3].setpoint_mv));
    assert!((CH5_MIN_VOLTAGE_MV..=CH5_MAX_VOLTAGE_MV).contains(&state.channels[4].setpoint_mv));
    assert_eq!(state.channels[3].drive_mv, state.channels[3].setpoint_mv);
    assert_eq!(state.channels[4].drive_mv, state.channels[4].setpoint_mv);
    assert!(state.sink_current_limit_ma <= 5_000);
    // An out-of-range AWG channel falls back to the safe default config.
    assert!(state.awg == AwgConfig::default());

    // A valid channel with inverted bounds is clamped rather than replaced.
    let mut inverted = hostile;
    inverted.awg.channel = 4;
    inverted.apply_to(&mut state);
    let awg = state.awg;
    assert!(matches!(awg.channel, 3 | 4));
    assert!((CH5_MIN_VOLTAGE_MV..=CH5_MAX_VOLTAGE_MV).contains(&awg.low_mv));
    assert!(awg.low_mv <= awg.high_mv && awg.high_mv <= CH5_MAX_VOLTAGE_MV);
    assert!((1..=99).contains(&awg.duty_percent));
    assert!(awg.frequency_millihz <= awg.waveform.max_frequency_millihz());
}
