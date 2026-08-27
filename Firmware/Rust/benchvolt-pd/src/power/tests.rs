extern crate std;

use super::*;
use crate::app::AppReducer;
use reducto::Reducer;
use std::vec::Vec;

#[test]
fn tps_status_faults_fail_closed_by_cause() {
    assert_eq!(tps55289_status_fault(0x00), None);
    assert_eq!(tps55289_status_fault(0x01), None);
    assert_eq!(tps55289_status_fault(0x40), Some(Fault::OverCurrent));
    assert_eq!(tps55289_status_fault(0x80), Some(Fault::OverCurrent));
    assert_eq!(tps55289_status_fault(0x20), Some(Fault::Hardware));
    assert_eq!(tps55289_status_fault(0xe0), Some(Fault::OverCurrent));
    assert!(tps55289_output_acknowledged(0x80, 0x00));
    assert!(!tps55289_output_acknowledged(0x00, 0x00));
    assert!(!tps55289_output_acknowledged(0x80, 0x20));
    assert!(!tps55289_output_acknowledged(0x80, 0x03));
}

#[test]
fn tps_codes_match_the_original_c_driver_equations() {
    assert_eq!(tps55289_voltage_code(800), 0);
    assert_eq!(tps55289_voltage_code(1_000), 20);
    assert_eq!(tps55289_voltage_code(12_000), 1_119);
    assert_eq!(tps55289_voltage_code(22_000), 0x07fe);

    assert_eq!(tps55289_current_code(0), 0);
    assert_eq!(tps55289_current_code(50), 0x81);
    assert_eq!(tps55289_current_code(3_000), 0xbc);
    assert_eq!(tps55289_current_code(SHARED_RAIL_LIMIT_MA), 0xe4);
    assert_eq!(tps55289_current_code(u16::MAX), 0xff);
}

#[test]
fn tps_register_updates_reject_invalid_readback_before_output_enable() {
    assert_eq!(
        tps55289_configuration_registers(0x80, 0x02, 0x00, false, false),
        Some([0x03, 0x00, 0x00])
    );
    assert_eq!(
        tps55289_configuration_registers(0x00, 0x00, 0x00, true, true),
        Some([0x03, 0x82, 0x03])
    );
    for invalid in [(0xff, 0x00, 0x00), (0x00, 0xff, 0x00), (0x00, 0x00, 0xff)] {
        assert_eq!(
            tps55289_configuration_registers(invalid.0, invalid.1, invalid.2, true, false),
            None
        );
    }
    assert_eq!(tps55289_output_mode(0x02, true), Some(0x82));
    assert_eq!(tps55289_output_mode(0x82, false), Some(0x02));
    assert_eq!(tps55289_output_mode(0xff, true), None);
    assert_eq!(tps55289_output_mode(0xff, false), None);
}

#[derive(Default)]
struct MockDriver {
    calls: Vec<DriverOperation>,
    fail_at: Option<usize>,
    rail_enabled: [bool; 2],
    rail_configured: [bool; 2],
    gates: [bool; 4],
    ch5_enable: bool,
    ch5_configured: bool,
    ch5_oe: bool,
}

impl MockDriver {
    fn rail_index(rail: Rail) -> usize {
        match rail {
            Rail::Dc1 => 0,
            Rail::Dc2 => 1,
        }
    }

    fn safe(&self) -> bool {
        (!self.gates[0] && !self.gates[1] || !self.rail_enabled[0] || self.rail_configured[0])
            && (!self.gates[2] && !self.gates[3]
                || !self.rail_enabled[1]
                || self.rail_configured[1])
            && (!self.ch5_oe || !self.ch5_enable || self.ch5_configured)
    }
}

impl PowerDriver for MockDriver {
    type Error = ();

    fn apply(&mut self, operation: DriverOperation) -> Result<(), Self::Error> {
        let call = self.calls.len();
        self.calls.push(operation);
        if self.fail_at == Some(call) {
            return Err(());
        }
        match operation {
            DriverOperation::ChannelGate { channel, enabled } => {
                self.gates[usize::from(channel)] = enabled
            }
            DriverOperation::RailEnable { rail, enabled } => {
                let index = Self::rail_index(rail);
                self.rail_enabled[index] = enabled;
                if !enabled {
                    self.rail_configured[index] = false;
                }
            }
            DriverOperation::ConfigureRail { rail, .. } => {
                let index = Self::rail_index(rail);
                if !self.rail_enabled[index] {
                    return Err(());
                }
                self.rail_configured[index] = true;
            }
            DriverOperation::VerifyRail { rail } => {
                let index = Self::rail_index(rail);
                if !self.rail_enabled[index] || !self.rail_configured[index] {
                    return Err(());
                }
            }
            DriverOperation::Ch5Enable(enabled) => {
                self.ch5_enable = enabled;
                if !enabled {
                    self.ch5_configured = false;
                    self.ch5_oe = false;
                }
            }
            DriverOperation::ConfigureCh5 { .. } => {
                if !self.ch5_enable {
                    return Err(());
                }
                self.ch5_configured = true;
            }
            DriverOperation::ClearCh5Status => {
                if !self.ch5_enable || !self.ch5_configured {
                    return Err(());
                }
            }
            DriverOperation::Ch5OutputEnable(enabled) => {
                if enabled && (!self.ch5_enable || !self.ch5_configured) {
                    return Err(());
                }
                self.ch5_oe = enabled;
            }
            DriverOperation::Ch5Voltage(_) | DriverOperation::Ch5VoltageUnverified(_) => {
                if !self.ch5_enable || !self.ch5_configured {
                    return Err(());
                }
            }
            DriverOperation::VerifyOutput { channel, .. } => {
                let enabled = if channel == 4 {
                    self.ch5_oe
                } else {
                    self.gates[usize::from(channel)]
                };
                if !enabled {
                    return Err(());
                }
            }
            DriverOperation::SetAdjustableDac { .. } => {}
        }
        assert!(self.safe());
        Ok(())
    }
}

fn eligible_state() -> AppState {
    let mut state = AppState::new(true, Some(25 * 16));
    state.pd_contract = Some(crate::pd::Contract {
        source_position: 1,
        millivolts: 5_000,
        operating_milliamps: 5_000,
        maximum_milliamps: 5_000,
    });
    for output in &mut state.channels {
        output.measurement = Measurement {
            millivolts: 0,
            milliamps: 0,
            valid: true,
        };
    }
    state
}

