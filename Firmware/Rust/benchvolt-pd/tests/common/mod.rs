//! Shared host-side harness: a scriptable mock power driver with electrical
//! invariants, and a `Harness` that mirrors src/main.rs's foreground loop
//! (cadence, protection, regulation, staged executor) over simulated time.
#![allow(dead_code)]

use benchvolt_pd::app::{Action, AppReducer, AppState, AwgStatus, Fault, Measurement, Screen};
use benchvolt_pd::cadence::ServiceCadence;
use benchvolt_pd::dispatch::dispatch_app;
use benchvolt_pd::input_policy::{encoder_action, ButtonTracker};
use benchvolt_pd::monitoring::ProtectionService;
use benchvolt_pd::power::{
    execute_global_shutdown, DriverOperation, FirmwareEffectPlanner, PowerDriver, PowerExecutor,
    Rail,
};
use reducto::{EffectApp, View};

pub struct NullView;

impl View for NullView {
    type State = AppState;

    fn render(&mut self, _state: &AppState) {}
}

/// Scripted failure injection for the mock driver.
#[derive(Clone, Copy)]
pub enum FailureMode {
    None,
    /// Fail the operation with this absolute index (0-based over all ops).
    FailAt(usize),
    /// Fail every operation the predicate matches.
    FailMatching(fn(&DriverOperation) -> bool),
    /// Fail every `period`-th operation (1-based within each period).
    Intermittent {
        period: usize,
    },
}

/// Models rails, gates, and the CH5 converter like the reference mock inside
/// src/power.rs's test module, records every operation, and supports scripted
/// failure injection. `safe()` checks the electrical sequencing invariant and
/// `physically_energized()` summarizes whether any output can carry power.
pub struct MockPowerDriver {
    pub ops: Vec<DriverOperation>,
    pub failures: FailureMode,
    pub rail_enabled: [bool; 2],
    pub rail_configured: [bool; 2],
    pub gates: [bool; 4],
    pub ch5_enable: bool,
    pub ch5_configured: bool,
    pub ch5_oe: bool,
    /// Latched when an injected failure hit a de-energizing operation, so the
    /// firmware may truthfully believe an output is off while the hardware is
    /// not. Self-clears once the modeled hardware is fully de-energized.
    pub disable_op_failed: bool,
}

impl Default for MockPowerDriver {
    fn default() -> Self {
        Self {
            ops: Vec::new(),
            failures: FailureMode::None,
            rail_enabled: [false; 2],
            rail_configured: [false; 2],
            gates: [false; 4],
            ch5_enable: false,
            ch5_configured: false,
            ch5_oe: false,
            disable_op_failed: false,
        }
    }
}

fn rail_index(rail: Rail) -> usize {
    match rail {
        Rail::Dc1 => 0,
        Rail::Dc2 => 1,
    }
}

fn is_de_energizing(operation: &DriverOperation) -> bool {
    matches!(
        operation,
        DriverOperation::ChannelGate { enabled: false, .. }
            | DriverOperation::RailEnable { enabled: false, .. }
            | DriverOperation::Ch5Enable(false)
            | DriverOperation::Ch5OutputEnable(false)
    )
}

impl MockPowerDriver {
    /// Gates are never on without their configured rail; CH5 OE is never on
    /// without an enabled, configured converter. A closed gate with a dead
    /// rail is de-energized and therefore still safe.
    pub fn safe(&self) -> bool {
        (!self.gates[0] && !self.gates[1] || !self.rail_enabled[0] || self.rail_configured[0])
            && (!self.gates[2] && !self.gates[3]
                || !self.rail_enabled[1]
                || self.rail_configured[1])
            && (!self.ch5_oe || !self.ch5_enable || self.ch5_configured)
    }

    pub fn physically_energized(&self) -> bool {
        (self.rail_enabled[0] && (self.gates[0] || self.gates[1]))
            || (self.rail_enabled[1] && (self.gates[2] || self.gates[3]))
            || (self.ch5_enable && self.ch5_oe)
    }

    pub fn dac_writes(&self) -> Vec<u16> {
        self.ops
            .iter()
            .filter_map(|operation| match operation {
                DriverOperation::SetAdjustableDac { millivolts } => Some(*millivolts),
                _ => None,
            })
            .collect()
    }

