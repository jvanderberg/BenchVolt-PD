use super::*;
use crate::settings::PersistentSettings;

#[test]
fn failed_global_shutdown_does_not_claim_outputs_are_physically_off() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.channels[2].requested_enabled = true;
    state.channels[2].physical_enabled = true;

    let next = AppReducer::reduce(&state, Action::GlobalShutdownFailed);

    assert!(next.channels[2].physical_enabled);
    assert!(!next.channels[2].requested_enabled);
    assert!(next.channels[2].fault == Fault::Hardware);
    assert!(!next.outputs_physically_off());
    assert!(next.awg_status == AwgStatus::Fault);
}

#[test]
fn ui_regulation_mode_toggle_keeps_the_live_drive_slewing() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.screen = Screen::Channel(4);
    state.focus = ControlFocus::RegulationMode;
    state.channels[4].physical_enabled = true;
    state.channels[4].setpoint_mv = 12_000;
    state.channels[4].drive_mv = 1_000;

    let toggled = AppReducer::reduce(&state, Action::AdjustFocused(1));

    assert!(toggled.channels[4].regulation_mode == RegulationMode::Cc);
    assert_eq!(toggled.channels[4].drive_mv, 1_000);

    // While off there is no physical drive to protect; snap to setpoint
    // like the USB SetRegulationMode path.
    state.channels[4].physical_enabled = false;
    let toggled = AppReducer::reduce(&state, Action::AdjustFocused(1));
    assert_eq!(toggled.channels[4].drive_mv, 12_000);
}

#[test]
fn cc_regulation_floor_uses_the_shared_channel_minimum_constants() {
    assert_eq!(crate::limits::adjustable_min_mv(3), 500);
    assert_eq!(crate::limits::adjustable_min_mv(4), 800);
}

#[test]
fn failed_boot_seal_restore_revokes_the_safe_recovery_status() {
    let state = AppState::new(true, Some(25 * 16));
    let next = AppReducer::reduce(&state, Action::BootRecoveryStatus(false));

    assert!(!next.recovery_armed);
}

#[test]
fn reducer_enforces_awg_channel_ownership_for_every_input_path() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.awg_status = AwgStatus::Running;
    state.awg_source = AwgSource::Builtin;
    state.awg.channel = 3;

    let enabled = AppReducer::reduce(
        &state,
        Action::SetOutputRequested {
            channel: 0,
            enabled: true,
        },
    );
    assert!(enabled == state);

    let mode = AppReducer::reduce(
        &state,
        Action::SetRegulationMode {
            channel: 3,
            mode: RegulationMode::Cc,
        },
    );
    assert!(mode == state);

    let stopped = AppReducer::reduce(
        &state,
        Action::SetOutputRequested {
            channel: 3,
            enabled: false,
        },
    );
    assert!(stopped.awg_status == AwgStatus::StopRequested);
    assert!(stopped.channels == state.channels);
}

#[test]
fn remote_voltage_setpoint_uses_the_same_reducer_safety_boundary() {
    let mut state = AppState::new(true, Some(25 * 16));
    let changed = AppReducer::reduce(
        &state,
        Action::SetVoltage {
            channel: 4,
            millivolts: 9_000,
        },
    );
    assert_eq!(changed.channels[4].setpoint_mv, 9_000);
    assert_eq!(changed.channels[4].drive_mv, 9_000);

    state.awg_status = AwgStatus::Running;
    let blocked = AppReducer::reduce(
        &state,
        Action::SetVoltage {
            channel: 4,
            millivolts: 9_000,
        },
    );
    assert!(blocked == state);
}

#[test]
fn boot_opens_main_menu_without_enabling_hardware() {
    let state = AppState::new(true, Some(25 * 16));
    assert!(state.screen == Screen::MainMenu);
    assert!(state.outputs_inactive());
}