fn enabling_state(channel: u8, operation: u16) -> AppState {
    let mut state = eligible_state();
    let output = &mut state.channels[usize::from(channel)];
    output.requested_enabled = true;
    output.transition = OutputTransition::Enabling(operation);
    state
}

const fn enable_effect(channel: u8, operation: u16) -> PowerEffect {
    PowerEffect::Output {
        channel,
        operation,
        enabled: true,
    }
}

#[test]
fn mcp4725_codes_match_the_original_c_calibration_points() {
    // 0.50 V -> 3975 and 5.00 V -> 340 are the measured calibration
    // anchors from the C firmware's SetVadjLVoltage().
    assert_eq!(mcp4725_code_for_millivolts(500), 3_975);
    assert_eq!(mcp4725_code_for_millivolts(5_000), 340);
    // Monotonically decreasing (inverting stage) and clamped.
    assert!(mcp4725_code_for_millivolts(1_000) < mcp4725_code_for_millivolts(900));
    assert_eq!(
        mcp4725_code_for_millivolts(0),
        mcp4725_code_for_millivolts(500)
    );
    assert_eq!(
        mcp4725_code_for_millivolts(u16::MAX),
        mcp4725_code_for_millivolts(5_000)
    );
    // Always a valid 12-bit code.
    for millivolts in (500..=5_000).step_by(10) {
        assert!(mcp4725_code_for_millivolts(millivolts) <= 4_095);
    }
}

#[test]
fn overflowed_power_plan_fails_safe_instead_of_running_truncated() {
    let mut plan = PowerPlan::new(0, 1, true, Some(Rail::Dc1));
    for _ in 0..POWER_PLAN_CAPACITY {
        plan.push(DriverOperation::ChannelGate {
            channel: 0,
            enabled: false,
        });
    }
    assert!(plan.is_valid());
    plan.push(DriverOperation::ChannelGate {
        channel: 0,
        enabled: true,
    });
    assert!(!plan.is_valid());

    let state = enabling_state(0, 1);
    let mut executor = PowerExecutor::new(MockDriver::default(), 0);
    executor.pending = Some(plan);
    let action = executor.service(1, &state);
    assert!(matches!(
        action,
        Some(Action::OutputFailed {
            channel: 0,
            operation: 1,
            ..
        })
    ));
    assert!(executor.driver.safe());
    assert!(!executor.is_busy());
}

#[test]
fn voltage_effects_for_fixed_channels_report_failure_not_false_success() {
    let state = eligible_state();
    let mut executor = PowerExecutor::new(MockDriver::default(), 0);
    let action = executor.submit(
        &state,
        PowerEffect::Voltage {
            channel: 1,
            millivolts: 2_500,
        },
    );
    assert!(matches!(
        action,
        Some(Action::HardwareSettingFailed { channel: 1, .. })
    ));
    // No hardware operation may be attempted for a channel with no
    // voltage hardware.
    assert!(executor.calls.is_empty());
}

#[test]
fn voltage_write_for_a_channel_mid_plan_is_rejected() {
    let state = enabling_state(4, 3);
    let mut executor = PowerExecutor::new(MockDriver::default(), 0);
    assert!(executor.submit(&state, enable_effect(4, 3)).is_none());
    assert!(executor.is_busy());
    let calls_before = executor.calls.len();
    let action = executor.submit(
        &state,
        PowerEffect::Voltage {
            channel: 4,
            millivolts: 12_000,
        },
    );
    assert!(matches!(
        action,
        Some(Action::HardwareSettingFailed { channel: 4, .. })
    ));
    assert_eq!(executor.calls.len(), calls_before);
}

#[test]
fn staged_shared_rail_enable_obeys_both_deadlines_across_timer_wrap() {
    let state = enabling_state(0, 7);
    let mut executor = PowerExecutor::new(MockDriver::default(), u16::MAX - 15);

    assert!(executor.submit(&state, enable_effect(0, 7)).is_none());
    assert_eq!(
        executor.calls,
        [
            DriverOperation::ChannelGate {
                channel: 0,
                enabled: false
            },
            DriverOperation::RailEnable {
                rail: Rail::Dc1,
                enabled: true
            }
        ]
    );
    assert!(executor.service(33, &state).is_none());
    assert_eq!(executor.calls.len(), 2);

    assert!(executor.service(34, &state).is_none());
    assert!(matches!(
        executor.calls.last(),
        Some(DriverOperation::ConfigureRail {
            rail: Rail::Dc1,
            ..
        })
    ));
    assert!(executor.service(83, &state).is_none());
    assert_eq!(executor.calls.len(), 3);

    let completion = executor.service(84, &state);
    assert!(matches!(
        completion,
        Some(Action::OutputApplied {
            channel: 0,
            operation: 7,
            enabled: true
        })
    ));
    assert!(executor.gates[0]);
    assert!(matches!(
        executor.calls.as_slice(),
        [
            DriverOperation::ChannelGate { enabled: false, .. },
            DriverOperation::RailEnable { enabled: true, .. },
            DriverOperation::ConfigureRail { .. },
            DriverOperation::VerifyRail { .. },
            DriverOperation::ChannelGate { enabled: true, .. },
            DriverOperation::VerifyOutput { .. }
        ]
    ));
}

#[test]
fn ch5_exposure_is_visible_to_protection_during_final_settle() {
    let mut state = enabling_state(4, 9);
    let mut executor = PowerExecutor::new(MockDriver::default(), 100);

    assert!(executor.submit(&state, enable_effect(4, 9)).is_none());
    assert_eq!(executor.calls.len(), 2);
    assert!(!executor.ch5_oe);

    let exposure = executor.service(150, &state).unwrap();
    assert!(matches!(
        exposure,
        Action::OutputEnergized {
            channel: 4,
            operation: 9
        }
    ));
    state = AppReducer::reduce(&state, exposure);
    assert!(state.channels[4].physical_enabled);
    assert!(state.channels[4].transition == OutputTransition::Enabling(9));
    assert!(executor.ch5_oe);
    assert!(executor.service(199, &state).is_none());

    assert!(matches!(
        executor.service(200, &state),
        Some(Action::OutputApplied {
            channel: 4,
            operation: 9,
            enabled: true
        })
    ));
}

#[test]
fn safety_state_change_preempts_a_due_energizing_stage() {
    let mut state = enabling_state(0, 11);
    let mut executor = PowerExecutor::new(MockDriver::default(), 0);
    assert!(executor.submit(&state, enable_effect(0, 11)).is_none());

    state.pd_contract = None;
    let completion = executor.service(50, &state);

    assert!(matches!(
        completion,
        Some(Action::OutputFailed {
            channel: 0,
            operation: 11,
            fault: Fault::Hardware
        })
    ));
    assert!(!executor.gates[0]);
    assert!(!executor.rail_enabled[0]);
    assert!(!executor.is_busy());
}

