use reducto::{Reducer, VersionedState};

const VOLTAGE_SLEW_STEP_MV: u16 = 200;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Screen {
    Overview,
    Channel(u8),
    UsbPdInput,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ControlFocus {
    None,
    OverviewOutput(u8),
    Output,
    Voltage,
    CurrentLimit,
    RegulationMode,
}

impl ControlFocus {
    fn next(self, channel: u8) -> Self {
        match (self, channel) {
            (Self::None, _) => Self::Output,
            (Self::Output, 3 | 4) => Self::Voltage,
            (Self::Output, _) => Self::CurrentLimit,
            (Self::Voltage, 3 | 4) => Self::RegulationMode,
            (Self::Voltage, _) => Self::CurrentLimit,
            (Self::RegulationMode, 3 | 4) => Self::CurrentLimit,
            (Self::CurrentLimit, _) | (Self::RegulationMode, _) | (Self::OverviewOutput(_), _) => {
                Self::None
            }
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum RegulationMode {
    Cv,
    Cc,
}

impl Screen {
    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Channel(0),
            Self::Channel(index) if index < 4 => Self::Channel(index + 1),
            Self::Channel(_) => Self::UsbPdInput,
            Self::UsbPdInput => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Overview => Self::UsbPdInput,
            Self::Channel(index) if index > 0 => Self::Channel(index - 1),
            Self::Channel(_) => Self::Overview,
            Self::UsbPdInput => Self::Channel(4),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
#[allow(dead_code)]
pub enum Fault {
    None,
    OverCurrent,
    OverTemperature,
    Sensor,
    Hardware,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum OutputTransition {
    Stable,
    Enabling(u16),
    Disabling(u16),
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Measurement {
    pub millivolts: u16,
    pub milliamps: u16,
    pub valid: bool,
}

impl Measurement {
    const INVALID: Self = Self {
        millivolts: 0,
        milliamps: 0,
        valid: false,
    };
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ChannelSnapshot {
    pub setpoint_mv: u16,
    pub drive_mv: u16,
    pub current_limit_ma: u16,
    pub requested_enabled: bool,
    pub physical_enabled: bool,
    pub operation: u16,
    pub transition: OutputTransition,
    pub fault: Fault,
    pub measurement: Measurement,
    pub regulation_mode: RegulationMode,
}

impl ChannelSnapshot {
    const fn disabled(setpoint_mv: u16) -> Self {
        Self {
            setpoint_mv,
            drive_mv: setpoint_mv,
            current_limit_ma: 3_000,
            requested_enabled: false,
            physical_enabled: false,
            operation: 0,
            transition: OutputTransition::Stable,
            fault: Fault::None,
            measurement: Measurement::INVALID,
            regulation_mode: RegulationMode::Cv,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct AppState {
    pub version: u32,
    pub screen: Screen,
    pub focus: ControlFocus,
    pub channels: [ChannelSnapshot; 5],
    pub sink: Measurement,
    pub sink_current_limit_ma: u16,
    pub temp_sixteenths_c: i16,
    pub temp_valid: bool,
    pub recovery_armed: bool,
    pub reboot_requested: bool,
}

impl AppState {
    pub fn new(recovery_armed: bool, temperature: Option<i16>) -> Self {
        let (temp_sixteenths_c, temp_valid) = match temperature {
            Some(raw) => (raw, true),
            None => (0, false),
        };
        Self {
            version: 0,
            screen: Screen::Overview,
            focus: ControlFocus::None,
            channels: [
                ChannelSnapshot::disabled(1_800),
                ChannelSnapshot::disabled(2_500),
                ChannelSnapshot::disabled(3_300),
                ChannelSnapshot::disabled(5_000),
                ChannelSnapshot::disabled(12_000),
            ],
            sink: Measurement::INVALID,
            sink_current_limit_ma: 5_000,
            temp_sixteenths_c,
            temp_valid,
            recovery_armed,
            reboot_requested: false,
        }
    }
}

impl VersionedState for AppState {
    fn version(&self) -> u32 {
        self.version
    }
}

#[derive(Clone, Copy)]
pub enum Action {
    NextScreen,
    PreviousScreen,
    GoOverview,
    RequestReboot,
    NextControl,
    AdjustFocused(i8),
    ToggleOutputRequested {
        channel: u8,
    },
    SetCurrentLimit {
        channel: u8,
        milliamps: u16,
    },
    SetRegulationMode {
        channel: u8,
        mode: RegulationMode,
    },
    RegulateChannel {
        channel: u8,
        measurement: Measurement,
    },
    SetSinkCurrentLimit(u16),
    SetOutputRequested {
        channel: u8,
        enabled: bool,
    },
    OutputApplied {
        channel: u8,
        operation: u16,
        enabled: bool,
    },
    OutputFailed {
        channel: u8,
        operation: u16,
        fault: Fault,
    },
    ProtectionTrip {
        channel: u8,
        fault: Fault,
    },
    HardwareSettingApplied,
    HardwareSettingFailed {
        channel: u8,
        fault: Fault,
    },
    Temperature(Option<i16>),
    Measurements([Measurement; 5]),
    SinkMeasurement(Measurement),
}

pub struct AppReducer;

impl Reducer for AppReducer {
    type State = AppState;
    type Action = Action;

    fn reduce(state: &Self::State, action: Self::Action) -> Self::State {
        let mut next = *state;
        let changed = match action {
            Action::NextScreen => {
                next.screen = state.screen.next();
                next.focus = ControlFocus::None;
                true
            }
            Action::PreviousScreen => {
                next.screen = state.screen.previous();
                next.focus = ControlFocus::None;
                true
            }
            Action::GoOverview
                if state.screen == Screen::Overview && state.focus == ControlFocus::None =>
            {
                false
            }
            Action::GoOverview => {
                next.screen = Screen::Overview;
                next.focus = ControlFocus::None;
                true
            }
            Action::RequestReboot if state.reboot_requested => false,
            Action::RequestReboot => {
                next.reboot_requested = true;
                true
            }
            Action::NextControl => match state.screen {
                Screen::Channel(channel) => {
                    next.focus = state.focus.next(channel);
                    true
                }
                Screen::UsbPdInput => {
                    next.focus = if state.focus == ControlFocus::CurrentLimit {
                        ControlFocus::None
                    } else {
                        ControlFocus::CurrentLimit
                    };
                    true
                }
                Screen::Overview => {
                    next.focus = match state.focus {
                        ControlFocus::OverviewOutput(channel) if channel < 4 => {
                            ControlFocus::OverviewOutput(channel + 1)
                        }
                        ControlFocus::OverviewOutput(_) => ControlFocus::None,
                        _ => ControlFocus::OverviewOutput(0),
                    };
                    true
                }
            },
            Action::AdjustFocused(direction) => {
                if direction == 0 {
                    false
                } else if state.screen == Screen::UsbPdInput
                    && state.focus == ControlFocus::CurrentLimit
                {
                    let adjusted =
                        i32::from(state.sink_current_limit_ma) + i32::from(direction) * 10;
                    let adjusted = adjusted.clamp(0, 5_000) as u16;
                    if adjusted == state.sink_current_limit_ma {
                        false
                    } else {
                        next.sink_current_limit_ma = adjusted;
                        true
                    }
                } else if let Screen::Channel(channel) = state.screen {
                    let output = &mut next.channels[usize::from(channel)];
                    if output.transition != OutputTransition::Stable {
                        false
                    } else {
                        match state.focus {
                            ControlFocus::Voltage if channel >= 3 => {
                                let (minimum, maximum) = if channel == 3 {
                                    (500, 5_000)
                                } else {
                                    (800, 22_000)
                                };
                                let adjusted =
                                    i32::from(output.setpoint_mv) + i32::from(direction) * 10;
                                let adjusted = adjusted.clamp(minimum, maximum) as u16;
                                if adjusted == output.setpoint_mv {
                                    false
                                } else {
                                    output.setpoint_mv = adjusted;
                                    if !output.physical_enabled {
                                        output.drive_mv = adjusted;
                                    }
                                    true
                                }
                            }
                            ControlFocus::CurrentLimit => {
                                let adjusted =
                                    i32::from(output.current_limit_ma) + i32::from(direction) * 10;
                                let adjusted = adjusted.clamp(0, 3_000) as u16;
                                if adjusted == output.current_limit_ma {
                                    false
                                } else {
                                    output.current_limit_ma = adjusted;
                                    true
                                }
                            }
                            ControlFocus::RegulationMode if channel >= 3 => {
                                output.regulation_mode = match output.regulation_mode {
                                    RegulationMode::Cv => RegulationMode::Cc,
                                    RegulationMode::Cc => RegulationMode::Cv,
                                };
                                output.drive_mv = output.setpoint_mv;
                                true
                            }
                            _ => false,
                        }
                    }
                } else {
                    false
                }
            }
            Action::ToggleOutputRequested { channel } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                if output.transition != OutputTransition::Stable {
                    false
                } else {
                    let enabled = !output.requested_enabled;
                    output.operation = output.operation.wrapping_add(1);
                    let operation = output.operation;
                    output.requested_enabled = enabled;
                    if enabled {
                        output.fault = Fault::None;
                        output.transition = OutputTransition::Enabling(operation);
                    } else {
                        output.transition = OutputTransition::Disabling(operation);
                    }
                    true
                }
            }
            Action::SetCurrentLimit { channel, milliamps } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                if milliamps > 3_000
                    || output.transition != OutputTransition::Stable
                    || output.current_limit_ma == milliamps
                {
                    false
                } else {
                    output.current_limit_ma = milliamps;
                    true
                }
            }
            Action::SetRegulationMode { channel, mode } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                if channel < 3
                    || output.transition != OutputTransition::Stable
                    || output.regulation_mode == mode
                {
                    false
                } else {
                    output.regulation_mode = mode;
                    if !output.physical_enabled {
                        output.drive_mv = output.setpoint_mv;
                    }
                    true
                }
            }
            Action::RegulateChannel {
                channel,
                measurement,
            } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                if channel < 3
                    || !output.physical_enabled
                    || output.transition != OutputTransition::Stable
                    || !measurement.valid
                {
                    false
                } else if output.drive_mv > output.setpoint_mv {
                    output.drive_mv = output
                        .drive_mv
                        .saturating_sub(VOLTAGE_SLEW_STEP_MV)
                        .max(output.setpoint_mv);
                    true
                } else if output.regulation_mode == RegulationMode::Cv {
                    if output.drive_mv < output.setpoint_mv {
                        output.drive_mv = output
                            .drive_mv
                            .saturating_add(VOLTAGE_SLEW_STEP_MV)
                            .min(output.setpoint_mv);
                        true
                    } else {
                        false
                    }
                } else {
                    let error_ma =
                        i32::from(output.current_limit_ma) - i32::from(measurement.milliamps);
                    if error_ma.abs() <= 4 {
                        false
                    } else {
                        let step_mv = (error_ma.unsigned_abs() * 2).clamp(10, 200) as u16;
                        let minimum_mv = if channel == 3 { 500 } else { 800 };
                        let drive_mv = if error_ma < 0 {
                            output.drive_mv.saturating_sub(step_mv).max(minimum_mv)
                        } else {
                            output
                                .drive_mv
                                .saturating_add(step_mv)
                                .min(output.setpoint_mv)
                        };
                        if drive_mv == output.drive_mv {
                            false
                        } else {
                            output.drive_mv = drive_mv;
                            true
                        }
                    }
                }
            }
            Action::SetSinkCurrentLimit(milliamps) => {
                if milliamps > 5_000 || state.sink_current_limit_ma == milliamps {
                    false
                } else {
                    next.sink_current_limit_ma = milliamps;
                    true
                }
            }
            Action::SetOutputRequested { channel, enabled } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                if enabled
                    && output.requested_enabled
                    && matches!(output.transition, OutputTransition::Stable)
                {
                    false
                } else {
                    output.operation = output.operation.wrapping_add(1);
                    let operation = output.operation;
                    output.requested_enabled = enabled;
                    if enabled {
                        output.fault = Fault::None;
                        output.transition = OutputTransition::Enabling(operation);
                    } else {
                        output.transition = OutputTransition::Disabling(operation);
                    }
                    true
                }
            }
            Action::OutputApplied {
                channel,
                operation,
                enabled,
            } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                let expected = if enabled {
                    OutputTransition::Enabling(operation)
                } else {
                    OutputTransition::Disabling(operation)
                };
                if output.transition != expected || output.requested_enabled != enabled {
                    false
                } else {
                    output.physical_enabled = enabled;
                    output.transition = OutputTransition::Stable;
                    true
                }
            }
            Action::OutputFailed {
                channel,
                operation,
                fault,
            } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                let matches_operation = matches!(
                    output.transition,
                    OutputTransition::Enabling(value) | OutputTransition::Disabling(value)
                        if value == operation
                );
                if !matches_operation {
                    false
                } else {
                    output.requested_enabled = false;
                    output.physical_enabled = false;
                    output.transition = OutputTransition::Stable;
                    output.fault = fault;
                    true
                }
            }
            Action::ProtectionTrip { channel, fault } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                if !output.requested_enabled
                    && !output.physical_enabled
                    && output.fault == fault
                    && matches!(output.transition, OutputTransition::Stable)
                {
                    false
                } else {
                    output.operation = output.operation.wrapping_add(1);
                    let operation = output.operation;
                    output.requested_enabled = false;
                    output.fault = fault;
                    output.transition = OutputTransition::Disabling(operation);
                    true
                }
            }
            Action::HardwareSettingApplied => false,
            Action::HardwareSettingFailed { channel, fault } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return next;
                };
                output.operation = output.operation.wrapping_add(1);
                output.requested_enabled = false;
                output.physical_enabled = false;
                output.transition = OutputTransition::Stable;
                output.fault = fault;
                true
            }
            Action::Temperature(Some(raw))
                if state.temp_valid && state.temp_sixteenths_c == raw =>
            {
                false
            }
            Action::Temperature(Some(raw)) => {
                next.temp_sixteenths_c = raw;
                next.temp_valid = true;
                true
            }
            Action::Temperature(None) if !state.temp_valid => false,
            Action::Temperature(None) => {
                next.temp_valid = false;
                true
            }
            Action::Measurements(measurements)
                if state
                    .channels
                    .iter()
                    .map(|channel| channel.measurement)
                    .eq(measurements.iter().copied()) =>
            {
                false
            }
            Action::Measurements(measurements) => {
                for (channel, measurement) in next.channels.iter_mut().zip(measurements) {
                    channel.measurement = measurement;
                }
                true
            }
            Action::SinkMeasurement(measurement) if state.sink == measurement => false,
            Action::SinkMeasurement(measurement) => {
                next.sink = measurement;
                true
            }
        };

        if changed {
            next.version = state.version.wrapping_add(1);
        }
        next
    }
}