#[test]
fn back_navigation_is_hierarchical_on_the_dc_screens() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::Channel(2);
    state.focus = ControlFocus::CurrentLimit;
    state = AppReducer::reduce(&state, Action::NavigateBack);
    assert!(state.screen == Screen::Overview);
    assert!(state.focus == ControlFocus::None);
    state = AppReducer::reduce(&state, Action::NavigateBack);
    assert!(state.screen == Screen::MainMenu);
    assert!(AppReducer::reduce(&state, Action::NavigateBack) == state);

    state.screen = Screen::UsbPdInput;
    state = AppReducer::reduce(&state, Action::NavigateBack);
    assert!(state.screen == Screen::Overview);

    // Menu-family screens still return straight to the main menu.
    state.screen = Screen::PdSource;
    state = AppReducer::reduce(&state, Action::NavigateBack);
    assert!(state.screen == Screen::MainMenu);
}

#[test]
fn dc_screen_cycle_keeps_pd_diagnostics_last() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.screen = Screen::Overview;
    for expected in [
        Screen::Channel(0),
        Screen::Channel(1),
        Screen::Channel(2),
        Screen::Channel(3),
        Screen::Channel(4),
        Screen::UsbPdInput,
        Screen::Overview,
    ] {
        state = AppReducer::reduce(&state, Action::NextScreen);
        assert!(state.screen == expected);
    }
}

#[test]
fn main_menu_routes_without_enabling_hardware() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.screen = Screen::MainMenu;
    let dc = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(dc.screen == Screen::Overview);
    assert!(dc.channels.iter().all(|channel| !channel.requested_enabled));

    state.menu_selection = 1;
    let awg = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(awg.screen == Screen::Awg);
    assert!(awg
        .channels
        .iter()
        .all(|channel| !channel.requested_enabled));
}

#[test]
fn help_scroll_moves_five_lines_and_clamps_at_both_ends() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::Help;

    state = AppReducer::reduce(&state, Action::NavigateMenu(1));
    assert_eq!(state.help_scroll, 5);
    state = AppReducer::reduce(&state, Action::NavigateMenu(12));
    assert_eq!(state.help_scroll, HELP_MAX_SCROLL.min(10));
    state.help_scroll = 25;
    state = AppReducer::reduce(&state, Action::NavigateMenu(1));
    assert_eq!(state.help_scroll, HELP_MAX_SCROLL);
    state = AppReducer::reduce(&state, Action::NavigateMenu(-1));
    assert_eq!(state.help_scroll, HELP_MAX_SCROLL - HELP_SCROLL_STEP);
    state.help_scroll = 0;
    state = AppReducer::reduce(&state, Action::NavigateMenu(-1));
    assert_eq!(state.help_scroll, 0);
}

#[test]
fn applying_a_profile_can_never_restore_energized_state() {
    let mut state = AppState::new(true, Some(25 * 16));
    for channel in &mut state.channels {
        channel.requested_enabled = true;
        channel.physical_enabled = true;
    }
    let mut configured = state;
    configured.channels[3].setpoint_mv = 2_400;
    configured.channels[4].setpoint_mv = 8_000;
    configured.temperature_unit = TemperatureUnit::Fahrenheit;
    let settings = PersistentSettings::from_state(&configured);

    let loaded = AppReducer::reduce(
        &state,
        Action::ApplyProfile(settings, ProfileStatus::Loaded(0)),
    );
    assert!(loaded.channels.iter().all(|channel| {
        !channel.requested_enabled
            && !channel.physical_enabled
            && channel.transition == OutputTransition::Stable
    }));
    assert_eq!(loaded.channels[3].setpoint_mv, 2_400);
    assert_eq!(loaded.channels[4].setpoint_mv, 8_000);
    assert!(loaded.temperature_unit == TemperatureUnit::Fahrenheit);
}

#[test]
fn invalid_action_guard_still_runs_shared_invariant_cleanup() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.awg_load = LoadMeasurement {
        milliamps_rms: 100,
        milliwatts_average: 500,
        valid: true,
    };
    let next = AppReducer::reduce(
        &state,
        Action::SetCurrentLimit {
            channel: 99,
            milliamps: 100,
        },
    );
    assert!(next.awg_load == LoadMeasurement::INVALID);
}