#[test]
fn global_shutdown_cancels_a_delayed_enable() {
    let state = enabling_state(0, 1);
    let mut executor = PowerExecutor::new(MockDriver::default(), 0);
    assert!(executor.submit(&state, enable_effect(0, 1)).is_none());
    assert!(executor.is_busy());

    assert!(execute_global_shutdown(&mut executor).is_ok());

    let calls_after_shutdown = executor.calls.len();
    assert!(!executor.is_busy());
    assert!(executor.service(1_000, &state).is_none());
    assert_eq!(executor.calls.len(), calls_after_shutdown);
    assert!(executor.safe());
}

#[test]
fn a_second_enable_is_rejected_without_bypassing_settle_stages() {
    let mut state = enabling_state(0, 1);
    state.channels[2].requested_enabled = true;
    state.channels[2].transition = OutputTransition::Enabling(2);
    let mut executor = PowerExecutor::new(MockDriver::default(), 0);
    assert!(executor.submit(&state, enable_effect(0, 1)).is_none());

    let second = executor.submit(&state, enable_effect(2, 2));

    assert!(matches!(
        second,
        Some(Action::OutputFailed {
            channel: 2,
            operation: 2,
            fault: Fault::Hardware
        })
    ));
    assert!(!executor.gates[2]);
    assert!(!executor.rail_enabled[1]);
    assert!(executor.is_busy());
}

#[test]
fn reducer_serializes_output_enable_requests_before_the_executor_boundary() {
    let state = enabling_state(0, 1);
    let next = AppReducer::reduce(
        &state,
        Action::SetOutputRequested {
            channel: 2,
            enabled: true,
        },
    );

    assert!(!next.channels[2].requested_enabled);
    assert!(next.channels[2].transition == OutputTransition::Stable);
}

#[test]
fn stale_exposure_actions_cannot_mark_an_output_physical() {
    let state = enabling_state(4, 3);
    let stale = AppReducer::reduce(
        &state,
        Action::OutputEnergized {
            channel: 4,
            operation: 2,
        },
    );

    assert!(!stale.channels[4].physical_enabled);
    assert_eq!(FirmwareEffectPlanner::plan(&state, &stale), None);
}

fn run_executor_to_terminal(
    executor: &mut PowerExecutor<MockDriver>,
    state: &mut AppState,
    effect: PowerEffect,
) -> Action {
    if let Some(action) = executor.submit(state, effect) {
        return action;
    }
    for now in [50, 100, 150] {
        if let Some(action) = executor.service(now, state) {
            if matches!(action, Action::OutputEnergized { .. }) {
                *state = AppReducer::reduce(state, action);
            } else {
                return action;
            }
        }
    }
    panic!("power sequence did not terminate");
}

#[test]
fn every_staged_enable_failure_still_shuts_the_target_down() {
    for (channel, stages) in [(0, 6), (4, 7)] {
        for fail_at in 0..stages {
            let mut state = enabling_state(channel, 17);
            let driver = MockDriver {
                fail_at: Some(fail_at),
                ..MockDriver::default()
            };
            let mut executor = PowerExecutor::new(driver, 0);

            let action =
                run_executor_to_terminal(&mut executor, &mut state, enable_effect(channel, 17));

            assert!(matches!(
                action,
                Action::OutputFailed {
                    channel: failed_channel,
                    operation: 17,
                    ..
                } if failed_channel == channel
            ));
            assert!(executor.safe(), "channel {channel}, stage {fail_at}");
            if channel == 4 {
                assert!(!executor.ch5_enable);
                assert!(!executor.ch5_oe);
            } else {
                assert!(!executor.gates[usize::from(channel)]);
                assert!(!executor.rail_enabled[0]);
            }
        }
    }
}

#[test]
fn ch5_disable_treats_oe_failure_as_best_effort_when_en_goes_low() {
    let mut state = eligible_state();
    state.channels[4].physical_enabled = true;
    state.channels[4].requested_enabled = false;
    state.channels[4].transition = OutputTransition::Disabling(4);
    let driver = MockDriver {
        fail_at: Some(0),
        ch5_enable: true,
        ch5_configured: true,
        ch5_oe: true,
        ..MockDriver::default()
    };
    let mut executor = PowerExecutor::new(driver, 0);

    let completion = executor.submit(
        &state,
        PowerEffect::Output {
            channel: 4,
            operation: 4,
            enabled: false,
        },
    );

    assert!(matches!(
        completion,
        Some(Action::OutputApplied {
            channel: 4,
            operation: 4,
            enabled: false
        })
    ));
    assert!(!executor.ch5_enable);
    assert!(!executor.ch5_oe);
}

#[test]
fn sink_limit_requires_three_consecutive_overcurrent_samples() {
    let mut state = eligible_state();
    state.channels[0].requested_enabled = true;
    state.channels[0].physical_enabled = true;
    state.sink_current_limit_ma = 1_000;
    let over = Measurement {
        millivolts: 5_000,
        milliamps: 1_001,
        valid: true,
    };
    let nominal = Measurement {
        millivolts: 5_000,
        milliamps: 1_000,
        valid: true,
    };
    let mut monitor = SinkProtectionMonitor::default();

    assert_eq!(monitor.observe(&state, over), None);
    assert_eq!(monitor.observe(&state, nominal), None);
    assert_eq!(monitor.observe(&state, over), None);
    assert_eq!(monitor.observe(&state, over), None);
    assert_eq!(
        monitor.observe(&state, over),
        Some(SinkProtectionEvent::Trip(Fault::OverCurrent))
    );
}

#[test]
fn negotiated_current_caps_a_higher_user_sink_limit() {
    let mut state = eligible_state();
    state.channels[0].physical_enabled = true;
    state.sink_current_limit_ma = 5_000;
    state.pd_contract.as_mut().unwrap().operating_milliamps = 1_500;
    let over_contract = Measurement {
        millivolts: 5_000,
        milliamps: 1_501,
        valid: true,
    };
    let mut monitor = SinkProtectionMonitor::default();

    assert_eq!(monitor.observe(&state, over_contract), None);
    assert_eq!(monitor.observe(&state, over_contract), None);
    assert_eq!(
        monitor.observe(&state, over_contract),
        Some(SinkProtectionEvent::Trip(Fault::OverCurrent))
    );
}