    pub fn ch5_voltage_writes(&self) -> Vec<u16> {
        self.ops
            .iter()
            .filter_map(|operation| match operation {
                DriverOperation::Ch5Voltage(millivolts)
                | DriverOperation::Ch5VoltageUnverified(millivolts) => Some(*millivolts),
                _ => None,
            })
            .collect()
    }

    fn injected_failure(&self, index: usize, operation: &DriverOperation) -> bool {
        match self.failures {
            FailureMode::None => false,
            FailureMode::FailAt(at) => index == at,
            FailureMode::FailMatching(matcher) => matcher(operation),
            FailureMode::Intermittent { period } => period != 0 && (index + 1) % period == 0,
        }
    }
}

impl PowerDriver for MockPowerDriver {
    type Error = ();

    fn apply(&mut self, operation: DriverOperation) -> Result<(), Self::Error> {
        let index = self.ops.len();
        self.ops.push(operation);
        if self.injected_failure(index, &operation) {
            // The command never reached the hardware: no state change.
            if is_de_energizing(&operation) {
                self.disable_op_failed = true;
            }
            return Err(());
        }
        let result = match operation {
            DriverOperation::ChannelGate { channel, enabled } => {
                self.gates[usize::from(channel)] = enabled;
                Ok(())
            }
            DriverOperation::RailEnable { rail, enabled } => {
                let index = rail_index(rail);
                self.rail_enabled[index] = enabled;
                if !enabled {
                    self.rail_configured[index] = false;
                }
                Ok(())
            }
            DriverOperation::ConfigureRail { rail, .. } => {
                let index = rail_index(rail);
                if self.rail_enabled[index] {
                    self.rail_configured[index] = true;
                    Ok(())
                } else {
                    Err(())
                }
            }
            DriverOperation::VerifyRail { rail } => {
                let index = rail_index(rail);
                if self.rail_enabled[index] && self.rail_configured[index] {
                    Ok(())
                } else {
                    Err(())
                }
            }
            DriverOperation::Ch5Enable(enabled) => {
                self.ch5_enable = enabled;
                if !enabled {
                    self.ch5_configured = false;
                    self.ch5_oe = false;
                }
                Ok(())
            }
            DriverOperation::ConfigureCh5 { .. } => {
                if self.ch5_enable {
                    self.ch5_configured = true;
                    Ok(())
                } else {
                    Err(())
                }
            }
            DriverOperation::ClearCh5Status => {
                if self.ch5_enable && self.ch5_configured {
                    Ok(())
                } else {
                    Err(())
                }
            }
            DriverOperation::Ch5OutputEnable(enabled) => {
                if enabled && (!self.ch5_enable || !self.ch5_configured) {
                    Err(())
                } else {
                    self.ch5_oe = enabled;
                    Ok(())
                }
            }
            DriverOperation::Ch5Voltage(_) | DriverOperation::Ch5VoltageUnverified(_) => {
                if self.ch5_enable && self.ch5_configured {
                    Ok(())
                } else {
                    Err(())
                }
            }
            DriverOperation::VerifyOutput { channel, .. } => {
                let enabled = if channel == 4 {
                    self.ch5_oe
                } else {
                    self.gates[usize::from(channel)]
                };
                if enabled {
                    Ok(())
                } else {
                    Err(())
                }
            }
            DriverOperation::SetAdjustableDac { .. } => Ok(()),
        };
        if !self.physically_energized() {
            self.disable_op_failed = false;
        }
        result
    }
}

/// Owns the reactive app, the staged power executor over the mock driver, the
/// protection service, and a simulated millisecond clock. `tick` reproduces
/// the src/main.rs foreground loop: 20 ms protection/measurement sampling,
/// 100 ms temperature, 200 ms display measurements, executor servicing, and
/// the AWG start/stop shutdown handshake.
pub struct Harness {
    pub app: EffectApp<AppReducer, NullView, FirmwareEffectPlanner, 8>,
    pub executor: PowerExecutor<MockPowerDriver>,
    pub protection: ProtectionService,
    pub cadence: ServiceCadence,
    pub button: ButtonTracker,
    pub now_ms: u32,
    /// Simulated load current per channel, reported while physically enabled.
    pub load_ma: [u16; 5],
    /// When false, channel ADC frames report invalid (sensor failure).
    pub measurements_valid: bool,
    pub temp: Option<i16>,
    pub sink_mv: u16,
    pub sink_ma: u16,
}