#[test]
fn sink_fault_latches_and_stops_awg_until_explicit_recovery() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.awg_status = AwgStatus::Running;

    let tripped = AppReducer::reduce(&state, Action::SinkProtectionTrip(Fault::OverCurrent));
    assert_eq!(tripped.sink_fault, Fault::OverCurrent);
    assert!(tripped.awg_status == AwgStatus::Fault);

    let unchanged =
        AppReducer::reduce(&tripped, Action::SinkProtectionTrip(Fault::OverCurrent));
    assert!(unchanged == tripped);

    let recovered = AppReducer::reduce(&tripped, Action::SinkProtectionRecovered);
    assert_eq!(recovered.sink_fault, Fault::None);
    assert!(recovered.awg_status == AwgStatus::Fault);
}

#[test]
fn losing_a_negotiated_pd_contract_latches_input_fault() {
    let mut state = AppState::new(true, Some(25 * 16));
    state.pd_contract = Some(crate::pd::Contract {
        source_position: 2,
        millivolts: 9_000,
        operating_milliamps: 3_000,
        maximum_milliamps: 3_000,
    });
    let lost = AppReducer::reduce(&state, Action::PdFailed(crate::pd::PdError::Detached));
    assert!(lost.pd_contract.is_none());
    assert_eq!(lost.pd_error, Some(crate::pd::PdError::Detached));
    assert_eq!(lost.sink_fault, Fault::Hardware);
}

#[test]
fn pd_negotiation_activity_is_explicit_and_terminal() {
    let app = AppState::new(false, Some(400));
    assert!(!app.pd_negotiating);

    let negotiating = AppReducer::reduce(&app, Action::PdNegotiationStarted);
    assert!(negotiating.pd_negotiating);
    assert!(negotiating.pd_contract.is_none());
    assert!(negotiating.pd_error.is_none());

    let failed =
        AppReducer::reduce(&negotiating, Action::PdFailed(crate::pd::PdError::Detached));
    assert!(!failed.pd_negotiating);
    assert_eq!(failed.pd_error, Some(crate::pd::PdError::Detached));

    let restarted = AppReducer::reduce(&failed, Action::PdNegotiationStarted);
    let contract = crate::pd::Contract {
        source_position: 3,
        millivolts: 20_000,
        operating_milliamps: 1_500,
        maximum_milliamps: 3_000,
    };
    let ready = AppReducer::reduce(&restarted, Action::PdNegotiated(contract));
    assert!(!ready.pd_negotiating);
    assert_eq!(ready.pd_contract, Some(contract));
    assert!(ready.pd_error.is_none());
}

#[test]
fn destructive_profile_actions_require_confirmation() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::ProfileLoad;
    state.profile_present[0] = true;
    let confirm = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(confirm.profile_status == ProfileStatus::ConfirmLoad(0));
    assert!(confirm.profile_request == ProfileRequest::None);
    let requested = AppReducer::reduce(&confirm, Action::ActivateMenu);
    assert!(requested.profile_request == ProfileRequest::Load(0));

    let mut state = AppState::new(true, None);
    state.screen = Screen::Settings;
    state.menu_selection = 3;
    let confirm = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(confirm.profile_status == ProfileStatus::ConfirmDefaults);
    assert!(confirm.profile_request == ProfileRequest::None);
    let requested = AppReducer::reduce(&confirm, Action::ActivateMenu);
    assert!(requested.profile_request == ProfileRequest::FactoryDefaults);
}

