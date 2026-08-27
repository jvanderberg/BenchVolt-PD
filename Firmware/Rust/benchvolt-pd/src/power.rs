use crate::app::{
    Action, AppState, AwgStatus, ChannelSnapshot, Fault, Measurement, OutputTransition,
    RegulationMode,
};
use reducto::TransitionEffect;

const STARTUP_GRACE_SAMPLES: u8 = 10;
const FAULT_CONFIRM_SAMPLES: u8 = 3;
const SINK_RECOVERY_SAMPLES: u8 = 10;
const VOLTAGE_SETTING_SETTLE_SAMPLES: u8 = 25;
const HARD_OVERCURRENT_CEILING_MA: u16 = 3_300;
/// Each shared converter feeds two independently limited 3 A channels, but
/// the r3 hardware specification caps their combined load at 5 A.
pub const SHARED_RAIL_LIMIT_MA: u16 = 5_000;
// Board tests show IOUT_LIMIT is not a usable CH5 regulation loop. Keep this
// fixed; user CC is implemented only by ADC feedback and voltage side effects.
const CH5_TPS_CONFIGURATION_LIMIT_MA: u16 = 3_000;

/// Convert the TPS55289's latched STATUS fault bits into application faults.
/// SCP and OCP are both current-path failures. OVP remains a hardware fault
/// because the application does not expose a distinct overvoltage cause.
pub const fn tps55289_status_fault(status: u8) -> Option<Fault> {
    if status & 0xc0 != 0 {
        Some(Fault::OverCurrent)
    } else if status & 0x20 != 0 {
        Some(Fault::Hardware)
    } else {
        None
    }
}

/// A TPS output acknowledgement covers OE and the converter's own status.
/// Actual output-voltage qualification remains the ADC protection monitor's job.
pub const fn tps55289_output_acknowledged(mode: u8, status: u8) -> bool {
    mode & 0x80 != 0 && tps55289_status_fault(status).is_none() && status & 0x03 != 0x03
}

/// Convert millivolts to the TPS55289 internal-feedback reference code.
pub const fn tps55289_voltage_code(millivolts: u16) -> u16 {
    let reference_uv = (millivolts as u32).saturating_mul(564) / 10;
    let delta_uv = reference_uv.saturating_sub(45_000);
    let code = (delta_uv.saturating_mul(10) + 2_822) / 5_645;
    if code > 0x07fe {
        0x07fe
    } else {
        code as u16
    }
}

/// Convert milliamps for a 10 mΩ sense resistor to IOUT_LIMIT.
pub const fn tps55289_current_code(milliamps: u16) -> u8 {
    if milliamps == 0 {
        0
    } else {
        let code = milliamps / 50;
        0x80 | if code > 127 { 127 } else { code as u8 }
    }
}

#[inline(always)]
pub const fn tps55289_configuration_registers(
    vout_fs: u8,
    mode: u8,
    slew: u8,
    enable_output: bool,
    forced_pwm: bool,
) -> Option<[u8; 3]> {
    if vout_fs == 0xff || mode == 0xff || slew == 0xff {
        return None;
    }
    let vout_fs = (vout_fs & !(0x80 | 0x03)) | 0x03;
    let mode = if enable_output {
        mode | 0x80
    } else {
        mode & !0x80
    };
    let mode = if forced_pwm {
        mode | 0x02
    } else {
        mode & !0x02
    };
    let slew = if forced_pwm {
        (slew & !0x03) | 0x03
    } else {
        slew
    };
    Some([vout_fs, mode, slew])
}