#[test]
fn sink_voltage_must_track_the_negotiated_contract() {
    for millivolts in [3_999, 6_001] {
        let mut state = eligible_state();
        state.channels[0].physical_enabled = true;
        let outside = Measurement {
            millivolts,
            milliamps: 500,
            valid: true,
        };
        let mut monitor = SinkProtectionMonitor::default();

        assert_eq!(monitor.observe(&state, outside), None);
        assert_eq!(monitor.observe(&state, outside), None);
        assert_eq!(
            monitor.observe(&state, outside),
            Some(SinkProtectionEvent::Trip(Fault::Hardware))
        );
    }
}

#[test]
fn gross_sink_overcurrent_trips_without_confirmation_delay() {
    let mut state = eligible_state();
    state.channels[0].physical_enabled = true;
    state.sink_current_limit_ma = 1_000;
    let gross = Measurement {
        millivolts: 5_000,
        milliamps: 1_351,
        valid: true,
    };

    assert_eq!(
        SinkProtectionMonitor::default().observe(&state, gross),
        Some(SinkProtectionEvent::Trip(Fault::OverCurrent))
    );
}

#[test]
fn sink_fault_recovery_requires_a_valid_contract_voltage() {
    let mut state = eligible_state();
    state.sink_fault = Fault::Hardware;
    let wrong_voltage = Measurement {
        millivolts: 3_999,
        milliamps: 500,
        valid: true,
    };
    let mut monitor = SinkProtectionMonitor::default();

    for _ in 0..SINK_RECOVERY_SAMPLES {
        assert_eq!(monitor.observe(&state, wrong_voltage), None);
    }
    state.pd_contract = None;
    let nominal = Measurement {
        millivolts: 5_000,
        ..wrong_voltage
    };
    for _ in 0..SINK_RECOVERY_SAMPLES {
        assert_eq!(monitor.observe(&state, nominal), None);
    }
}

#[test]
fn sink_sensor_failure_trips_only_while_an_output_is_active() {
    let state = eligible_state();
    let invalid = Measurement {
        millivolts: 0,
        milliamps: 0,
        valid: false,
    };
    let mut monitor = SinkProtectionMonitor::default();
    assert_eq!(monitor.observe(&state, invalid), None);

    let mut active = state;
    active.channels[2].requested_enabled = true;
    assert_eq!(
        monitor.observe(&active, invalid),
        Some(SinkProtectionEvent::Trip(Fault::Sensor))
    );
}

#[test]
fn sink_fault_recovers_only_after_outputs_are_off_and_input_is_stably_safe() {
    let mut state = eligible_state();
    state.sink_fault = Fault::OverCurrent;
    state.channels[0].physical_enabled = true;
    let safe = Measurement {
        millivolts: 5_000,
        milliamps: 500,
        valid: true,
    };
    let mut monitor = SinkProtectionMonitor::default();
    for _ in 0..SINK_RECOVERY_SAMPLES {
        assert_eq!(monitor.observe(&state, safe), None);
    }

    state.channels[0].physical_enabled = false;
    for _ in 1..SINK_RECOVERY_SAMPLES {
        assert_eq!(monitor.observe(&state, safe), None);
    }
    assert_eq!(
        monitor.observe(&state, safe),
        Some(SinkProtectionEvent::Recovered)
    );
}

#[test]
fn latched_sink_fault_blocks_output_enable() {
    let mut state = eligible_state();
    state.sink_fault = Fault::OverCurrent;
    assert_eq!(
        run_enable(&mut MockDriver::default(), &state, 0),
        Err(Fault::OverCurrent)
    );
}

#[test]
fn missing_pd_contract_blocks_output_enable() {
    let mut state = eligible_state();
    state.pd_contract = None;
    assert_eq!(
        run_enable(&mut MockDriver::default(), &state, 0),
        Err(Fault::Hardware)
    );
}

#[test]
fn ch5_uses_forced_pwm_only_for_awg_start() {
    let mut dc = eligible_state();
    let mut driver = MockDriver::default();
    assert!(run_enable(&mut driver, &dc, 4).is_ok());
    assert!(driver.calls.iter().any(|operation| matches!(
        operation,
        DriverOperation::ConfigureCh5 {
            forced_pwm: false,
            ..
        }
    )));
    assert_eq!(
        driver
            .calls
            .iter()
            .filter(|operation| matches!(operation, DriverOperation::ClearCh5Status))
            .count(),
        2
    );

    dc.awg.channel = 4;
    dc.awg_status = AwgStatus::Starting;
    let mut driver = MockDriver::default();
    assert!(run_enable(&mut driver, &dc, 4).is_ok());
    assert!(driver.calls.iter().any(|operation| matches!(
        operation,
        DriverOperation::ConfigureCh5 {
            forced_pwm: true,
            ..
        }
    )));
}

#[test]
fn awg_protection_tracks_live_drive_not_saved_dc_setpoint() {
    let mut state = eligible_state();
    state.awg.channel = 4;
    state.awg_status = AwgStatus::Running;
    state.channels[4].setpoint_mv = 12_000;
    state.channels[4].drive_mv = 1_000;
    state.channels[4].physical_enabled = true;
    state.channels[4].requested_enabled = true;

    let projected = protection_output(&state, 4);
    assert_eq!(projected.setpoint_mv, 1_000);
    assert_eq!(projected.drive_mv, 1_000);
    assert!(projected.regulation_mode == RegulationMode::Cv);

    let mut monitor = ProtectionMonitor::default();
    let low = Measurement {
        millivolts: 990,
        milliamps: 0,
        valid: true,
    };
    for _ in 0..100 {
        assert!(monitor.observe(&projected, low).is_none());
    }
}

#[test]
fn awg_policy_ignores_unsynchronized_voltage_but_keeps_overcurrent() {
    let mut state = eligible_state();
    state.awg.channel = 3;
    state.awg_status = AwgStatus::Running;
    state.channels[3].physical_enabled = true;
    state.channels[3].requested_enabled = true;
    state.channels[3].current_limit_ma = 100;
    let output = protection_output(&state, 3);
    let aliased = Measurement {
        millivolts: 5_000,
        milliamps: 0,
        valid: true,
    };
    let mut monitor = ProtectionMonitor::default();
    for _ in 0..100 {
        assert!(monitor
            .observe_with_voltage_tracking(&output, aliased, false)
            .is_none());
    }

    let overcurrent = Measurement {
        millivolts: 500,
        milliamps: 101,
        valid: true,
    };
    assert!(monitor
        .observe_with_voltage_tracking(&output, overcurrent, false)
        .is_none());
    assert!(monitor
        .observe_with_voltage_tracking(&output, overcurrent, false)
        .is_none());
    assert!(matches!(
        monitor.observe_with_voltage_tracking(&output, overcurrent, false),
        Some(Fault::OverCurrent)
    ));
}