pub fn eligible_contract() -> benchvolt_pd::pd::Contract {
    benchvolt_pd::pd::Contract {
        source_position: 1,
        millivolts: 5_000,
        operating_milliamps: 5_000,
        maximum_milliamps: 5_000,
    }
}

impl Harness {
    /// A booted, output-eligible unit: PD contract negotiated, temperature and
    /// ADC frames valid, everything off.
    pub fn new() -> Self {
        let state = AppState::new(true, Some(25 * 16));
        let mut harness = Self {
            app: EffectApp::new(NullView, state),
            executor: PowerExecutor::new(MockPowerDriver::default(), 0),
            protection: ProtectionService::default(),
            cadence: ServiceCadence::default(),
            button: ButtonTracker::new(true),
            now_ms: 0,
            load_ma: [0; 5],
            measurements_valid: true,
            temp: Some(25 * 16),
            sink_mv: 5_000,
            sink_ma: 0,
        };
        harness.dispatch(Action::PdNegotiated(eligible_contract()));
        harness.dispatch(Action::Temperature(harness.temp));
        let frame = harness.measurement_frame();
        harness.dispatch(Action::Measurements(frame));
        harness
    }

    pub fn state(&self) -> &AppState {
        self.app.state()
    }

    pub fn driver(&self) -> &MockPowerDriver {
        &self.executor
    }

    pub fn driver_mut(&mut self) -> &mut MockPowerDriver {
        &mut self.executor
    }

    fn channel_measurement(&self, channel: usize) -> Measurement {
        let output = &self.state().channels[channel];
        if output.physical_enabled {
            Measurement {
                millivolts: output.drive_mv,
                milliamps: self.load_ma[channel],
                valid: self.measurements_valid,
            }
        } else {
            Measurement {
                millivolts: 0,
                milliamps: 0,
                valid: true,
            }
        }
    }

    pub fn measurement_frame(&self) -> [Measurement; 5] {
        core::array::from_fn(|channel| self.channel_measurement(channel))
    }

    /// Dispatch one action through the canonical dispatch loop (reducer,
    /// effect planner, executor submission, completion feedback) and check
    /// every invariant afterwards.
    pub fn dispatch(&mut self, action: Action) -> bool {
        let before = DriveSnapshot::capture(self.app.state());
        let changed = dispatch_app(&mut self.app, &mut self.executor, action);
        assert_invariants(self);
        assert_bounded_slew(&before, self.app.state());
        changed
    }

    /// Rotate the encoder one detent, mapped through the input policy.
    /// `accelerated` mirrors velocity-scaled detents (|value| >= 8 is a fast
    /// spin); its sign is the direction.
    pub fn detent(&mut self, accelerated: i8) {
        let direction = accelerated.signum();
        if let Some(action) = encoder_action(self.state(), direction, accelerated) {
            self.dispatch(action);
        }
    }

    /// A debounced short click through the button tracker (maps to
    /// `Action::NextControl`).
    pub fn click(&mut self) {
        // Respect the 50 ms debounce window since the previous press.
        self.tick(60);
        let press = self.now_ms as u16;
        if let Some(action) = self.button.sample(press, false) {
            self.dispatch(action);
        }
        self.tick(40);
        let release = self.now_ms as u16;
        if let Some(action) = self.button.sample(release, true) {
            self.dispatch(action);
        }
        self.tick(60);
    }

    fn mirror_awg_lifecycle(&mut self) {
        match self.state().awg_status {
            AwgStatus::StartRequested => {
                if execute_global_shutdown(&mut self.executor).is_ok() {
                    self.dispatch(Action::GlobalShutdownApplied);
                    self.dispatch(Action::AwgStartPrepared);
                } else {
                    self.dispatch(Action::GlobalShutdownFailed);
                }
            }
            AwgStatus::StopRequested => {
                if execute_global_shutdown(&mut self.executor).is_ok() {
                    self.dispatch(Action::GlobalShutdownApplied);
                } else {
                    self.dispatch(Action::GlobalShutdownFailed);
                }
            }
            _ => {}
        }
    }