#[test]
fn awg_configuration_and_start_are_pure_requested_state() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::Awg;
    state = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(state.awg_editing);
    state = AppReducer::reduce(&state, Action::AdjustAwg(1));
    assert_eq!(state.awg.channel, 4);
    assert!(state
        .channels
        .iter()
        .all(|channel| !channel.requested_enabled));

    state.awg_editing = false;
    state.menu_selection = 6;
    state = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(state.awg_status == AwgStatus::StartRequested);
    assert!(state
        .channels
        .iter()
        .all(|channel| !channel.requested_enabled));

    state = AppReducer::reduce(&state, Action::AwgStartPrepared);
    let operation = state.channels[4].operation;
    assert!(state.awg_status == AwgStatus::Starting);
    assert!(state.channels[4].transition == OutputTransition::Enabling(operation));
    state = AppReducer::reduce(
        &state,
        Action::OutputApplied {
            channel: 4,
            operation,
            enabled: true,
        },
    );
    assert!(state.awg_status == AwgStatus::Running);
    state = AppReducer::reduce(&state, Action::AwgSample(2_000));
    assert_eq!(state.channels[4].drive_mv, 2_000);
}

#[test]
fn running_awg_accepts_live_parameter_edits_but_keeps_channel_ownership() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::Awg;
    state.awg_status = AwgStatus::Running;
    state.awg_editing = true;
    state.menu_selection = 2;
    let adjusted = AppReducer::reduce(&state, Action::AdjustAwg(16));
    assert_eq!(adjusted.awg.frequency_millihz, 2_600);

    state.menu_selection = 0;
    let unchanged = AppReducer::reduce(&state, Action::AdjustAwg(1));
    assert_eq!(unchanged.awg.channel, state.awg.channel);
    assert!(unchanged == state);
}

#[test]
fn square_duty_is_live_clamped_and_inert_for_other_waveforms() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::Awg;
    state.awg_status = AwgStatus::Running;
    state.awg_editing = true;
    state.menu_selection = 3;

    let adjusted = AppReducer::reduce(&state, Action::AdjustAwg(12));
    assert_eq!(adjusted.awg.duty_percent, 62);
    let clamped = AppReducer::reduce(&adjusted, Action::AdjustAwg(100));
    assert_eq!(clamped.awg.duty_percent, 99);

    state.awg.waveform = AwgWaveform::Triangle;
    let unchanged = AppReducer::reduce(&state, Action::AdjustAwg(1));
    assert!(unchanged == state);
    let click = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(click.screen == Screen::Awg);
    assert!(click == state);
}

#[test]
fn shaped_awg_waveforms_allow_120_hz() {
    for waveform in [AwgWaveform::Triangle, AwgWaveform::Ramp, AwgWaveform::Sine] {
        assert_eq!(waveform.max_frequency_millihz(), 120_000);
    }
    assert_eq!(AwgWaveform::Square.max_frequency_millihz(), 125_000);
}

#[test]
fn voltage_edit_keeps_fine_steps_and_boosts_only_fast_spin() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::Channel(4);
    state.focus = ControlFocus::Voltage;

    let fine = AppReducer::reduce(&state, Action::AdjustFocused(1));
    assert_eq!(fine.channels[4].setpoint_mv, 12_010);

    let fast = AppReducer::reduce(&fine, Action::AdjustFocused(16));
    assert_eq!(fast.channels[4].setpoint_mv, 12_410);

    let reverse = AppReducer::reduce(&fast, Action::AdjustFocused(-16));
    assert_eq!(reverse.channels[4].setpoint_mv, 12_010);
}

#[test]
fn remote_arb_request_is_pure_and_preserves_builtin_configuration() {
    let state = AppState::new(true, None);
    let requested = AppReducer::reduce(
        &state,
        Action::RequestArbStart {
            channel: 4,
            initial_mv: 900,
            low_mv: 800,
            high_mv: 2_000,
        },
    );
    assert!(requested.awg_source == AwgSource::Arbitrary);
    assert!(requested.awg_status == AwgStatus::StartRequested);
    assert!(requested.screen == Screen::Awg);
    assert_eq!(requested.menu_selection, 6);
    assert_eq!(requested.active_awg_channel(), 4);
    assert_eq!(requested.active_awg_initial_mv(), 900);
    assert_eq!(requested.active_awg_bounds(), (800, 2_000));
    assert!(requested.awg == state.awg);
    assert!(
        PersistentSettings::from_state(&requested) == PersistentSettings::from_state(&state)
    );
    assert!(requested
        .channels
        .iter()
        .all(|channel| !channel.requested_enabled && !channel.physical_enabled));

    let prepared = AppReducer::reduce(&requested, Action::AwgStartPrepared);
    assert!(prepared.channels[4].requested_enabled);
    assert_eq!(prepared.channels[4].drive_mv, 900);
    assert!(!prepared.channels[3].requested_enabled);

    let mut running = prepared;
    running.awg_status = AwgStatus::Running;
    let ui_toggle = AppReducer::reduce(&running, Action::ToggleOutputRequested { channel: 0 });
    assert!(ui_toggle.awg_status == AwgStatus::StopRequested);
    assert!(!ui_toggle.channels[0].requested_enabled);
}

