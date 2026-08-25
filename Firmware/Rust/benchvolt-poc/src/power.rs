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
            channel,
            operation,
            enabled,
            rail_to_disable_on_failure,
        }
    }

    fn push(&mut self, operation: DriverOperation) {
        self.operations[usize::from(self.len)] = operation;
        self.len += 1;
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
            let result = match channel {
                3 => self
                    .driver
                    .apply(DriverOperation::SetAdjustableDac { millivolts }),
                4 => self.driver.apply(DriverOperation::Ch5Voltage(millivolts)),
                _ => return Some(Action::HardwareSettingApplied),
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

fn best_effort_shutdown<D: PowerDriver>(driver: &mut D, state: &AppState, channel: u8) {
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
mod tests {
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
                DriverOperation::Ch5Voltage(_) => {
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
}