    /// Advance simulated time millisecond by millisecond, running the same
    /// periodic work as the firmware foreground loop.
    pub fn tick(&mut self, milliseconds: u32) {
        for _ in 0..milliseconds {
            self.now_ms += 1;
            self.mirror_awg_lifecycle();
            let due = self.cadence.advance(1);

            if due.temperature {
                self.dispatch(Action::Temperature(self.temp));
                if let Some(fault) = ProtectionService::temperature_fault(self.temp) {
                    let actions = ProtectionService::temperature_trip_actions(self.state(), fault);
                    for action in actions.into_iter().flatten() {
                        self.dispatch(action);
                    }
                    let _ = execute_global_shutdown(&mut self.executor);
                    assert_invariants(self);
                }
            }

            if due.measurement {
                let measurements = self.measurement_frame();
                let sink = Measurement {
                    millivolts: self.sink_mv,
                    milliamps: self.sink_ma,
                    valid: true,
                };
                for rail in [Rail::Dc1, Rail::Dc2] {
                    let state = *self.app.state();
                    let actions =
                        self.protection
                            .observe_shared_current(&state, &measurements, rail);
                    for action in actions.into_iter().flatten() {
                        self.dispatch(action);
                    }
                }
                let state = *self.app.state();
                if let Some(action) = self.protection.observe_sink(&state, sink) {
                    self.dispatch(action);
                }
                for channel in 0..5u8 {
                    let measurement = measurements[usize::from(channel)];
                    let state = *self.app.state();
                    let action = self
                        .protection
                        .observe_channel(&state, channel, measurement);
                    if let Some(action) = action {
                        self.dispatch(action);
                    }
                }
                for channel in 3..=4u8 {
                    if self.state().awg_status != AwgStatus::Running
                        || channel != self.state().active_awg_channel()
                    {
                        self.dispatch(Action::RegulateChannel {
                            channel,
                            measurement: measurements[usize::from(channel)],
                        });
                    }
                }
            }

            if due.display_measurement {
                let measurements = self.measurement_frame();
                self.dispatch(Action::Measurements(measurements));
                self.dispatch(Action::SinkMeasurement(Measurement {
                    millivolts: self.sink_mv,
                    milliamps: self.sink_ma,
                    valid: true,
                }));
            }

            let now = self.now_ms as u16;
            if let Some(action) = self.executor.service(now, self.app.state()) {
                self.dispatch(action);
            }
            assert_invariants(self);
        }
    }

    /// Directly enable a channel through the USB-shaped action and drive the
    /// staged executor to completion.
    pub fn enable_channel(&mut self, channel: u8) {
        self.dispatch(Action::SetOutputRequested {
            channel,
            enabled: true,
        });
        self.tick(200);
        assert!(self.state().channels[usize::from(channel)].physical_enabled);
    }

    /// Clear every environmental hazard and drive the system to a fully
    /// de-energized, idle state, asserting it is reachable.
    pub fn quiesce(&mut self) {
        self.driver_mut().failures = FailureMode::None;
        self.load_ma = [0; 5];
        self.measurements_valid = true;
        self.temp = Some(25 * 16);
        self.sink_mv = 5_000;
        self.sink_ma = 0;
        if !matches!(
            self.state().awg_status,
            AwgStatus::Stopped | AwgStatus::Fault
        ) {
            self.dispatch(Action::GoMainMenu);
            self.tick(50);
        }
        let ok = execute_global_shutdown(&mut self.executor).is_ok();
        self.dispatch(if ok {
            Action::GlobalShutdownApplied
        } else {
            Action::GlobalShutdownFailed
        });
        self.tick(500);
        assert!(self.state().outputs_physically_off());
        assert!(self.state().output_transitions_stable());
        assert!(!self.executor.is_busy());
        assert!(!self.driver().physically_energized());
        assert!(self.driver().safe());
    }