#[test]
fn remote_awg_configure_validates_and_applies() {
    let state = AppState::new(true, None);
    let config = AwgConfig {
        channel: 4,
        waveform: AwgWaveform::Sine,
        frequency_millihz: 60_000,
        duty_percent: 50,
        low_mv: 1_000,
        high_mv: 12_000,
    };
    let configured = AppReducer::reduce(&state, Action::ConfigureAwg(config));
    assert!(configured.awg == config);

    // Out-of-range fields leave the configuration untouched.
    for bad in [
        AwgConfig {
            frequency_millihz: 125_000,
            ..config
        },
        AwgConfig {
            frequency_millihz: 0,
            ..config
        },
        AwgConfig {
            duty_percent: 0,
            ..config
        },
        AwgConfig {
            low_mv: 700,
            ..config
        },
        AwgConfig {
            high_mv: 22_100,
            ..config
        },
        AwgConfig {
            low_mv: 5_000,
            high_mv: 4_000,
            ..config
        },
        AwgConfig {
            channel: 2,
            ..config
        },
    ] {
        let rejected = AppReducer::reduce(&configured, Action::ConfigureAwg(bad));
        assert!(rejected.awg == config);
    }

    // Square accepts its slightly higher 125 Hz ceiling; CH4 bounds apply.
    let square = AwgConfig {
        channel: 3,
        waveform: AwgWaveform::Square,
        frequency_millihz: 125_000,
        duty_percent: 25,
        low_mv: 500,
        high_mv: 5_000,
    };
    let configured = AppReducer::reduce(&state, Action::ConfigureAwg(square));
    assert!(configured.awg == square);

    // Reconfiguration is rejected while a run is active.
    let mut running = configured;
    running.awg_status = AwgStatus::Running;
    let rejected = AppReducer::reduce(&running, Action::ConfigureAwg(config));
    assert!(rejected.awg == square);
}

#[test]
fn remote_awg_start_mirrors_the_panel_start_semantics() {
    let state = AppState::new(true, None);
    let requested = AppReducer::reduce(&state, Action::RequestAwgStart);
    assert!(requested.awg_source == AwgSource::Builtin);
    assert!(requested.awg_status == AwgStatus::StartRequested);
    assert_eq!(requested.active_awg_channel(), state.awg.channel);
    // The panel follows a remote start to the AWG screen, landing on the
    // Start/Stop row so the physical stop control is immediately usable.
    assert!(requested.screen == Screen::Awg);
    assert_eq!(requested.menu_selection, 6);

    // A faulted engine requires an explicit stop acknowledgement first,
    // and an active run cannot be restarted.
    for status in [
        AwgStatus::Fault,
        AwgStatus::Running,
        AwgStatus::StartRequested,
        AwgStatus::Starting,
        AwgStatus::StopRequested,
    ] {
        let mut busy = state;
        busy.awg_status = status;
        let unchanged = AppReducer::reduce(&busy, Action::RequestAwgStart);
        assert!(unchanged.awg_status == status);
    }
}