fn requested(state: &AppState, channel: u8, enabled: bool) -> AppState {
    AppReducer::reduce(state, Action::SetOutputRequested { channel, enabled })
}

#[test]
fn stale_hardware_completion_cannot_change_state() {
    let initial = eligible_state();
    let enabling = requested(&initial, 0, true);
    let cancelled = requested(&enabling, 0, false);
    let stale = AppReducer::reduce(
        &cancelled,
        Action::OutputApplied {
            channel: 0,
            operation: 1,
            enabled: true,
        },
    );
    assert!(stale == cancelled);
    assert!(!stale.channels[0].physical_enabled);
}

#[test]
fn configured_planner_covers_power_navigation_and_no_effect_transitions() {
    let old = eligible_state();
    let enabling = requested(&old, 0, true);
    assert_eq!(
        FirmwareEffectPlanner::plan(&old, &enabling),
        Some(FirmwareEffect {
            power: effect_for_transition(&old, &enabling),
            global_shutdown: false,
        })
    );

    let mut awg = old;
    awg.screen = crate::app::Screen::Awg;
    assert_eq!(
        FirmwareEffectPlanner::plan(&old, &awg),
        Some(FirmwareEffect {
            power: None,
            global_shutdown: true,
        })
    );

    let mut temperature = old;
    temperature.temp_sixteenths_c += 1;
    assert_eq!(FirmwareEffectPlanner::plan(&old, &temperature), None);

    let mut sink_trip = old;
    sink_trip.sink_fault = Fault::OverCurrent;
    assert_eq!(
        FirmwareEffectPlanner::plan(&old, &sink_trip),
        Some(FirmwareEffect {
            power: None,
            global_shutdown: true,
        })
    );

    let mut active = old;
    active.channels[0].requested_enabled = true;
    active.channels[0].physical_enabled = true;
    let mut lower_contract_limit = active;
    lower_contract_limit.sink_current_limit_ma -= 10;
    assert_eq!(
        FirmwareEffectPlanner::plan(&active, &lower_contract_limit),
        Some(FirmwareEffect {
            power: None,
            global_shutdown: true,
        })
    );
}

#[test]
fn ordinary_screen_navigation_preserves_an_active_output_without_power_effects() {
    let mut overview = eligible_state();
    overview.screen = crate::app::Screen::Overview;
    overview.channels[0].requested_enabled = true;
    overview.channels[0].physical_enabled = true;

    let detail = AppReducer::reduce(&overview, Action::NextScreen);
    assert!(detail.screen == crate::app::Screen::Channel(0));
    assert!(detail.channels[0].requested_enabled);
    assert!(detail.channels[0].physical_enabled);
    assert_eq!(detail.channels[0].fault, Fault::None);
    assert_eq!(FirmwareEffectPlanner::plan(&overview, &detail), None);

    let returned = AppReducer::reduce(&detail, Action::PreviousScreen);
    assert!(returned.screen == crate::app::Screen::Overview);
    assert!(returned.channels == overview.channels);
    assert_eq!(FirmwareEffectPlanner::plan(&detail, &returned), None);
}

#[test]
fn active_pd_contract_loss_plans_global_shutdown_before_any_power_effect() {
    let mut active = eligible_state();
    active.channels[0].requested_enabled = true;
    active.channels[0].physical_enabled = true;

    let lost = AppReducer::reduce(&active, Action::PdFailed(crate::pd::PdError::Detached));
    assert_eq!(lost.pd_contract, None);
    assert_eq!(lost.sink_fault, Fault::Hardware);
    assert_eq!(
        FirmwareEffectPlanner::plan(&active, &lost),
        Some(FirmwareEffect {
            power: None,
            global_shutdown: true,
        })
    );
}

#[test]
fn every_single_driver_failure_fails_closed() {
    for channel in 0..5 {
        let state = requested(&eligible_state(), channel, true);
        let effect = effect_for_transition(&eligible_state(), &state).unwrap();
        let mut successful = MockDriver::default();
        let _ = execute_effect(&mut successful, &state, effect);
        let call_count = successful.calls.len();

        for failure in 0..call_count {
            let mut driver = MockDriver {
                fail_at: Some(failure),
                ..MockDriver::default()
            };
            let action = execute_effect(&mut driver, &state, effect);
            let final_state = AppReducer::reduce(&state, action);
            assert!(driver.safe());
            assert!(!final_state.channels[usize::from(channel)].physical_enabled);
            assert!(!final_state.channels[usize::from(channel)].requested_enabled);
        }
    }
}

#[test]
fn randomized_requests_and_injected_failures_preserve_interlocks() {
    let mut seed = 0x51a7_3c9du32;
    let mut state = eligible_state();
    let mut driver = MockDriver::default();
    for _ in 0..10_000 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let channel = ((seed >> 8) % 5) as u8;
        let enabled = seed & 1 != 0;
        let old = state;
        state = requested(&state, channel, enabled);
        if let Some(effect) = effect_for_transition(&old, &state) {
            driver.fail_at = if seed & 0x18 == 0 {
                Some(driver.calls.len())
            } else {
                None
            };
            let completion = execute_effect(&mut driver, &state, effect);
            state = AppReducer::reduce(&state, completion);
        }
        assert!(driver.safe());
    }
}