#[inline(always)]
pub const fn tps55289_output_mode(mode: u8, enabled: bool) -> Option<u8> {
    if mode == 0xff {
        None
    } else if enabled {
        Some(mode | 0x80)
    } else {
        Some(mode & !0x80)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ProtectionSnapshot {
    pub active: bool,
    pub last: Measurement,
    pub peak_milliamps: u16,
    pub grace_remaining: u8,
    pub overcurrent_samples: u8,
    pub voltage_samples: u8,
    pub samples_since_enable: u16,
    pub trip: Measurement,
}

#[derive(Clone, Copy)]
pub struct ProtectionMonitor {
    active: bool,
    last: Measurement,
    peak_milliamps: u16,
    grace_remaining: u8,
    overcurrent_samples: u8,
    voltage_samples: u8,
    tracked_drive_mv: u16,
    voltage_settle_remaining: u8,
    samples_since_enable: u16,
    trip: Measurement,
}

impl Default for ProtectionMonitor {
    fn default() -> Self {
        Self {
            active: false,
            last: Measurement {
                millivolts: 0,
                milliamps: 0,
                valid: false,
            },
            peak_milliamps: 0,
            grace_remaining: 0,
            overcurrent_samples: 0,
            voltage_samples: 0,
            tracked_drive_mv: 0,
            voltage_settle_remaining: 0,
            samples_since_enable: 0,
            trip: Measurement {
                millivolts: 0,
                milliamps: 0,
                valid: false,
            },
        }
    }
}

impl ProtectionMonitor {
    pub fn snapshot(&self) -> ProtectionSnapshot {
        ProtectionSnapshot {
            active: self.active,
            last: self.last,
            peak_milliamps: self.peak_milliamps,
            grace_remaining: self.grace_remaining,
            overcurrent_samples: self.overcurrent_samples,
            voltage_samples: self.voltage_samples,
            samples_since_enable: self.samples_since_enable,
            trip: self.trip,
        }
    }

    pub fn observe(&mut self, output: &ChannelSnapshot, measurement: Measurement) -> Option<Fault> {
        self.observe_with_voltage_tracking(output, measurement, true)
    }

    pub fn observe_with_voltage_tracking(
        &mut self,
        output: &ChannelSnapshot,
        measurement: Measurement,
        voltage_tracking: bool,
    ) -> Option<Fault> {
        if !output.physical_enabled {
            self.active = false;
            self.grace_remaining = 0;
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            self.samples_since_enable = 0;
            return None;
        }
        if !measurement.valid {
            self.last = measurement;
            self.trip = measurement;
            return Some(Fault::Sensor);
        }
        if !self.active {
            self.active = true;
            self.peak_milliamps = 0;
            self.grace_remaining = STARTUP_GRACE_SAMPLES;
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            self.tracked_drive_mv = output.drive_mv;
            self.voltage_settle_remaining = 0;
            self.samples_since_enable = 0;
        }
        self.last = measurement;
        self.peak_milliamps = self.peak_milliamps.max(measurement.milliamps);
        self.samples_since_enable = self.samples_since_enable.saturating_add(1);
        if output.drive_mv != self.tracked_drive_mv {
            self.tracked_drive_mv = output.drive_mv;
            self.voltage_settle_remaining = VOLTAGE_SETTING_SETTLE_SAMPLES;
            self.voltage_samples = 0;
        }
        if self.grace_remaining > 0 {
            self.grace_remaining -= 1;
            // Startup grace suppresses voltage-settling false positives, but
            // never permits a gross current excursion beyond the channel's
            // physical operating envelope.
            if measurement.milliamps > HARD_OVERCURRENT_CEILING_MA {
                self.trip = measurement;
                return Some(Fault::OverCurrent);
            }
            return None;
        }

        let overcurrent_threshold = match output.regulation_mode {
            RegulationMode::Cv => output.current_limit_ma,
            RegulationMode::Cc => {
                (u32::from(output.current_limit_ma) * 125 / 100 + 100).min(3_300) as u16
            }
        };
        if measurement.milliamps > overcurrent_threshold {
            self.overcurrent_samples = self.overcurrent_samples.saturating_add(1);
        } else {
            self.overcurrent_samples = 0;
        }
        if self.overcurrent_samples >= FAULT_CONFIRM_SAMPLES {
            self.trip = measurement;
            return Some(Fault::OverCurrent);
        }

        if !voltage_tracking {
            self.voltage_samples = 0;
            return None;
        }

        // A commanded downward step can leave the measured output above the
        // new window while output capacitance discharges. I2C/DAC failures
        // already fail closed in the voltage side effect, and overcurrent is
        // still checked above on every sample. Only voltage tracking receives
        // this bounded settling interval.
        if self.voltage_settle_remaining > 0 {
            self.voltage_settle_remaining -= 1;
            self.voltage_samples = 0;
            return None;
        }

        let voltage_below_window = output.regulation_mode == RegulationMode::Cv
            && u32::from(measurement.millivolts) * 100 < u32::from(output.setpoint_mv) * 80;
        let voltage_above_window =
            u32::from(measurement.millivolts) * 100 > u32::from(output.setpoint_mv) * 120;
        let voltage_outside_window = voltage_below_window || voltage_above_window;
        if voltage_outside_window {
            self.voltage_samples = self.voltage_samples.saturating_add(1);
        } else {
            self.voltage_samples = 0;
        }
        if self.voltage_samples >= FAULT_CONFIRM_SAMPLES {
            self.trip = measurement;
            Some(Fault::Hardware)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkProtectionEvent {
    Trip(Fault),
    Recovered,
}

#[derive(Clone, Copy, Default)]
pub struct SinkProtectionMonitor {
    overcurrent_samples: u8,
    voltage_samples: u8,
    recovery_samples: u8,
}

fn sink_voltage_within_contract(state: &AppState, measurement: Measurement) -> bool {
    state.pd_contract.is_some_and(|contract| {
        let measured = u32::from(measurement.millivolts) * 100;
        let negotiated = u32::from(contract.millivolts);
        measured >= negotiated * 80 && measured <= negotiated * 120
    })
}

#[derive(Clone, Copy, Default)]
pub struct SharedRailProtectionMonitor {
    overcurrent_samples: [u8; 2],
}

impl SharedRailProtectionMonitor {
    pub fn observe(
        &mut self,
        state: &AppState,
        measurements: &[Measurement; 5],
        rail: Rail,
    ) -> Option<Fault> {
        let (rail_index, channels) = match rail {
            Rail::Dc1 => (0, [0usize, 1]),
            Rail::Dc2 => (1, [2usize, 3]),
        };
        let mut active = false;
        let mut total_ma = 0u32;
        for channel in channels {
            if state.channels[channel].physical_enabled {
                active = true;
                if !measurements[channel].valid {
                    self.overcurrent_samples[rail_index] = 0;
                    return Some(Fault::Sensor);
                }
                total_ma = total_ma.saturating_add(u32::from(measurements[channel].milliamps));
            }
        }
        if !active {
            self.overcurrent_samples[rail_index] = 0;
            return None;
        }
        if total_ma > u32::from(SHARED_RAIL_LIMIT_MA) {
            self.overcurrent_samples[rail_index] =
                self.overcurrent_samples[rail_index].saturating_add(1);
        } else {
            self.overcurrent_samples[rail_index] = 0;
        }
        if self.overcurrent_samples[rail_index] >= FAULT_CONFIRM_SAMPLES {
            Some(Fault::OverCurrent)
        } else {
            None
        }
    }
}

impl SinkProtectionMonitor {
    pub fn observe(
        &mut self,
        state: &AppState,
        measurement: Measurement,
    ) -> Option<SinkProtectionEvent> {
        let output_active = state
            .channels
            .iter()
            .any(|output| output.requested_enabled || output.physical_enabled);
        let effective_limit_ma = state
            .pd_contract
            .map(|contract| contract.operating_milliamps)
            .unwrap_or(state.sink_current_limit_ma)
            .min(state.sink_current_limit_ma);

        if state.sink_fault != Fault::None {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            if !output_active
                && measurement.valid
                && measurement.milliamps <= effective_limit_ma
                && sink_voltage_within_contract(state, measurement)
            {
                self.recovery_samples = self.recovery_samples.saturating_add(1);
                if self.recovery_samples >= SINK_RECOVERY_SAMPLES {
                    self.recovery_samples = 0;
                    return Some(SinkProtectionEvent::Recovered);
                }
            } else {
                self.recovery_samples = 0;
            }
            return None;
        }

        self.recovery_samples = 0;
        if !output_active {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            return None;
        }
        if !measurement.valid {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            return Some(SinkProtectionEvent::Trip(Fault::Sensor));
        }
        if state.pd_contract.is_none() {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            return Some(SinkProtectionEvent::Trip(Fault::Hardware));
        }
        let gross_limit_ma = ((u32::from(effective_limit_ma) * 125 / 100) + 100).min(5_500) as u16;
        if measurement.milliamps > gross_limit_ma {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            return Some(SinkProtectionEvent::Trip(Fault::OverCurrent));
        }
        if measurement.milliamps > effective_limit_ma {
            self.overcurrent_samples = self.overcurrent_samples.saturating_add(1);
        } else {
            self.overcurrent_samples = 0;
        }
        if self.overcurrent_samples >= FAULT_CONFIRM_SAMPLES {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            return Some(SinkProtectionEvent::Trip(Fault::OverCurrent));
        }
        if sink_voltage_within_contract(state, measurement) {
            self.voltage_samples = 0;
        } else {
            self.voltage_samples = self.voltage_samples.saturating_add(1);
        }
        if self.voltage_samples >= FAULT_CONFIRM_SAMPLES {
            self.overcurrent_samples = 0;
            self.voltage_samples = 0;
            Some(SinkProtectionEvent::Trip(Fault::Hardware))
        } else {
            None
        }
    }
}

/// Project application state into the semantics protection must validate.
/// AWG owns the adjustable channel's voltage command, so comparing its live
/// output with the saved DC compliance setpoint would create false faults
/// during any sufficiently long LOW interval.
pub fn protection_output(state: &AppState, channel: u8) -> ChannelSnapshot {
    let mut output = state.channels[usize::from(channel)];
    if state.awg_status == AwgStatus::Running && channel == state.active_awg_channel() {
        output.regulation_mode = RegulationMode::Cv;
        output.setpoint_mv = output.drive_mv;
    }
    output
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerEffect {
    Output {
        channel: u8,
        operation: u16,
        enabled: bool,
    },
    Voltage {
        channel: u8,
        millivolts: u16,
    },
}

pub struct FirmwareEffectPlanner;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareEffect {
    pub power: Option<PowerEffect>,
    pub global_shutdown: bool,
}

impl TransitionEffect<AppState> for FirmwareEffectPlanner {
    type Effect = FirmwareEffect;

    fn plan(old: &AppState, new: &AppState) -> Option<Self::Effect> {
        let awg_boundary = old.screen != new.screen
            && (old.screen == crate::app::Screen::Awg || new.screen == crate::app::Screen::Awg);
        let sink_trip = old.sink_fault != new.sink_fault && new.sink_fault != Fault::None;
        let contract_lost = old.pd_contract.is_some() && new.pd_contract.is_none();
        let contract_limit_changed = old.sink_current_limit_ma != new.sink_current_limit_ma
            && old.pd_contract.is_some()
            && old
                .channels
                .iter()
                .any(|output| output.requested_enabled || output.physical_enabled);
        let global_shutdown = awg_boundary || sink_trip || contract_lost || contract_limit_changed;
        let power = if global_shutdown {
            None
        } else {
            effect_for_transition(old, new)
        };
        (global_shutdown || power.is_some()).then_some(FirmwareEffect {
            power,
            global_shutdown,
        })
    }
}

pub fn effect_for_transition(old: &AppState, new: &AppState) -> Option<PowerEffect> {
    old.channels
        .iter()
        .zip(new.channels.iter())
        .enumerate()
        .find_map(|(channel, (old, new))| {
            if old.transition == new.transition {
                return None;
            }
            match new.transition {
                OutputTransition::Enabling(operation) => Some(PowerEffect::Output {
                    channel: channel as u8,
                    operation,
                    enabled: true,
                }),
                OutputTransition::Disabling(operation) => Some(PowerEffect::Output {
                    channel: channel as u8,
                    operation,
                    enabled: false,
                }),
                OutputTransition::Stable => None,
            }
        })
        .or_else(|| {
            old.channels
                .iter()
                .zip(new.channels.iter())
                .enumerate()
                .find_map(|(channel, (old, new))| {
                    let millivolts = if matches!(channel, 3 | 4)
                        && old.drive_mv != new.drive_mv
                        && new.physical_enabled
                    {
                        Some(new.drive_mv)
                    } else {
                        None
                    };
                    millivolts.map(|millivolts| PowerEffect::Voltage {
                        channel: channel as u8,
                        millivolts,
                    })
                })
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rail {
    Dc1,
    Dc2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverOperation {
    ChannelGate {
        channel: u8,
        enabled: bool,
    },
    RailEnable {
        rail: Rail,
        enabled: bool,
    },
    ConfigureRail {
        rail: Rail,
        millivolts: u16,
    },
    VerifyRail {
        rail: Rail,
    },
    SetAdjustableDac {
        millivolts: u16,
    },
    Ch5Enable(bool),
    ConfigureCh5 {
        millivolts: u16,
        current_limit_ma: u16,
        forced_pwm: bool,
    },
    ClearCh5Status,
    Ch5OutputEnable(bool),
    Ch5Voltage(u16),
    /// Waveform-sample fast path: one write, no verify read-back, no retries.
    Ch5VoltageUnverified(u16),
    VerifyOutput {
        channel: u8,
        millivolts: u16,
    },
}

pub trait PowerDriver {
    type Error;

    fn apply(&mut self, operation: DriverOperation) -> Result<(), Self::Error>;

    fn cancel_pending(&mut self) {}
}

const POWER_PLAN_CAPACITY: usize = 7;
const HARDWARE_SETTLE_MS: u16 = 50;

#[derive(Clone, Copy)]
struct PowerPlan {
    operations: [DriverOperation; POWER_PLAN_CAPACITY],
    len: u8,
    cursor: u8,
    overflowed: bool,
    channel: u8,
    operation: u16,
    enabled: bool,
    rail_to_disable_on_failure: Option<Rail>,
}

impl PowerPlan {
    fn new(
        channel: u8,
        operation: u16,
        enabled: bool,
        rail_to_disable_on_failure: Option<Rail>,
    ) -> Self {
        Self {
            operations: [DriverOperation::Ch5Enable(false); POWER_PLAN_CAPACITY],
            len: 0,
            cursor: 0,
            overflowed: false,
            channel,
            operation,
            enabled,
            rail_to_disable_on_failure,
        }
    }

    fn push(&mut self, operation: DriverOperation) {
        if usize::from(self.len) < POWER_PLAN_CAPACITY {
            self.operations[usize::from(self.len)] = operation;
            self.len += 1;
        } else {
            // Overflow means the plan is incomplete; poison it so `next()`
            // never runs a truncated sequence. `run_ready` treats the
            // exhausted plan as complete only after every stage ran, so a
            // poisoned plan aborts to the fail-safe path.
            self.overflowed = true;
        }
    }

    fn is_valid(&self) -> bool {
        !self.overflowed
    }

    fn next(&mut self) -> Option<DriverOperation> {
        if self.cursor == self.len {
            return None;
        }
        let operation = self.operations[usize::from(self.cursor)];
        self.cursor += 1;
        Some(operation)
    }
}

/// Runs power-up sequencing one bounded stage at a time. Hardware settling is
/// represented by a deadline, leaving the firmware loop free to service USB,
/// PD, watchdog, and protection between stages.
pub struct PowerExecutor<D> {
    driver: D,
    pending: Option<PowerPlan>,
    deadline_ms: Option<u16>,
    now_ms: u16,
}

impl<D> PowerExecutor<D> {
    pub const fn new(driver: D, now_ms: u16) -> Self {
        Self {
            driver,
            pending: None,
            deadline_ms: None,
            now_ms,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }
}

impl<D> core::ops::Deref for PowerExecutor<D> {
    type Target = D;

    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}

impl<D> core::ops::DerefMut for PowerExecutor<D> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.driver
    }
}

fn operation_settle_ms(operation: DriverOperation) -> u16 {
    match operation {
        DriverOperation::RailEnable { enabled: true, .. }
        | DriverOperation::ConfigureRail { .. }
        | DriverOperation::Ch5Enable(true)
        | DriverOperation::Ch5OutputEnable(true) => HARDWARE_SETTLE_MS,
        _ => 0,
    }
}

fn deadline_reached(now_ms: u16, deadline_ms: u16) -> bool {
    now_ms.wrapping_sub(deadline_ms) < 0x8000
}

fn plan_output(
    state: &AppState,
    channel: u8,
    operation: u16,
    enabled: bool,
) -> Result<PowerPlan, Fault> {
    let output = state
        .channels
        .get(usize::from(channel))
        .ok_or(Fault::Hardware)?;
    let sibling_on = sibling_is_on(state, channel);
    let rail = rail_for(channel);
    let mut plan = PowerPlan::new(
        channel,
        operation,
        enabled,
        (!sibling_on).then_some(rail).flatten(),
    );
    if enabled {
        enable_is_eligible(state, channel)?;
        if channel == 4 {
            plan.push(DriverOperation::Ch5Enable(false));
            plan.push(DriverOperation::Ch5Enable(true));
            plan.push(DriverOperation::ConfigureCh5 {
                millivolts: output.drive_mv,
                current_limit_ma: CH5_TPS_CONFIGURATION_LIMIT_MA,
                forced_pwm: state.awg_status == AwgStatus::Starting
                    && state.active_awg_channel() == 4,
            });
            plan.push(DriverOperation::ClearCh5Status);
            plan.push(DriverOperation::ClearCh5Status);
            plan.push(DriverOperation::Ch5OutputEnable(true));
        } else {
            let rail = rail.ok_or(Fault::Hardware)?;
            plan.push(DriverOperation::ChannelGate {
                channel,
                enabled: false,
            });
            if !sibling_on {
                plan.push(DriverOperation::RailEnable {
                    rail,
                    enabled: true,
                });
                plan.push(DriverOperation::ConfigureRail {
                    rail,
                    millivolts: rail_setpoint(rail),
                });
            }
            plan.push(DriverOperation::VerifyRail { rail });
            if channel == 3 {
                plan.push(DriverOperation::SetAdjustableDac {
                    millivolts: output.drive_mv,
                });
            }
            plan.push(DriverOperation::ChannelGate {
                channel,
                enabled: true,
            });
        }
        plan.push(DriverOperation::VerifyOutput {
            channel,
            millivolts: output.drive_mv,
        });
    } else if channel == 4 {
        plan.push(DriverOperation::Ch5OutputEnable(false));
        plan.push(DriverOperation::Ch5Enable(false));
    } else {
        plan.push(DriverOperation::ChannelGate {
            channel,
            enabled: false,
        });
        if !sibling_on {
            plan.push(DriverOperation::RailEnable {
                rail: rail.ok_or(Fault::Hardware)?,
                enabled: false,
            });
        }
    }
    Ok(plan)
}

impl<D: PowerDriver> PowerExecutor<D> {
    fn plan_matches_state(plan: PowerPlan, state: &AppState) -> Result<bool, Fault> {
        let Some(output) = state.channels.get(usize::from(plan.channel)) else {
            return Err(Fault::Hardware);
        };
        let expected = if plan.enabled {
            OutputTransition::Enabling(plan.operation)
        } else {
            OutputTransition::Disabling(plan.operation)
        };
        if output.transition != expected || output.requested_enabled != plan.enabled {
            return Ok(false);
        }
        if plan.enabled {
            enable_is_eligible(state, plan.channel)?;
        }
        Ok(true)
    }

    fn shutdown_failed_plan(&mut self, plan: PowerPlan) {
        if plan.channel == 4 {
            let _ = self.driver.apply(DriverOperation::Ch5OutputEnable(false));
            let _ = self.driver.apply(DriverOperation::Ch5Enable(false));
        } else {
            let _ = self.driver.apply(DriverOperation::ChannelGate {
                channel: plan.channel,
                enabled: false,
            });
            if let Some(rail) = plan.rail_to_disable_on_failure {
                let _ = self.driver.apply(DriverOperation::RailEnable {
                    rail,
                    enabled: false,
                });
            }
        }
    }

    fn completion(plan: PowerPlan) -> Action {
        Action::OutputApplied {
            channel: plan.channel,
            operation: plan.operation,
            enabled: plan.enabled,
        }
    }

    fn failure(plan: PowerPlan) -> Action {
        Action::OutputFailed {
            channel: plan.channel,
            operation: plan.operation,
            fault: Fault::Hardware,
        }
    }

    fn run_ready(&mut self, state: &AppState) -> Option<Action> {
        let mut plan = self.pending.take()?;
        if !plan.is_valid() {
            self.deadline_ms = None;
            self.shutdown_failed_plan(plan);
            return Some(Self::failure(plan));
        }
        match Self::plan_matches_state(plan, state) {
            Ok(true) => {}
            Ok(false) => {
                self.deadline_ms = None;
                self.shutdown_failed_plan(plan);
                return None;
            }
            Err(fault) => {
                self.deadline_ms = None;
                self.shutdown_failed_plan(plan);
                let mut action = Self::failure(plan);
                if let Action::OutputFailed { fault: value, .. } = &mut action {
                    *value = fault;
                }
                return Some(action);
            }
        }
        while let Some(operation) = plan.next() {
            if self.driver.apply(operation).is_err() {
                if !plan.enabled && matches!(operation, DriverOperation::Ch5OutputEnable(false)) {
                    continue;
                }
                self.deadline_ms = None;
                self.shutdown_failed_plan(plan);
                return Some(Self::failure(plan));
            }
            let settle_ms = operation_settle_ms(operation);
            if settle_ms != 0 {
                self.deadline_ms = Some(self.now_ms.wrapping_add(settle_ms));
                self.pending = Some(plan);
                return matches!(operation, DriverOperation::Ch5OutputEnable(true)).then_some(
                    Action::OutputEnergized {
                        channel: plan.channel,
                        operation: plan.operation,
                    },
                );
            }
        }
        self.deadline_ms = None;
        Some(Self::completion(plan))
    }

    #[inline(never)]
    pub fn submit(&mut self, state: &AppState, effect: PowerEffect) -> Option<Action> {
        if let PowerEffect::Voltage {
            channel,
            millivolts,
        } = effect
        {
            // P4: a voltage write for the channel a staged plan is currently
            // sequencing would interleave I2C traffic into that sequence.
            if matches!(self.pending, Some(plan) if plan.channel == channel) {
                return Some(Action::HardwareSettingFailed {
                    channel,
                    fault: Fault::Hardware,
                });
            }
            let result = match channel {
                3 => self
                    .driver
                    .apply(DriverOperation::SetAdjustableDac { millivolts }),
                // While the AWG streams samples every 500 us, the verified
                // write-and-read-back transaction (~90 I2C bits plus retries)
                // exceeds the sample period and turns the waveform into
                // timing jitter. Samples are self-correcting — the next one
                // overwrites 500 us later — so they use a single unverified
                // write; ordinary setpoint changes keep the verified path.
                4 if state.awg_status == AwgStatus::Running => self
                    .driver
                    .apply(DriverOperation::Ch5VoltageUnverified(millivolts)),
                4 => self.driver.apply(DriverOperation::Ch5Voltage(millivolts)),
                // Only CH4/CH5 have voltage hardware; acknowledging anything
                // else would be a false success.
                _ => {
                    return Some(Action::HardwareSettingFailed {
                        channel,
                        fault: Fault::Hardware,
                    })
                }
            };
            return Some(if result.is_ok() {
                Action::HardwareSettingApplied
            } else {
                best_effort_shutdown(&mut self.driver, state, channel);
                Action::HardwareSettingFailed {
                    channel,
                    fault: Fault::Hardware,
                }
            });
        }
        let PowerEffect::Output {
            channel,
            operation,
            enabled,
        } = effect
        else {
            unreachable!()
        };
        if self.pending.is_some() {
            let preempts = matches!(
                self.pending,
                Some(PowerPlan {
                    channel: active_channel,
                    enabled: true,
                    ..
                }) if active_channel == channel && !enabled
            );
            if preempts {
                let active = self.pending.take().unwrap();
                self.deadline_ms = None;
                self.shutdown_failed_plan(active);
            } else {
                return Some(if enabled {
                    best_effort_shutdown(&mut self.driver, state, channel);
                    Action::OutputFailed {
                        channel,
                        operation,
                        fault: Fault::Hardware,
                    }
                } else {
                    match run_disable(&mut self.driver, state, channel) {
                        Ok(()) => Action::OutputApplied {
                            channel,
                            operation,
                            enabled: false,
                        },
                        Err(fault) => {
                            best_effort_shutdown(&mut self.driver, state, channel);
                            Action::OutputFailed {
                                channel,
                                operation,
                                fault,
                            }
                        }
                    }
                });
            }
        }
        if self.pending.is_some() {
            return None;
        }
        match plan_output(state, channel, operation, enabled) {
            Ok(plan) => self.pending = Some(plan),
            Err(fault) => {
                best_effort_shutdown(&mut self.driver, state, channel);
                return Some(Action::OutputFailed {
                    channel,
                    operation,
                    fault,
                });
            }
        }
        self.run_ready(state)
    }

    pub fn service(&mut self, now_ms: u16, state: &AppState) -> Option<Action> {
        self.now_ms = now_ms;
        match self.deadline_ms {
            Some(deadline) if !deadline_reached(now_ms, deadline) => None,
            _ => self.run_ready(state),
        }
    }
}

impl<D: PowerDriver> PowerDriver for PowerExecutor<D> {
    type Error = D::Error;

    fn apply(&mut self, operation: DriverOperation) -> Result<(), Self::Error> {
        self.driver.apply(operation)
    }

    fn cancel_pending(&mut self) {
        self.pending = None;
        self.deadline_ms = None;
        self.driver.cancel_pending();
    }
}

pub const OVERTEMPERATURE_TRIP_SIXTEENTHS_C: i16 = 75 * 16;
pub const OVERTEMPERATURE_REENABLE_SIXTEENTHS_C: i16 = 70 * 16;

fn rail_for(channel: u8) -> Option<Rail> {
    match channel {
        0 | 1 => Some(Rail::Dc1),
        2 | 3 => Some(Rail::Dc2),
        _ => None,
    }
}

fn rail_setpoint(rail: Rail) -> u16 {
    match rail {
        Rail::Dc1 => 3_000,
        Rail::Dc2 => 5_500,
    }
}

fn sibling_is_on(state: &AppState, channel: u8) -> bool {
    let Some(rail) = rail_for(channel) else {
        return false;
    };
    state.channels.iter().enumerate().any(|(index, output)| {
        index != usize::from(channel)
            && rail_for(index as u8) == Some(rail)
            && output.physical_enabled
    })
}

fn enable_is_eligible(state: &AppState, channel: u8) -> Result<(), Fault> {
    let Some(output) = state.channels.get(usize::from(channel)) else {
        return Err(Fault::Hardware);
    };
    if !state.recovery_armed || !state.temp_valid || !output.measurement.valid {
        return Err(Fault::Sensor);
    }
    if state.sink_fault != Fault::None {
        return Err(state.sink_fault);
    }
    if state.pd_contract.is_none() {
        return Err(Fault::Hardware);
    }
    if state.temp_sixteenths_c >= OVERTEMPERATURE_REENABLE_SIXTEENTHS_C {
        return Err(Fault::OverTemperature);
    }
    if output.current_limit_ma > 3_000 {
        return Err(Fault::Hardware);
    }
    Ok(())
}

/// Public so the test harness can mirror the emitter contract of
/// `HardwareSettingFailed`: the hardware for the channel is best-effort shut
/// down before the failure action is dispatched.
pub fn best_effort_shutdown<D: PowerDriver>(driver: &mut D, state: &AppState, channel: u8) {
    if channel == 4 {
        let _ = driver.apply(DriverOperation::Ch5OutputEnable(false));
        let _ = driver.apply(DriverOperation::Ch5Enable(false));
        return;
    }
    let _ = driver.apply(DriverOperation::ChannelGate {
        channel,
        enabled: false,
    });
    if !sibling_is_on(state, channel) {
        if let Some(rail) = rail_for(channel) {
            let _ = driver.apply(DriverOperation::RailEnable {
                rail,
                enabled: false,
            });
        }
    }
}

/// Synchronous enable used ONLY by the host test-suite as a reference
/// sequence. Production enables run through `PowerExecutor::submit`'s staged
/// `plan_output` path, which inserts hardware settle intervals between
/// stages; this function does not and must never be called from firmware.
#[cfg(test)]
fn run_enable<D: PowerDriver>(driver: &mut D, state: &AppState, channel: u8) -> Result<(), Fault> {
    enable_is_eligible(state, channel)?;
    let output = &state.channels[usize::from(channel)];

    if channel == 4 {
        driver
            .apply(DriverOperation::Ch5Enable(false))
            .map_err(|_| Fault::Hardware)?;
        driver
            .apply(DriverOperation::Ch5Enable(true))
            .map_err(|_| Fault::Hardware)?;
        driver
            .apply(DriverOperation::ConfigureCh5 {
                millivolts: output.drive_mv,
                current_limit_ma: CH5_TPS_CONFIGURATION_LIMIT_MA,
                forced_pwm: state.awg_status == AwgStatus::Starting
                    && state.active_awg_channel() == 4,
            })
            .map_err(|_| Fault::Hardware)?;
        // STATUS is read-to-clear.  Clear power-up/configuration history before
        // OE so the runtime monitor only considers events from this enable.
        // The stock C firmware likewise reads STATUS twice during CH5 init.
        driver
            .apply(DriverOperation::ClearCh5Status)
            .map_err(|_| Fault::Hardware)?;
        driver
            .apply(DriverOperation::ClearCh5Status)
            .map_err(|_| Fault::Hardware)?;
        driver
            .apply(DriverOperation::Ch5OutputEnable(true))
            .map_err(|_| Fault::Hardware)?;
    } else {
        driver
            .apply(DriverOperation::ChannelGate {
                channel,
                enabled: false,
            })
            .map_err(|_| Fault::Hardware)?;
        let rail = rail_for(channel).ok_or(Fault::Hardware)?;
        if !sibling_is_on(state, channel) {
            driver
                .apply(DriverOperation::RailEnable {
                    rail,
                    enabled: true,
                })
                .map_err(|_| Fault::Hardware)?;
            driver
                .apply(DriverOperation::ConfigureRail {
                    rail,
                    millivolts: rail_setpoint(rail),
                })
                .map_err(|_| Fault::Hardware)?;
        }
        driver
            .apply(DriverOperation::VerifyRail { rail })
            .map_err(|_| Fault::Hardware)?;
        if channel == 3 {
            driver
                .apply(DriverOperation::SetAdjustableDac {
                    millivolts: output.drive_mv,
                })
                .map_err(|_| Fault::Hardware)?;
        }
        driver
            .apply(DriverOperation::ChannelGate {
                channel,
                enabled: true,
            })
            .map_err(|_| Fault::Hardware)?;
    }
    driver
        .apply(DriverOperation::VerifyOutput {
            channel,
            millivolts: output.drive_mv,
        })
        .map_err(|_| Fault::Hardware)
}

fn run_disable<D: PowerDriver>(driver: &mut D, state: &AppState, channel: u8) -> Result<(), Fault> {
    if channel == 4 {
        // OE is best effort: it can NACK when the converter is already held in
        // reset. Verified EN low is the authoritative physical shutdown.
        let _ = driver.apply(DriverOperation::Ch5OutputEnable(false));
        driver
            .apply(DriverOperation::Ch5Enable(false))
            .map_err(|_| Fault::Hardware)
    } else {
        driver
            .apply(DriverOperation::ChannelGate {
                channel,
                enabled: false,
            })
            .map_err(|_| Fault::Hardware)?;
        if !sibling_is_on(state, channel) {
            driver
                .apply(DriverOperation::RailEnable {
                    rail: rail_for(channel).ok_or(Fault::Hardware)?,
                    enabled: false,
                })
                .map_err(|_| Fault::Hardware)?;
        }
        Ok(())
    }
}

/// Synchronous effect execution for the host test-suite; see `run_enable`.
#[cfg(test)]
pub fn execute_effect<D: PowerDriver>(
    driver: &mut D,
    state: &AppState,
    effect: PowerEffect,
) -> Action {
    match effect {
        PowerEffect::Output {
            channel,
            operation,
            enabled,
        } => {
            let result = if enabled {
                run_enable(driver, state, channel)
            } else {
                run_disable(driver, state, channel)
            };
            match result {
                Ok(()) => Action::OutputApplied {
                    channel,
                    operation,
                    enabled,
                },
                Err(fault) => {
                    best_effort_shutdown(driver, state, channel);
                    Action::OutputFailed {
                        channel,
                        operation,
                        fault,
                    }
                }
            }
        }
        PowerEffect::Voltage {
            channel,
            millivolts,
        } => {
            let result = match channel {
                3 => driver.apply(DriverOperation::SetAdjustableDac { millivolts }),
                4 => driver.apply(DriverOperation::Ch5Voltage(millivolts)),
                _ => return Action::HardwareSettingApplied,
            };
            if result.is_ok() {
                Action::HardwareSettingApplied
            } else {
                best_effort_shutdown(driver, state, channel);
                Action::HardwareSettingFailed {
                    channel,
                    fault: Fault::Hardware,
                }
            }
        }
    }
}

/// Immediate, best-effort physical shutdown used by global safety interlocks.
/// Every independent off control is attempted even if an earlier driver call fails.
/// MCP4725 code for the CH4 (VLow) inverting drive stage, calibrated from
/// the original C firmware: 0.50 V -> code 3975, 5.00 V -> code 340.
/// Pure so the inverse mapping is host-testable.
pub const fn mcp4725_code_for_millivolts(millivolts: u16) -> u16 {
    let millivolts = if millivolts < 500 {
        500
    } else if millivolts > 5_000 {
        5_000
    } else {
        millivolts
    };
    let scaled = ((millivolts - 500) as u32 * 3_635 + 2_250) / 4_500;
    (3_975u32.saturating_sub(scaled)) as u16
}

pub fn execute_global_shutdown<D: PowerDriver>(driver: &mut D) -> Result<(), Fault> {
    driver.cancel_pending();
    let mut failed = false;
    for channel in 0..4 {
        failed |= driver
            .apply(DriverOperation::ChannelGate {
                channel,
                enabled: false,
            })
            .is_err();
    }
    // OE can NACK when CH5 is already held in reset. Physical EN low is the
    // authoritative, GPIO-readable shutdown state, so still attempt OE but do
    // not turn an already-safe shutdown into a false hardware fault.
    let _ = driver.apply(DriverOperation::Ch5OutputEnable(false));
    failed |= driver.apply(DriverOperation::Ch5Enable(false)).is_err();
    failed |= driver
        .apply(DriverOperation::RailEnable {
            rail: Rail::Dc1,
            enabled: false,
        })
        .is_err();
    failed |= driver
        .apply(DriverOperation::RailEnable {
            rail: Rail::Dc2,
            enabled: false,
        })
        .is_err();
    if failed {
        Err(Fault::Hardware)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