#[test]
fn pd_source_screen_arms_applies_and_cancels_under_the_outputs_off_rule() {
    let mut state = AppState::new(true, None);
    state.menu_selection = 3;
    state = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(state.screen == Screen::PdSource);
    assert!(state.pd_source_stale);

    let mut pdos = [NO_PDO; PD_SOURCE_MAX_PDOS];
    pdos[0] = crate::pd::FixedPdo {
        source_position: 1,
        millivolts: 9_000,
        milliamps: 3_000,
    };
    state = AppReducer::reduce(
        &state,
        Action::PdSourceListLoaded {
            pdos,
            count: 1,
            error: false,
        },
    );
    assert!(!state.pd_source_stale);
    assert_eq!(state.pd_source_rows(), 3);

    // Click on the row arms it; a second click disarms.
    state = AppReducer::reduce(&state, Action::ActivateMenu);
    assert_eq!(state.pd_source_armed, Some(0));
    let disarmed = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(disarmed.pd_source_armed.is_none());

    // Apply with a live output is a rejected no-op.
    state.menu_selection = 1;
    state.channels[0].physical_enabled = true;
    let rejected = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(rejected == state);
    state.channels[0].physical_enabled = false;

    let applied = AppReducer::reduce(&state, Action::ActivateMenu);
    assert_eq!(
        applied.pd_apply_request,
        Some(PdoApply {
            millivolts: 9_000,
            milliamps: 3_000,
        })
    );
    assert_eq!(applied.pdo_apply_pending_mv, 9_000);
    assert_eq!(applied.pd_banner_mv, Some(9_000));

    // A failed apply clears the journal flag and banner and shows the
    // error; a stray completion with nothing pending changes nothing.
    let failed = AppReducer::reduce(&applied, Action::PdoApplyFinished(false));
    assert!(failed.pd_apply_request.is_none());
    assert_eq!(failed.pdo_apply_pending_mv, 0);
    assert!(failed.pd_banner_mv.is_none());
    assert!(failed.pd_source_error);
    assert!(AppReducer::reduce(&failed, Action::PdoApplyFinished(false)) == failed);

    // Cancel discards and leaves.
    state.menu_selection = 2;
    let cancelled = AppReducer::reduce(&state, Action::ActivateMenu);
    assert!(cancelled.screen == Screen::MainMenu);
    assert!(cancelled.pd_source_armed.is_none());

    // Re-entry with a cached list must not mark it stale: the read's
    // Get_Source_Cap restarts negotiation and can VBUS-reset the board,
    // so it runs at most once per boot.
    let mut reentry = cancelled;
    reentry.menu_selection = 3;
    let reentry = AppReducer::reduce(&reentry, Action::ActivateMenu);
    assert!(reentry.screen == Screen::PdSource);
    assert!(!reentry.pd_source_stale);
}

#[test]
fn pd_source_list_reload_clamps_the_armed_row_and_cursor() {
    let mut state = AppState::new(true, None);
    state.screen = Screen::PdSource;
    state.pd_source_count = 3;
    state.pd_source_armed = Some(2);
    state.menu_selection = 4;

    let reloaded = AppReducer::reduce(
        &state,
        Action::PdSourceListLoaded {
            pdos: [NO_PDO; PD_SOURCE_MAX_PDOS],
            count: 0,
            error: true,
        },
    );
    assert!(reloaded.pd_source_armed.is_none());
    assert_eq!(reloaded.menu_selection, 1);
    assert!(reloaded.pd_source_error);
    assert!(!reloaded.pd_source_apply_ready());
}

#[test]
fn output_safety_predicates_distinguish_requested_physical_and_transition_state() {
    let mut state = AppState::new(true, None);
    assert!(state.outputs_inactive());
    assert!(state.outputs_physically_off());
    assert!(state.output_transitions_stable());

    state.channels[0].requested_enabled = true;
    state.channels[0].transition = OutputTransition::Enabling(1);
    assert!(!state.outputs_inactive());
    assert!(state.outputs_physically_off());
    assert!(!state.output_transitions_stable());

    state.channels[0].requested_enabled = false;
    state.channels[0].physical_enabled = true;
    state.channels[0].transition = OutputTransition::Stable;
    assert!(!state.outputs_inactive());
    assert!(!state.outputs_physically_off());
    assert!(state.output_transitions_stable());
}