#[test]
fn global_shutdown_attempts_every_off_control_after_each_possible_failure() {
    for failure in 0..8 {
        let mut driver = MockDriver {
            fail_at: Some(failure),
            rail_enabled: [true; 2],
            rail_configured: [true; 2],
            gates: [true; 4],
            ch5_enable: true,
            ch5_configured: true,
            ch5_oe: true,
            ..MockDriver::default()
        };
        let result = execute_global_shutdown(&mut driver);
        if failure == 4 {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
        assert_eq!(driver.calls.len(), 8);
        assert!(driver.safe());
    }
}

#[test]
fn setting_edits_are_pure_when_off_and_emit_hardware_effects_when_on() {
    let mut state = eligible_state();
    state.screen = crate::app::Screen::Channel(4);
    state = AppReducer::reduce(&state, Action::NextControl);
    state = AppReducer::reduce(&state, Action::NextControl);
    assert!(state.focus == crate::app::ControlFocus::Voltage);

    let old = state;
    state = AppReducer::reduce(&state, Action::AdjustFocused(1));
    assert_eq!(state.channels[4].setpoint_mv, 12_010);
    assert!(effect_for_transition(&old, &state).is_none());

    state = AppReducer::reduce(
        &state,
        Action::SetOutputRequested {
            channel: 4,
            enabled: true,
        },
    );
    let enable = effect_for_transition(&old, &state).unwrap();
    let mut driver = MockDriver::default();
    state = AppReducer::reduce(&state, execute_effect(&mut driver, &state, enable));
    assert!(state.channels[4].physical_enabled);

    let old = state;
    state = AppReducer::reduce(&state, Action::AdjustFocused(5));
    assert_eq!(state.channels[4].setpoint_mv, 12_060);
    assert_eq!(state.channels[4].drive_mv, 12_010);
    assert!(effect_for_transition(&old, &state).is_none());

    let old = state;
    state = AppReducer::reduce(
        &state,
        Action::RegulateChannel {
            channel: 4,
            measurement: Measurement {
                millivolts: 12_010,
                milliamps: 0,
                valid: true,
            },
        },
    );
    assert_eq!(state.channels[4].drive_mv, 12_060);
    assert_eq!(
        effect_for_transition(&old, &state),
        Some(PowerEffect::Voltage {
            channel: 4,
            millivolts: 12_060
        })
    );
}

#[test]
fn usb_pd_input_limit_edit_is_bounded_and_has_no_direct_bus_effect() {
    let mut state = eligible_state();
    state.screen = crate::app::Screen::UsbPdInput;

    state = AppReducer::reduce(&state, Action::NextControl);
    assert!(state.focus == crate::app::ControlFocus::CurrentLimit);

    let old = state;
    state = AppReducer::reduce(&state, Action::AdjustFocused(-5));
    assert_eq!(state.sink_current_limit_ma, 4_950);
    assert!(effect_for_transition(&old, &state).is_none());

    state.sink_current_limit_ma = 4_990;
    state = AppReducer::reduce(&state, Action::AdjustFocused(127));
    assert_eq!(state.sink_current_limit_ma, 5_000);
    state.sink_current_limit_ma = 10;
    state = AppReducer::reduce(&state, Action::AdjustFocused(-127));
    assert_eq!(state.sink_current_limit_ma, 0);

    state = AppReducer::reduce(&state, Action::NextControl);
    assert!(state.focus == crate::app::ControlFocus::None);
}

#[test]
fn overview_clicks_cycle_output_toggles_then_restore_screen_navigation() {
    let mut state = eligible_state();
    state.screen = crate::app::Screen::Overview;
    assert!(state.screen == crate::app::Screen::Overview);
    for channel in 0..5u8 {
        state = AppReducer::reduce(&state, Action::NextControl);
        assert!(state.focus == crate::app::ControlFocus::OverviewOutput(channel));
    }
    state = AppReducer::reduce(&state, Action::NextControl);
    assert!(state.focus == crate::app::ControlFocus::None);

    state = AppReducer::reduce(&state, Action::NextControl);
    state = AppReducer::reduce(&state, Action::GoOverview);
    assert!(state.screen == crate::app::Screen::Overview);
    assert!(state.focus == crate::app::ControlFocus::None);
}

#[test]
fn ch5_cc_reducer_drives_voltage_side_effects_and_fails_closed() {
    let mut state = eligible_state();
    state.channels[4].current_limit_ma = 720;

    let off = state;
    state = AppReducer::reduce(
        &state,
        Action::SetRegulationMode {
            channel: 4,
            mode: RegulationMode::Cc,
        },
    );
    assert!(state.channels[4].regulation_mode == RegulationMode::Cc);
    assert_eq!(state.channels[4].current_limit_ma, 720);
    assert!(effect_for_transition(&off, &state).is_none());

    let old = state;
    state = AppReducer::reduce(
        &state,
        Action::SetOutputRequested {
            channel: 4,
            enabled: true,
        },
    );
    let mut driver = MockDriver::default();
    let enable = effect_for_transition(&old, &state).unwrap();
    state = AppReducer::reduce(&state, execute_effect(&mut driver, &state, enable));
    assert!(state.channels[4].physical_enabled);
    assert!(driver.calls.iter().any(|operation| matches!(
        operation,
        DriverOperation::ConfigureCh5 {
            current_limit_ma: 3_000,
            ..
        }
    )));

    let old = state;
    state = AppReducer::reduce(
        &state,
        Action::SetCurrentLimit {
            channel: 4,
            milliamps: 500,
        },
    );
    assert!(effect_for_transition(&old, &state).is_none());

    let old = state;
    state = AppReducer::reduce(
        &state,
        Action::RegulateChannel {
            channel: 4,
            measurement: Measurement {
                millivolts: 12_000,
                milliamps: 600,
                valid: true,
            },
        },
    );
    assert_eq!(state.channels[4].drive_mv, 11_800);
    assert_eq!(
        effect_for_transition(&old, &state),
        Some(PowerEffect::Voltage {
            channel: 4,
            millivolts: 11_800,
        })
    );
    let effect = effect_for_transition(&old, &state).unwrap();
    let completion = execute_effect(&mut driver, &state, effect);
    state = AppReducer::reduce(&state, completion);
    assert!(matches!(
        driver.calls.last(),
        Some(DriverOperation::Ch5Voltage(11_800))
    ));

    let old = state;
    let failed = AppReducer::reduce(
        &state,
        Action::RegulateChannel {
            channel: 4,
            measurement: Measurement {
                millivolts: 11_800,
                milliamps: 800,
                valid: true,
            },
        },
    );
    driver.fail_at = Some(driver.calls.len());
    let effect = effect_for_transition(&old, &failed).unwrap();
    let completion = execute_effect(&mut driver, &failed, effect);
    let failed = AppReducer::reduce(&failed, completion);
    assert!(driver.safe());
    assert!(!failed.channels[4].physical_enabled);
    assert!(!failed.channels[4].requested_enabled);
    assert!(failed.channels[4].fault == Fault::Hardware);
}

#[test]
fn ch4_cc_reducer_drives_dac_side_effects_and_fails_closed() {
    let mut state = eligible_state();
    state.channels[3].requested_enabled = true;
    state.channels[3].physical_enabled = true;
    state.channels[3].setpoint_mv = 5_000;
    state.channels[3].drive_mv = 5_000;
    state.channels[3].current_limit_ma = 500;
    state.channels[3].regulation_mode = RegulationMode::Cc;

    let old = state;
    state = AppReducer::reduce(
        &state,
        Action::RegulateChannel {
            channel: 3,
            measurement: Measurement {
                millivolts: 5_000,
                milliamps: 600,
                valid: true,
            },
        },
    );
    assert_eq!(state.channels[3].drive_mv, 4_800);
    let effect = effect_for_transition(&old, &state).unwrap();
    assert_eq!(
        effect,
        PowerEffect::Voltage {
            channel: 3,
            millivolts: 4_800,
        }
    );

    let mut driver = MockDriver::default();
    let completion = execute_effect(&mut driver, &state, effect);
    state = AppReducer::reduce(&state, completion);
    assert!(matches!(
        driver.calls.last(),
        Some(DriverOperation::SetAdjustableDac { millivolts: 4_800 })
    ));

    let old = state;
    let failed = AppReducer::reduce(
        &state,
        Action::RegulateChannel {
            channel: 3,
            measurement: Measurement {
                millivolts: 4_800,
                milliamps: 800,
                valid: true,
            },
        },
    );
    driver.fail_at = Some(driver.calls.len());
    let completion = execute_effect(
        &mut driver,
        &failed,
        effect_for_transition(&old, &failed).unwrap(),
    );
    let failed = AppReducer::reduce(&failed, completion);
    assert!(driver.safe());
    assert!(!failed.channels[3].physical_enabled);
    assert!(!failed.channels[3].requested_enabled);
    assert!(failed.channels[3].fault == Fault::Hardware);
}

#[test]
fn ch5_cc_protection_allows_voltage_droop_but_catches_runaway_current() {
    let mut state = eligible_state();
    let output = &mut state.channels[4];
    output.requested_enabled = true;
    output.physical_enabled = true;
    output.setpoint_mv = 12_000;
    output.current_limit_ma = 500;
    output.regulation_mode = RegulationMode::Cc;
    let regulating = Measurement {
        millivolts: 8_000,
        milliamps: 600,
        valid: true,
    };
    let runaway = Measurement {
        millivolts: 8_000,
        milliamps: 800,
        valid: true,
    };
    let mut monitor = ProtectionMonitor::default();
    for _ in 0..STARTUP_GRACE_SAMPLES {
        assert!(monitor.observe(output, regulating).is_none());
    }
    for _ in 0..20 {
        assert!(monitor.observe(output, regulating).is_none());
    }
    assert!(monitor.observe(output, runaway).is_none());
    assert!(monitor.observe(output, runaway).is_none());
    assert!(monitor.observe(output, runaway) == Some(Fault::OverCurrent));
}

#[test]
fn ch5_cc_controller_converges_on_a_resistive_load() {
    let mut state = eligible_state();
    state.channels[4].requested_enabled = true;
    state.channels[4].physical_enabled = true;
    state.channels[4].setpoint_mv = 12_000;
    state.channels[4].drive_mv = 12_000;
    state.channels[4].current_limit_ma = 500;
    state.channels[4].regulation_mode = RegulationMode::Cc;

    for _ in 0..100 {
        let current_ma = state.channels[4].drive_mv / 20;
        state = AppReducer::reduce(
            &state,
            Action::RegulateChannel {
                channel: 4,
                measurement: Measurement {
                    millivolts: state.channels[4].drive_mv,
                    milliamps: current_ma,
                    valid: true,
                },
            },
        );
        assert!((800..=12_000).contains(&state.channels[4].drive_mv));
    }
    assert!((496..=504).contains(&(state.channels[4].drive_mv / 20)));
}

#[test]
fn ch5_cc_constant_power_instability_reaches_the_runaway_interlock() {
    let mut state = eligible_state();
    state.channels[4].requested_enabled = true;
    state.channels[4].physical_enabled = true;
    state.channels[4].setpoint_mv = 12_000;
    state.channels[4].drive_mv = 12_000;
    state.channels[4].current_limit_ma = 500;
    state.channels[4].regulation_mode = RegulationMode::Cc;
    let mut monitor = ProtectionMonitor::default();
    let mut tripped = false;

    for _ in 0..100 {
        let voltage_mv = state.channels[4].drive_mv;
        let measurement = Measurement {
            millivolts: voltage_mv,
            milliamps: (7_000_000u32 / u32::from(voltage_mv)) as u16,
            valid: true,
        };
        if monitor.observe(&state.channels[4], measurement) == Some(Fault::OverCurrent) {
            tripped = true;
            break;
        }
        state = AppReducer::reduce(
            &state,
            Action::RegulateChannel {
                channel: 4,
                measurement,
            },
        );
    }
    assert!(tripped);
}

#[test]
fn protection_rejects_startup_transients_and_rearms_after_disable() {
    let mut state = eligible_state();
    let output = &mut state.channels[4];
    output.requested_enabled = true;
    output.physical_enabled = true;
    output.setpoint_mv = 12_000;
    output.current_limit_ma = 400;
    let overload = Measurement {
        millivolts: 2_000,
        milliamps: 900,
        valid: true,
    };
    let nominal = Measurement {
        millivolts: 12_000,
        milliamps: 0,
        valid: true,
    };
    let mut monitor = ProtectionMonitor::default();

    for _ in 0..STARTUP_GRACE_SAMPLES {
        assert!(monitor.observe(output, overload).is_none());
    }
    assert!(monitor.observe(output, overload).is_none());
    assert!(monitor.observe(output, nominal).is_none());
    assert!(monitor.observe(output, overload).is_none());
    assert!(monitor.observe(output, overload).is_none());
    assert!(monitor.observe(output, overload) == Some(Fault::OverCurrent));
    let tripped = monitor.snapshot();
    assert!(tripped.trip == overload);
    assert_eq!(tripped.peak_milliamps, overload.milliamps);

    output.physical_enabled = false;
    assert!(monitor.observe(output, overload).is_none());
    assert!(monitor.snapshot().trip == overload);
    output.physical_enabled = true;
    for _ in 0..STARTUP_GRACE_SAMPLES {
        assert!(monitor.observe(output, nominal).is_none());
    }
    assert!(monitor.observe(output, nominal).is_none());
}

#[test]
fn voltage_edits_get_bounded_settling_without_weakening_current_protection() {
    let mut state = eligible_state();
    let output = &mut state.channels[3];
    output.requested_enabled = true;
    output.physical_enabled = true;
    output.setpoint_mv = 5_000;
    output.drive_mv = 5_000;
    output.current_limit_ma = 500;
    output.regulation_mode = RegulationMode::Cc;
    let nominal = Measurement {
        millivolts: 5_000,
        milliamps: 500,
        valid: true,
    };
    let lagging_voltage = Measurement {
        millivolts: 3_376,
        milliamps: 500,
        valid: true,
    };

    let mut monitor = ProtectionMonitor::default();
    for _ in 0..=STARTUP_GRACE_SAMPLES {
        assert!(monitor.observe(output, nominal).is_none());
    }
    output.setpoint_mv = 2_770;
    output.drive_mv = 2_770;
    for _ in 0..VOLTAGE_SETTING_SETTLE_SAMPLES {
        assert!(monitor.observe(output, lagging_voltage).is_none());
    }
    assert!(monitor.observe(output, lagging_voltage).is_none());
    assert!(monitor.observe(output, lagging_voltage).is_none());
    assert!(monitor.observe(output, lagging_voltage) == Some(Fault::Hardware));

    let mut monitor = ProtectionMonitor::default();
    output.setpoint_mv = 5_000;
    output.drive_mv = 5_000;
    for _ in 0..=STARTUP_GRACE_SAMPLES {
        assert!(monitor.observe(output, nominal).is_none());
    }
    output.setpoint_mv = 2_770;
    output.drive_mv = 2_770;
    let overload = Measurement {
        millivolts: 3_376,
        milliamps: 800,
        valid: true,
    };
    assert!(monitor.observe(output, overload).is_none());
    assert!(monitor.observe(output, overload).is_none());
    assert!(monitor.observe(output, overload) == Some(Fault::OverCurrent));
}

#[test]
fn output_toggle_retries_a_fault_then_toggles_back_off() {
    let mut state = eligible_state();
    state.channels[4].fault = Fault::OverCurrent;
    state.channels[4].current_limit_ma = 410;

    let old = state;
    state = AppReducer::reduce(&state, Action::ToggleOutputRequested { channel: 4 });
    assert!(state.channels[4].requested_enabled);
    assert!(state.channels[4].fault == Fault::None);
    let enable = effect_for_transition(&old, &state).unwrap();
    let mut driver = MockDriver::default();
    state = AppReducer::reduce(&state, execute_effect(&mut driver, &state, enable));
    assert!(state.channels[4].physical_enabled);
    assert!(driver.calls.iter().any(|operation| matches!(
        operation,
        DriverOperation::ConfigureCh5 {
            current_limit_ma: 3_000,
            ..
        }
    )));

    let old = state;
    state = AppReducer::reduce(
        &state,
        Action::SetCurrentLimit {
            channel: 4,
            milliamps: 420,
        },
    );
    assert_eq!(state.channels[4].current_limit_ma, 420);
    assert!(effect_for_transition(&old, &state).is_none());

    let old = state;
    state = AppReducer::reduce(&state, Action::ToggleOutputRequested { channel: 4 });
    assert!(!state.channels[4].requested_enabled);
    assert!(matches!(
        effect_for_transition(&old, &state),
        Some(PowerEffect::Output { enabled: false, .. })
    ));
}

#[test]
fn startup_grace_still_trips_a_gross_overcurrent_immediately() {
    let mut state = eligible_state();
    let output = &mut state.channels[0];
    output.physical_enabled = true;
    output.current_limit_ma = 3_000;
    let gross_overcurrent = Measurement {
        millivolts: output.setpoint_mv,
        milliamps: HARD_OVERCURRENT_CEILING_MA + 1,
        valid: true,
    };

    let mut monitor = ProtectionMonitor::default();
    assert_eq!(
        monitor.observe(output, gross_overcurrent),
        Some(Fault::OverCurrent)
    );
    assert!(monitor.snapshot().trip == gross_overcurrent);
}

#[test]
fn disabling_the_last_sibling_turns_off_its_shared_rail() {
    let mut state = eligible_state();
    state.channels[0].physical_enabled = true;
    let mut driver = MockDriver::default();

    assert!(run_disable(&mut driver, &state, 0).is_ok());
    assert_eq!(
        driver.calls.as_slice(),
        &[
            DriverOperation::ChannelGate {
                channel: 0,
                enabled: false,
            },
            DriverOperation::RailEnable {
                rail: Rail::Dc1,
                enabled: false,
            },
        ]
    );
}

#[test]
fn disabling_one_sibling_keeps_a_shared_rail_alive_for_the_other() {
    let mut state = eligible_state();
    state.channels[0].physical_enabled = true;
    state.channels[1].physical_enabled = true;
    let mut driver = MockDriver::default();

    assert!(run_disable(&mut driver, &state, 0).is_ok());
    assert_eq!(
        driver.calls.as_slice(),
        &[DriverOperation::ChannelGate {
            channel: 0,
            enabled: false,
        }]
    );
}

#[test]
fn shared_rail_limit_requires_three_summed_overcurrent_samples() {
    let mut state = eligible_state();
    state.channels[0].physical_enabled = true;
    state.channels[1].physical_enabled = true;
    let mut measurements = [Measurement {
        millivolts: 3_000,
        milliamps: 0,
        valid: true,
    }; 5];
    measurements[0].milliamps = 2_500;
    measurements[1].milliamps = 2_500;

    let mut monitor = SharedRailProtectionMonitor::default();
    for _ in 0..3 {
        assert!(monitor.observe(&state, &measurements, Rail::Dc1).is_none());
    }
    measurements[1].milliamps = 2_510;
    assert!(monitor.observe(&state, &measurements, Rail::Dc1).is_none());
    assert!(monitor.observe(&state, &measurements, Rail::Dc1).is_none());
    assert_eq!(
        monitor.observe(&state, &measurements, Rail::Dc1),
        Some(Fault::OverCurrent)
    );
}

#[test]
fn shared_rail_monitor_ignores_inactive_sibling_current_and_fails_closed_active_sensors() {
    let mut state = eligible_state();
    state.channels[0].physical_enabled = true;
    let mut measurements = [Measurement {
        millivolts: 3_000,
        milliamps: 2_900,
        valid: true,
    }; 5];
    measurements[1].milliamps = 6_000;

    let mut monitor = SharedRailProtectionMonitor::default();
    for _ in 0..3 {
        assert!(monitor.observe(&state, &measurements, Rail::Dc1).is_none());
    }
    measurements[0].valid = false;
    assert_eq!(
        monitor.observe(&state, &measurements, Rail::Dc1),
        Some(Fault::Sensor)
    );
}

#[test]
fn reboot_request_is_pure_application_state() {
    let state = eligible_state();
    let next = AppReducer::reduce(&state, Action::RequestReboot);
    assert!(next.reboot_requested);
    assert!(effect_for_transition(&state, &next).is_none());
}