    /// Navigate from anywhere to a channel screen with the given focus depth
    /// (1 = Output, 2 = Voltage, 3 = RegulationMode for channels 3/4).
    pub fn focus_channel_control(&mut self, channel: u8, clicks: u8) {
        self.dispatch(Action::GoOverview);
        for _ in 0..=channel {
            self.dispatch(Action::NextScreen);
        }
        assert!(self.state().screen == Screen::Channel(channel));
        for _ in 0..clicks {
            self.dispatch(Action::NextControl);
        }
    }
}

/// The single invariant gate used after every dispatch and every tick.
pub fn assert_invariants(harness: &Harness) {
    let state = harness.app.state();
    let driver: &MockPowerDriver = &harness.executor;

    // Reducer coherence: an enabling channel was always requested and had its
    // fault cleared; a latched fault always drops the enable request.
    for (index, output) in state.channels.iter().enumerate() {
        if let benchvolt_pd::app::OutputTransition::Enabling(_) = output.transition {
            assert!(
                output.requested_enabled,
                "channel {index} enabling without a request"
            );
            assert!(
                output.fault == Fault::None,
                "channel {index} enabling with latched fault {:?}",
                output.fault
            );
        }
        if output.fault != Fault::None {
            assert!(
                !output.requested_enabled,
                "channel {index} requested while fault {:?} latched",
                output.fault
            );
        }
    }

    // Electrical sequencing invariant on the modeled hardware.
    assert!(driver.safe(), "mock driver electrical invariant violated");

    // If the reducer claims every output is physically off, the hardware must
    // agree -- unless a de-energizing driver command was deliberately failed,
    // in which case the firmware cannot know (this is exactly the case
    // Action::GlobalShutdownFailed must keep physical_enabled for).
    if state.outputs_physically_off() && !driver.disable_op_failed {
        assert!(
            !driver.physically_energized(),
            "state claims outputs off but hardware is energized"
        );
    }
}

/// Per-channel drive state captured before a dispatch, for behavioral
/// invariants that constrain *transitions* rather than single states.
pub struct DriveSnapshot {
    drive_mv: [u16; 5],
    physical: [bool; 5],
    stable: [bool; 5],
    awg_hot_channel: Option<u8>,
}

impl DriveSnapshot {
    pub fn capture(state: &AppState) -> Self {
        let awg_hot_channel = matches!(
            state.awg_status,
            benchvolt_pd::app::AwgStatus::Starting | benchvolt_pd::app::AwgStatus::Running
        )
        .then(|| state.active_awg_channel());
        Self {
            drive_mv: core::array::from_fn(|index| state.channels[index].drive_mv),
            physical: core::array::from_fn(|index| state.channels[index].physical_enabled),
            stable: core::array::from_fn(|index| {
                state.channels[index].transition
                    == benchvolt_pd::app::OutputTransition::Stable
            }),
            awg_hot_channel,
        }
    }
}

/// INVARIANT (bounded slew): while a channel is physically enabled and not
/// owned by a starting/running AWG, no single dispatch may move its physical
/// drive by more than one 200 mV control step. Every reducer arm - present
/// or future - that touches `drive_mv` is checked here on every fuzz and
/// integration dispatch.
pub fn assert_bounded_slew(before: &DriveSnapshot, after: &AppState) {
    const SLEW_STEP_MV: u16 = 200;
    let awg_hot_after = matches!(
        after.awg_status,
        benchvolt_pd::app::AwgStatus::Starting | benchvolt_pd::app::AwgStatus::Running
    )
    .then(|| after.active_awg_channel());
    for channel in 0..5usize {
        let output = &after.channels[channel];
        let guarded = before.physical[channel]
            && output.physical_enabled
            && before.stable[channel]
            && output.transition == benchvolt_pd::app::OutputTransition::Stable
            && before.awg_hot_channel != Some(channel as u8)
            && awg_hot_after != Some(channel as u8);
        if guarded {
            let delta = output.drive_mv.abs_diff(before.drive_mv[channel]);
            assert!(
                delta <= SLEW_STEP_MV,
                "channel {channel} drive jumped {delta} mV in one dispatch \
                 (limit {SLEW_STEP_MV}); live drives must move via bounded steps"
            );
        }
    }
}
