use reducto::Reducer;

use crate::{
    limits::{CH5_MAX_VOLTAGE_MV, CH5_MIN_VOLTAGE_MV},
    ui_content::{
        AWG_ITEM_COUNT, HELP_MAX_SCROLL, HELP_SCROLL_STEP, MAIN_MENU_ITEMS, PROFILE_ITEM_COUNT,
        SETTINGS_ITEM_COUNT,
    },
};

const VOLTAGE_SLEW_STEP_MV: u16 = 200;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Screen {
    MainMenu,
    Overview,
    Channel(u8),
    UsbPdInput,
    Awg,
    Settings,
    ProfileSave,
    ProfileLoad,
    PdSource,
    System,
    Help,
}

/// The STUSB4500 exposes at most seven source PDOs.
pub const PD_SOURCE_MAX_PDOS: usize = 7;

pub const NO_PDO: crate::pd::FixedPdo = crate::pd::FixedPdo {
    source_position: 0,
    millivolts: 0,
    milliamps: 0,
};

/// A pending on-device PDO apply; serviced by main.rs (journal write, then
/// STUSB reprofile) the same loop pass it is armed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PdoApply {
    pub millivolts: u16,
    pub milliamps: u16,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TemperatureUnit {
    Celsius,
    Fahrenheit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwgWaveform {
    Square,
    Triangle,
    Ramp,
    Sine,
}

impl AwgWaveform {
    pub const fn max_frequency_millihz(self) -> u32 {
        match self {
            Self::Square => 125_000,
            Self::Triangle | Self::Ramp | Self::Sine => 120_000,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum AwgStatus {
    Stopped,
    StartRequested,
    Starting,
    Running,
    StopRequested,
    Fault,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum AwgSource {
    Builtin,
    Arbitrary,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ArbRunConfig {
    pub channel: u8,
    pub initial_mv: u16,
    pub low_mv: u16,
    pub high_mv: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwgConfig {
    pub channel: u8,
    pub waveform: AwgWaveform,
    pub frequency_millihz: u32,
    pub duty_percent: u8,
    pub low_mv: u16,
    pub high_mv: u16,
}

impl AwgConfig {
    pub const fn default() -> Self {
        Self {
            channel: 3,
            waveform: AwgWaveform::Square,
            frequency_millihz: 1_000,
            duty_percent: 50,
            low_mv: 1_000,
            high_mv: 5_000,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProfileRequest {
    None,
    Save(u8),
    Load(u8),
    FactoryDefaults,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProfileStatus {
    Idle,
    ConfirmSave(u8),
    ConfirmLoad(u8),
    ConfirmDefaults,
    Working,
    Saved(u8),
    Loaded(u8),
    DefaultsLoaded,
    Empty(u8),
    Failed,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegulationMode {
    Cv,
    Cc,
}

impl Screen {
    fn next(self) -> Self {
        match self {
            Self::MainMenu
            | Self::Awg
            | Self::Settings
            | Self::ProfileSave
            | Self::ProfileLoad
            | Self::PdSource
            | Self::System => self,
            Self::Help => self,
            Self::Overview => Self::Channel(0),
            Self::Channel(index) if index < 4 => Self::Channel(index + 1),
            Self::Channel(_) => Self::UsbPdInput,
            Self::UsbPdInput => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::MainMenu
            | Self::Awg
            | Self::Settings
            | Self::ProfileSave
            | Self::ProfileLoad
            | Self::PdSource
            | Self::System => self,
            Self::Help => self,
            Self::Overview => Self::UsbPdInput,
            Self::Channel(index) if index > 0 => Self::Channel(index - 1),
            Self::Channel(_) => Self::Overview,
            Self::UsbPdInput => Self::Channel(4),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadMeasurement {
    pub milliamps_rms: u16,
    pub milliwatts_average: u32,
    pub valid: bool,
}

impl LoadMeasurement {
    pub const INVALID: Self = Self {
        milliamps_rms: 0,
        milliwatts_average: 0,
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
    pub screen: Screen,
    pub focus: ControlFocus,
    pub channels: [ChannelSnapshot; 5],
    pub sink: Measurement,
    pub sink_current_limit_ma: u16,
    pub sink_fault: Fault,
    pub pd_contract: Option<crate::pd::Contract>,
    pub pd_error: Option<crate::pd::PdError>,
    pub pd_negotiating: bool,
    pub temp_sixteenths_c: i16,
    pub temp_valid: bool,
    pub recovery_armed: bool,
    pub reboot_requested: bool,
    pub temperature_unit: TemperatureUnit,
    pub menu_selection: u8,
    pub profile_request: ProfileRequest,
    pub profile_status: ProfileStatus,
    pub profile_present: [bool; 3],
    pub awg: AwgConfig,
    pub awg_source: AwgSource,
    pub arb_run: ArbRunConfig,
    pub awg_status: AwgStatus,
    pub awg_editing: bool,
    pub awg_load: LoadMeasurement,
    pub help_scroll: u8,
    /// Cached source-advertised fixed PDOs for the PD Source screen.
    pub pd_source_pdos: [crate::pd::FixedPdo; PD_SOURCE_MAX_PDOS],
    pub pd_source_count: u8,
    /// True when the cached list needs a fresh capability read (screen entry
    /// or a contract change while on the screen); main.rs performs the read.
    pub pd_source_stale: bool,
    pub pd_source_error: bool,
    /// Armed row index; UI state only, never persisted.
    pub pd_source_armed: Option<u8>,
    /// Requested apply voltage shown as the requested-vs-actual banner.
    pub pd_banner_mv: Option<u16>,
    /// Non-zero while a PDO apply is in flight; journaled so a VBUS hard
    /// reset routes the next boot back to the PD Source screen. 0 = none.
    pub pdo_apply_pending_mv: u16,
    pub pd_apply_request: Option<PdoApply>,
}

impl AppState {
    pub fn new(recovery_armed: bool, temperature: Option<i16>) -> Self {
        let (temp_sixteenths_c, temp_valid) = match temperature {
            Some(raw) => (raw, true),
            None => (0, false),
        };
        Self {
            screen: Screen::MainMenu,
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
            sink_fault: Fault::None,
            pd_contract: None,
            pd_error: None,
            pd_negotiating: false,
            temp_sixteenths_c,
            temp_valid,
            recovery_armed,
            reboot_requested: false,
            temperature_unit: TemperatureUnit::Celsius,
            menu_selection: 0,
            profile_request: ProfileRequest::None,
            profile_status: ProfileStatus::Idle,
            profile_present: [false; 3],
            awg: AwgConfig::default(),
            awg_source: AwgSource::Builtin,
            arb_run: ArbRunConfig {
                channel: 3,
                initial_mv: 500,
                low_mv: 500,
                high_mv: 500,
            },
            awg_status: AwgStatus::Stopped,
            awg_editing: false,
            awg_load: LoadMeasurement::INVALID,
            help_scroll: 0,
            pd_source_pdos: [NO_PDO; PD_SOURCE_MAX_PDOS],
            pd_source_count: 0,
            pd_source_stale: false,
            pd_source_error: false,
            pd_source_armed: None,
            pd_banner_mv: None,
            pdo_apply_pending_mv: 0,
            pd_apply_request: None,
        }
    }

    pub const fn active_awg_channel(&self) -> u8 {
        match self.awg_source {
            AwgSource::Builtin => self.awg.channel,
            AwgSource::Arbitrary => self.arb_run.channel,
        }
    }

    pub const fn active_awg_initial_mv(&self) -> u16 {
        match self.awg_source {
            AwgSource::Builtin => self.awg.low_mv,
            AwgSource::Arbitrary => self.arb_run.initial_mv,
        }
    }

    pub const fn active_awg_bounds(&self) -> (u16, u16) {
        match self.awg_source {
            AwgSource::Builtin => (self.awg.low_mv, self.awg.high_mv),
            AwgSource::Arbitrary => (self.arb_run.low_mv, self.arb_run.high_mv),
        }
    }

    pub fn outputs_inactive(&self) -> bool {
        self.channels
            .iter()
            .all(|output| !output.requested_enabled && !output.physical_enabled)
    }

    pub fn outputs_physically_off(&self) -> bool {
        self.channels.iter().all(|output| !output.physical_enabled)
    }

    pub fn output_transitions_stable(&self) -> bool {
        self.channels
            .iter()
            .all(|output| output.transition == OutputTransition::Stable)
    }

    /// PD Source rows: one per cached PDO, plus Apply and Cancel.
    pub fn pd_source_rows(&self) -> u8 {
        self.pd_source_count + 2
    }

    /// Same admission rule as the USB `SOUR:PDO:SET` path: outputs must be
    /// inactive, plus no competing apply and an idle AWG.
    pub fn pd_source_apply_ready(&self) -> bool {
        self.pd_source_armed.is_some()
            && !self.pd_source_error
            && self.outputs_inactive()
            && self.pd_apply_request.is_none()
            && matches!(self.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
    }
}

#[derive(Clone, Copy)]
pub enum Action {
    NextScreen,
    PreviousScreen,
    GoOverview,
    NavigateBack,
    RequestReboot,
    BootRecoveryStatus(bool),
    NavigateMenu(i8),
    ActivateMenu,
    ProfileOperationFinished(ProfileStatus),
    ApplyProfile(crate::settings::PersistentSettings, ProfileStatus),
    GlobalShutdownApplied,
    GlobalShutdownFailed,
    AdjustAwg(i8),
    ConfigureAwg(AwgConfig),
    RequestAwgStart,
    RequestArbStart {
        channel: u8,
        initial_mv: u16,
        low_mv: u16,
        high_mv: u16,
    },
    AwgStartPrepared,
    AwgStopped,
    AwgSample(u16),
    AwgLoadMeasurement(LoadMeasurement),
    NextControl,
    AdjustFocused(i8),
    ToggleOutputRequested {
        channel: u8,
    },
    SetCurrentLimit {
        channel: u8,
        milliamps: u16,
    },
    SetVoltage {
        channel: u8,
        millivolts: u16,
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
    OutputEnergized {
        channel: u8,
        operation: u16,
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
    SinkProtectionTrip(Fault),
    SinkProtectionRecovered,
    PdNegotiated(crate::pd::Contract),
    PdNegotiationStarted,
    PdFailed(crate::pd::PdError),
    PdSourceListLoaded {
        pdos: [crate::pd::FixedPdo; PD_SOURCE_MAX_PDOS],
        count: u8,
        error: bool,
    },
    PdoApplyFinished(bool),
}

pub struct AppReducer;

impl AppReducer {
    /// A remote waveform start pulls the panel to the AWG screen with the
    /// Start/Stop row highlighted, so the physical stop control is in reach.
    fn show_awg_screen(state: &mut AppState) {
        state.screen = Screen::Awg;
        state.menu_selection = 6;
        state.focus = ControlFocus::None;
    }

    fn enforce_invariants(mut state: AppState) -> AppState {
        if state.awg_status != AwgStatus::Running && state.awg_load.valid {
            state.awg_load = LoadMeasurement::INVALID;
        }
        state
    }
}

impl Reducer for AppReducer {
    type State = AppState;
    type Action = Action;

    fn reduce(state: &Self::State, action: Self::Action) -> Self::State {
        let mut next = *state;
        let _ = match action {
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
            Action::NavigateBack if state.screen == Screen::MainMenu => false,
            Action::NavigateBack => {
                next.focus = ControlFocus::None;
                match state.screen {
                    // The DC detail screens step back to the channel list;
                    // every other screen returns to the main menu.
                    Screen::Channel(_) | Screen::UsbPdInput => {
                        next.screen = Screen::Overview;
                    }
                    _ => {
                        next.screen = Screen::MainMenu;
                        next.menu_selection = 0;
                        next.profile_status = ProfileStatus::Idle;
                        next.pd_source_armed = None;
                        next.pd_banner_mv = None;
                        if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) {
                            next.awg_status = AwgStatus::StopRequested;
                        }
                    }
                }
                true
            }
            Action::NavigateMenu(direction) => {
                if state.screen == Screen::Help {
                    let adjusted = if direction.is_positive() {
                        state
                            .help_scroll
                            .saturating_add(HELP_SCROLL_STEP)
                            .min(HELP_MAX_SCROLL)
                    } else {
                        state.help_scroll.saturating_sub(HELP_SCROLL_STEP)
                    };
                    if adjusted == state.help_scroll {
                        false
                    } else {
                        next.help_scroll = adjusted;
                        true
                    }
                } else {
                    let count = match state.screen {
                        Screen::MainMenu => MAIN_MENU_ITEMS.len() as u8,
                        Screen::Settings => SETTINGS_ITEM_COUNT,
                        Screen::ProfileSave | Screen::ProfileLoad => PROFILE_ITEM_COUNT,
                        Screen::Awg => AWG_ITEM_COUNT,
                        Screen::PdSource => state.pd_source_rows(),
                        Screen::System | Screen::Help => 1,
                        _ => 0,
                    };
                    if direction == 0 || count == 0 {
                        false
                    } else {
                        let selection = if direction < 0 {
                            state.menu_selection.checked_sub(1).unwrap_or(count - 1)
                        } else if state.menu_selection + 1 >= count {
                            0
                        } else {
                            state.menu_selection + 1
                        };
                        if selection == state.menu_selection {
                            false
                        } else {
                            next.menu_selection = selection;
                            next.profile_status = ProfileStatus::Idle;
                            true
                        }
                    }
                }
            }
            Action::ActivateMenu => match state.screen {
                Screen::MainMenu => {
                    next.screen = match state.menu_selection {
                        0 => Screen::Overview,
                        1 => Screen::Awg,
                        2 => Screen::Settings,
                        3 => Screen::PdSource,
                        4 => Screen::System,
                        _ => Screen::Help,
                    };
                    next.menu_selection = 0;
                    next.help_scroll = 0;
                    if next.screen == Screen::PdSource {
                        // Read the capability list at most once per boot: the
                        // Get_Source_Cap it transmits restarts negotiation,
                        // and some sources answer that with a VBUS hard reset
                        // (observed: entering the screen cold-booted the
                        // board). The cache cannot go stale — this VBUS-
                        // powered board cannot survive a source swap. A
                        // failed read still retries on the next entry.
                        next.pd_source_stale = state.pd_source_count == 0;
                        next.pd_source_error = false;
                        next.pd_source_armed = None;
                    }
                    if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) {
                        next.awg_status = AwgStatus::StopRequested;
                    }
                    true
                }
                Screen::Settings => match state.menu_selection {
                    0 => {
                        next.temperature_unit = match state.temperature_unit {
                            TemperatureUnit::Celsius => TemperatureUnit::Fahrenheit,
                            TemperatureUnit::Fahrenheit => TemperatureUnit::Celsius,
                        };
                        true
                    }
                    1 => {
                        next.screen = Screen::ProfileSave;
                        next.menu_selection = 0;
                        true
                    }
                    2 => {
                        next.screen = Screen::ProfileLoad;
                        next.menu_selection = 0;
                        true
                    }
                    3 if state.profile_request == ProfileRequest::None => {
                        if state.profile_status == ProfileStatus::ConfirmDefaults {
                            next.profile_request = ProfileRequest::FactoryDefaults;
                            next.profile_status = ProfileStatus::Working;
                        } else {
                            next.profile_status = ProfileStatus::ConfirmDefaults;
                        }
                        true
                    }
                    4 => {
                        next.screen = Screen::MainMenu;
                        next.menu_selection = 0;
                        true
                    }
                    _ => false,
                },
                Screen::ProfileSave => {
                    if state.menu_selection < 3 && state.profile_request == ProfileRequest::None {
                        let slot = state.menu_selection;
                        if state.profile_present[usize::from(slot)]
                            && state.profile_status != ProfileStatus::ConfirmSave(slot)
                        {
                            next.profile_status = ProfileStatus::ConfirmSave(slot);
                        } else {
                            next.profile_request = ProfileRequest::Save(slot);
                            next.profile_status = ProfileStatus::Working;
                        }
                        true
                    } else if state.menu_selection == 3 {
                        next.screen = Screen::Settings;
                        next.menu_selection = 1;
                        true
                    } else {
                        false
                    }
                }
                Screen::ProfileLoad => {
                    if state.menu_selection < 3 && state.profile_request == ProfileRequest::None {
                        let slot = state.menu_selection;
                        if !state.profile_present[usize::from(slot)] {
                            next.profile_status = ProfileStatus::Empty(slot);
                        } else if state.profile_status == ProfileStatus::ConfirmLoad(slot) {
                            next.profile_request = ProfileRequest::Load(slot);
                            next.profile_status = ProfileStatus::Working;
                        } else {
                            next.profile_status = ProfileStatus::ConfirmLoad(slot);
                        }
                        true
                    } else if state.menu_selection == 3 {
                        next.screen = Screen::Settings;
                        next.menu_selection = 2;
                        true
                    } else {
                        false
                    }
                }
                Screen::Awg => match state.menu_selection {
                    0 if matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) => {
                        next.awg_editing = !state.awg_editing;
                        true
                    }
                    1..=2 | 4..=5
                        if matches!(
                            state.awg_status,
                            AwgStatus::Stopped | AwgStatus::Running | AwgStatus::Fault
                        ) =>
                    {
                        next.awg_editing = !state.awg_editing;
                        true
                    }
                    3 if state.awg.waveform == AwgWaveform::Square
                        && matches!(
                            state.awg_status,
                            AwgStatus::Stopped | AwgStatus::Running | AwgStatus::Fault
                        ) =>
                    {
                        next.awg_editing = !state.awg_editing;
                        true
                    }
                    6 => match state.awg_status {
                        AwgStatus::Stopped if state.pd_apply_request.is_none() => {
                            next.awg_source = AwgSource::Builtin;
                            next.awg_status = AwgStatus::StartRequested;
                            true
                        }
                        AwgStatus::Fault => {
                            // First acknowledge a fault with a confirmed global
                            // shutdown. A later click is the explicit retry.
                            next.awg_status = AwgStatus::StopRequested;
                            true
                        }
                        AwgStatus::Running => {
                            next.awg_status = AwgStatus::StopRequested;
                            true
                        }
                        _ => false,
                    },
                    7 => {
                        next.screen = Screen::MainMenu;
                        next.menu_selection = 0;
                        next.awg_editing = false;
                        if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) {
                            next.awg_status = AwgStatus::StopRequested;
                        }
                        true
                    }
                    _ => false,
                },
                Screen::PdSource => {
                    let count = state.pd_source_count;
                    if state.menu_selection < count {
                        // Click on a PDO row arms (or disarms) it as the
                        // pending choice; nothing is applied yet.
                        next.pd_source_armed = if state.pd_source_armed
                            == Some(state.menu_selection)
                        {
                            None
                        } else {
                            Some(state.menu_selection)
                        };
                        true
                    } else if state.menu_selection == count {
                        // Apply: state only. main.rs journals the pending
                        // record and reprofiles the STUSB off this request.
                        // Readiness implies an armed row exists.
                        if state.pd_source_apply_ready() {
                            let index = state.pd_source_armed.unwrap_or(0);
                            let pdo = state.pd_source_pdos[usize::from(index)];
                            next.pd_apply_request = Some(PdoApply {
                                millivolts: pdo.millivolts,
                                milliamps: pdo.milliamps.min(5_000),
                            });
                            next.pdo_apply_pending_mv = pdo.millivolts;
                            next.pd_banner_mv = Some(pdo.millivolts);
                            true
                        } else {
                            false
                        }
                    } else {
                        // Cancel discards the armed choice and leaves.
                        next.screen = Screen::MainMenu;
                        next.menu_selection = 0;
                        next.pd_source_armed = None;
                        next.pd_banner_mv = None;
                        true
                    }
                }
                Screen::System | Screen::Help => {
                    next.screen = Screen::MainMenu;
                    next.menu_selection = 0;
                    true
                }
                _ => false,
            },
            Action::ProfileOperationFinished(status) => {
                next.profile_request = ProfileRequest::None;
                next.profile_status = status;
                if let ProfileStatus::Saved(slot) = status {
                    if let Some(present) = next.profile_present.get_mut(usize::from(slot)) {
                        *present = true;
                    }
                }
                true
            }
            Action::ApplyProfile(settings, status) => {
                for channel in &mut next.channels {
                    channel.operation = channel.operation.wrapping_add(1);
                    channel.requested_enabled = false;
                    channel.physical_enabled = false;
                    channel.transition = OutputTransition::Stable;
                    channel.fault = Fault::None;
                }
                settings.apply_to(&mut next);
                next.profile_request = ProfileRequest::None;
                next.profile_status = status;
                true
            }
            Action::GlobalShutdownApplied => {
                let mut any_changed = false;
                for channel in &mut next.channels {
                    let changed = channel.requested_enabled
                        || channel.physical_enabled
                        || channel.transition != OutputTransition::Stable;
                    any_changed |= changed;
                    if changed {
                        channel.operation = channel.operation.wrapping_add(1);
                        channel.requested_enabled = false;
                        channel.physical_enabled = false;
                        channel.transition = OutputTransition::Stable;
                    }
                }
                if matches!(state.awg_status, AwgStatus::StopRequested) {
                    next.awg_status = AwgStatus::Stopped;
                    any_changed = true;
                }
                any_changed
            }
            Action::GlobalShutdownFailed => {
                for channel in &mut next.channels {
                    channel.operation = channel.operation.wrapping_add(1);
                    channel.requested_enabled = false;
                    // A failed shutdown must not claim the hardware is off:
                    // `physical_enabled` is left untouched so
                    // `outputs_physically_off()` stays truthful for flash
                    // compaction and boot-seal gating.
                    channel.transition = OutputTransition::Stable;
                    channel.fault = Fault::Hardware;
                }
                next.awg_status = AwgStatus::Fault;
                true
            }
            Action::AdjustAwg(direction) => {
                if direction == 0
                    || !state.awg_editing
                    || !matches!(
                        state.awg_status,
                        AwgStatus::Stopped | AwgStatus::Running | AwgStatus::Fault
                    )
                {
                    false
                } else {
                    match state.menu_selection {
                        0 if matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) => {
                            next.awg.channel = if state.awg.channel == 3 { 4 } else { 3 };
                            let (minimum, maximum) = if next.awg.channel == 3 {
                                (500, 5_000)
                            } else {
                                (CH5_MIN_VOLTAGE_MV, CH5_MAX_VOLTAGE_MV)
                            };
                            next.awg.low_mv = next.awg.low_mv.clamp(minimum, maximum);
                            next.awg.high_mv = next.awg.high_mv.clamp(next.awg.low_mv, maximum);
                            true
                        }
                        1 => {
                            next.awg.waveform = match (state.awg.waveform, direction < 0) {
                                (AwgWaveform::Square, true) => AwgWaveform::Sine,
                                (AwgWaveform::Square, false) => AwgWaveform::Triangle,
                                (AwgWaveform::Triangle, true) => AwgWaveform::Square,
                                (AwgWaveform::Triangle, false) => AwgWaveform::Ramp,
                                (AwgWaveform::Ramp, true) => AwgWaveform::Triangle,
                                (AwgWaveform::Ramp, false) => AwgWaveform::Sine,
                                (AwgWaveform::Sine, true) => AwgWaveform::Ramp,
                                (AwgWaveform::Sine, false) => AwgWaveform::Square,
                            };
                            next.awg.frequency_millihz = next
                                .awg
                                .frequency_millihz
                                .min(next.awg.waveform.max_frequency_millihz());
                            true
                        }
                        2 => {
                            let maximum = state.awg.waveform.max_frequency_millihz();
                            let adjusted =
                                i64::from(state.awg.frequency_millihz) + i64::from(direction) * 100;
                            let adjusted = adjusted.clamp(100, i64::from(maximum)) as u32;
                            if adjusted == state.awg.frequency_millihz {
                                false
                            } else {
                                next.awg.frequency_millihz = adjusted;
                                true
                            }
                        }
                        3 if state.awg.waveform == AwgWaveform::Square => {
                            let adjusted = i16::from(state.awg.duty_percent) + i16::from(direction);
                            let adjusted = adjusted.clamp(1, 99) as u8;
                            if adjusted == state.awg.duty_percent {
                                false
                            } else {
                                next.awg.duty_percent = adjusted;
                                true
                            }
                        }
                        4 => {
                            let minimum =
                                i32::from(crate::limits::adjustable_min_mv(state.awg.channel));
                            let adjusted = i32::from(state.awg.low_mv) + i32::from(direction) * 10;
                            let adjusted =
                                adjusted.clamp(minimum, i32::from(state.awg.high_mv)) as u16;
                            if adjusted == state.awg.low_mv {
                                false
                            } else {
                                next.awg.low_mv = adjusted;
                                true
                            }
                        }
                        5 => {
                            let maximum = if state.awg.channel == 3 {
                                5_000
                            } else {
                                i32::from(CH5_MAX_VOLTAGE_MV)
                            };
                            let adjusted = i32::from(state.awg.high_mv) + i32::from(direction) * 10;
                            let adjusted =
                                adjusted.clamp(i32::from(state.awg.low_mv), maximum) as u16;
                            if adjusted == state.awg.high_mv {
                                false
                            } else {
                                next.awg.high_mv = adjusted;
                                true
                            }
                        }
                        _ => false,
                    }
                }
            }
            Action::ConfigureAwg(config) => {
                let (minimum, maximum) = if config.channel == 3 {
                    (500, 5_000)
                } else if config.channel == 4 {
                    (CH5_MIN_VOLTAGE_MV, CH5_MAX_VOLTAGE_MV)
                } else {
                    return Self::enforce_invariants(next);
                };
                if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
                    || !(100..=config.waveform.max_frequency_millihz())
                        .contains(&config.frequency_millihz)
                    || !(1..=99).contains(&config.duty_percent)
                    || !(minimum..=maximum).contains(&config.low_mv)
                    || !(config.low_mv..=maximum).contains(&config.high_mv)
                {
                    false
                } else {
                    next.awg = config;
                    true
                }
            }
            Action::RequestAwgStart => {
                if state.awg_status != AwgStatus::Stopped || state.pd_apply_request.is_some() {
                    false
                } else {
                    next.awg_source = AwgSource::Builtin;
                    next.awg_editing = false;
                    next.awg_status = AwgStatus::StartRequested;
                    Self::show_awg_screen(&mut next);
                    true
                }
            }
            Action::RequestArbStart {
                channel,
                initial_mv,
                low_mv,
                high_mv,
            } => {
                let (minimum, maximum) = if channel == 3 {
                    (500, 5_000)
                } else if channel == 4 {
                    (CH5_MIN_VOLTAGE_MV, CH5_MAX_VOLTAGE_MV)
                } else {
                    return Self::enforce_invariants(next);
                };
                if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
                    || state.pd_apply_request.is_some()
                    || !(minimum..=maximum).contains(&low_mv)
                    || !(low_mv..=maximum).contains(&high_mv)
                    || !(low_mv..=high_mv).contains(&initial_mv)
                {
                    false
                } else {
                    next.awg_source = AwgSource::Arbitrary;
                    next.arb_run = ArbRunConfig {
                        channel,
                        initial_mv,
                        low_mv,
                        high_mv,
                    };
                    next.awg_editing = false;
                    next.awg_status = AwgStatus::StartRequested;
                    Self::show_awg_screen(&mut next);
                    true
                }
            }
            Action::AwgStartPrepared => {
                if state.awg_status != AwgStatus::StartRequested {
                    false
                } else {
                    let output = &mut next.channels[usize::from(state.active_awg_channel())];
                    output.operation = output.operation.wrapping_add(1);
                    output.drive_mv = state.active_awg_initial_mv();
                    output.requested_enabled = true;
                    output.fault = Fault::None;
                    output.transition = OutputTransition::Enabling(output.operation);
                    next.awg_status = AwgStatus::Starting;
                    true
                }
            }
            Action::AwgStopped => {
                if state.awg_status == AwgStatus::Stopped {
                    false
                } else {
                    next.awg_status = AwgStatus::Stopped;
                    true
                }
            }
            Action::AwgSample(millivolts) => {
                let (low_mv, high_mv) = state.active_awg_bounds();
                if state.awg_status != AwgStatus::Running
                    || !(low_mv..=high_mv).contains(&millivolts)
                {
                    false
                } else {
                    let output = &mut next.channels[usize::from(state.active_awg_channel())];
                    if output.drive_mv == millivolts {
                        false
                    } else {
                        output.drive_mv = millivolts;
                        true
                    }
                }
            }
            Action::AwgLoadMeasurement(_measurement) if state.awg_status != AwgStatus::Running => {
                false
            }
            Action::AwgLoadMeasurement(measurement) if state.awg_load == measurement => false,
            Action::AwgLoadMeasurement(measurement) => {
                next.awg_load = measurement;
                true
            }
            Action::RequestReboot if state.reboot_requested => false,
            Action::RequestReboot => {
                next.reboot_requested = true;
                true
            }
            Action::BootRecoveryStatus(armed) => {
                next.recovery_armed = armed;
                state.recovery_armed != armed
            }
            Action::NextControl => match state.screen {
                Screen::MainMenu
                | Screen::Awg
                | Screen::Settings
                | Screen::ProfileSave
                | Screen::ProfileLoad
                | Screen::PdSource
                | Screen::System => return Self::reduce(state, Action::ActivateMenu),
                Screen::Help => return Self::reduce(state, Action::ActivateMenu),
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
                if direction == 0
                    || !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
                {
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
                                    (i32::from(CH5_MIN_VOLTAGE_MV), i32::from(CH5_MAX_VOLTAGE_MV))
                                };
                                // Preserve 10 mV single-step precision, but give
                                // the wide CH4/CH5 ranges a useful fast-spin rate.
                                // Other editors retain the shared acceleration
                                // unchanged.
                                let step_mv = if direction.unsigned_abs() >= 8 {
                                    25
                                } else {
                                    10
                                };
                                let adjusted =
                                    i32::from(output.setpoint_mv) + i32::from(direction) * step_mv;
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
                                // While physically enabled the drive keeps
                                // slewing toward the setpoint in bounded
                                // steps; snapping it here would emit one
                                // full-swing voltage write.
                                if !output.physical_enabled {
                                    output.drive_mv = output.setpoint_mv;
                                }
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
                if state.pd_apply_request.is_some() {
                    // A PDO apply is in flight; outputs are inactive by its
                    // admission rule and must stay that way until it lands.
                    false
                } else if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) {
                    next.awg_status = AwgStatus::StopRequested;
                    true
                } else if state
                    .channels
                    .iter()
                    .any(|output| output.transition != OutputTransition::Stable)
                {
                    false
                } else {
                    let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                        return Self::enforce_invariants(next);
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
            }
            Action::SetCurrentLimit { channel, milliamps } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
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
            Action::SetVoltage {
                channel,
                millivolts,
            } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
                };
                let in_range = match channel {
                    3 => (crate::limits::CH4_MIN_VOLTAGE_MV..=crate::limits::CH4_MAX_VOLTAGE_MV)
                        .contains(&millivolts),
                    4 => (CH5_MIN_VOLTAGE_MV..=CH5_MAX_VOLTAGE_MV).contains(&millivolts),
                    _ => false,
                };
                if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
                    || output.transition != OutputTransition::Stable
                    || !in_range
                    || output.setpoint_mv == millivolts
                {
                    false
                } else {
                    output.setpoint_mv = millivolts;
                    if !output.physical_enabled {
                        output.drive_mv = millivolts;
                    }
                    true
                }
            }
            Action::SetRegulationMode { channel, mode } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
                };
                if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
                    || channel < 3
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
                    return Self::enforce_invariants(next);
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
                        let minimum_mv = crate::limits::adjustable_min_mv(channel);
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
                if enabled
                    && (state.pd_apply_request.is_some()
                        || state
                            .channels
                            .iter()
                            .any(|output| output.transition != OutputTransition::Stable))
                {
                    return Self::enforce_invariants(next);
                }
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
                };
                if !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault) {
                    if !enabled
                        && state.awg_status == AwgStatus::Running
                        && channel == state.active_awg_channel()
                    {
                        next.awg_status = AwgStatus::StopRequested;
                        true
                    } else {
                        false
                    }
                } else if enabled
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
                    return Self::enforce_invariants(next);
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
                    if enabled
                        && state.awg_status == AwgStatus::Starting
                        && channel == state.active_awg_channel()
                    {
                        next.awg_status = AwgStatus::Running;
                    }
                    true
                }
            }
            Action::OutputEnergized { channel, operation } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
                };
                if output.transition != OutputTransition::Enabling(operation)
                    || !output.requested_enabled
                {
                    false
                } else {
                    output.physical_enabled = true;
                    true
                }
            }
            Action::OutputFailed {
                channel,
                operation,
                fault,
            } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
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
                    if channel == state.active_awg_channel()
                        && !matches!(state.awg_status, AwgStatus::Stopped)
                    {
                        next.awg_status = AwgStatus::Fault;
                    }
                    true
                }
            }
            Action::ProtectionTrip { channel, fault } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
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
                    if channel == state.active_awg_channel()
                        && !matches!(state.awg_status, AwgStatus::Stopped)
                    {
                        next.awg_status = AwgStatus::Fault;
                    }
                    true
                }
            }
            Action::HardwareSettingApplied => false,
            Action::HardwareSettingFailed { channel, fault } => {
                let Some(output) = next.channels.get_mut(usize::from(channel)) else {
                    return Self::enforce_invariants(next);
                };
                // Completion fence: every emitter of this action either shut
                // the channel down first or refused a write without touching
                // hardware. A channel mid-transition is owned by a staged
                // plan whose outcome arrives as a token-checked OutputApplied
                // or OutputFailed; recording "off" here would be a lie.
                if output.transition != OutputTransition::Stable {
                    return Self::enforce_invariants(next);
                }
                output.operation = output.operation.wrapping_add(1);
                output.requested_enabled = false;
                output.physical_enabled = false;
                output.transition = OutputTransition::Stable;
                output.fault = fault;
                if channel == state.active_awg_channel()
                    && !matches!(state.awg_status, AwgStatus::Stopped)
                {
                    next.awg_status = AwgStatus::Fault;
                }
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
            Action::SinkProtectionTrip(fault)
                if fault == Fault::None || state.sink_fault == fault =>
            {
                false
            }
            Action::SinkProtectionTrip(fault) => {
                next.sink_fault = fault;
                if !matches!(state.awg_status, AwgStatus::Stopped) {
                    next.awg_status = AwgStatus::Fault;
                }
                true
            }
            Action::SinkProtectionRecovered if state.sink_fault == Fault::None => false,
            Action::SinkProtectionRecovered => {
                next.sink_fault = Fault::None;
                true
            }
            Action::PdNegotiated(contract)
                if state.pd_contract == Some(contract)
                    && state.pd_error.is_none()
                    && state.pdo_apply_pending_mv == 0 =>
            {
                false
            }
            Action::PdNegotiated(contract) => {
                next.pd_contract = Some(contract);
                next.pd_error = None;
                next.pd_negotiating = false;
                // An in-place renegotiation outcome resolves a pending PDO
                // apply; the cleared flag reaches flash on the next journal
                // write. Deliberately do NOT mark the cached PDO list stale
                // here: the capability read itself transmits Get_Source_Cap,
                // which restarts negotiation and produces exactly this event
                // — re-reading on it is a self-sustaining read/renegotiate
                // loop (observed on hardware). The list refreshes on screen
                // entry only; the ACTIVE marker tracks pd_contract live.
                next.pdo_apply_pending_mv = 0;
                true
            }
            Action::PdNegotiationStarted
                if state.pd_contract.is_none()
                    && state.pd_error.is_none()
                    && state.pd_negotiating =>
            {
                false
            }
            Action::PdNegotiationStarted => {
                next.pd_contract = None;
                next.pd_error = None;
                next.pd_negotiating = true;
                true
            }
            Action::PdFailed(error)
                if state.pd_contract.is_none() && state.pd_error == Some(error) =>
            {
                false
            }
            Action::PdFailed(error) => {
                let contract_lost = state.pd_contract.is_some();
                next.pd_contract = None;
                next.pd_error = Some(error);
                next.pd_negotiating = false;
                next.pdo_apply_pending_mv = 0;
                if contract_lost
                    || state
                        .channels
                        .iter()
                        .any(|output| output.requested_enabled || output.physical_enabled)
                {
                    next.sink_fault = Fault::Hardware;
                    if !matches!(state.awg_status, AwgStatus::Stopped) {
                        next.awg_status = AwgStatus::Fault;
                    }
                }
                true
            }
            Action::PdSourceListLoaded { pdos, count, error } => {
                next.pd_source_pdos = pdos;
                next.pd_source_count = count.min(PD_SOURCE_MAX_PDOS as u8);
                next.pd_source_error = error;
                next.pd_source_stale = false;
                if next
                    .pd_source_armed
                    .is_some_and(|index| index >= next.pd_source_count)
                {
                    next.pd_source_armed = None;
                }
                if state.screen == Screen::PdSource
                    && next.menu_selection >= next.pd_source_rows()
                {
                    next.menu_selection = next.pd_source_rows() - 1;
                }
                true
            }
            Action::PdoApplyFinished(_) if state.pd_apply_request.is_none() => false,
            Action::PdoApplyFinished(ok) => {
                next.pd_apply_request = None;
                if !ok {
                    // Nothing was applied; the cleared flag reaches flash on
                    // the next journal write, so no boot banner appears.
                    next.pdo_apply_pending_mv = 0;
                    next.pd_banner_mv = None;
                    next.pd_source_error = true;
                }
                true
            }
        };

        Self::enforce_invariants(next)
    }
}

#[cfg(test)]
mod tests;
