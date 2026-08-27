//! The foreground loop's service steps, one named function per concern.
//! `main` stays a short orchestration; the policy of each pass lives here.

use benchvolt_pd::app::{Action, AwgSource, AwgStatus, Screen};
use benchvolt_pd::cadence::{Due, ServiceCadence};
use benchvolt_pd::early_shutdown::raw_emergency_shutdown;
use benchvolt_pd::measurement::MeasurementWindows;
use benchvolt_pd::monitoring::{ProtectionService, TpsStatusObservation};
use benchvolt_pd::paint_queue::dma_deadline_reached;
use benchvolt_pd::pd::{Service as PdService, ServiceEvent as PdServiceEvent};
use benchvolt_pd::power::{execute_global_shutdown, Rail};
use benchvolt_pd::settings::{PersistentSettings, SettingsDebouncer};
use benchvolt_pd::usb_command::{output_completion_response, pd_completion_response};
use benchvolt_pd::usb_output::OutputTransaction;
use benchvolt_pd::waveform::{Directive as WaveformDirective, Service as WaveformService};
use stm32f0xx_hal::pac;

use crate::board::adc::AdcBank;
use crate::board::i2c::SoftPdBus;
use crate::boot::{persist_settings, SettingsStore};
use crate::runtime::{confirmed_global_shutdown, dispatch_app};
use crate::types::{FirmwareApp, FirmwarePower, PdI2c};
use crate::usb_transport::queue_usb_response;
use crate::{diagnostics, display_dma, input::monotonic_ms};

/// Loop-owned scalar state that survives across passes.
pub(crate) struct LoopState {
    /// Elapsed time withheld from the PD service while a waveform is running.
    pub pd_deferred_elapsed_ms: u16,
    pub comm_capable_checked: bool,
    /// One-shot deferred journal work (compaction of a full page, clearing a
    /// journaled PDO-apply flag) runs only after the loop proves healthy —
    /// never inside the boot attach window, where a source hard reset could
    /// interrupt the page erase and blank the only settings page.
    pub journal_maintenance_done: bool,
    /// The boot record carried a pdo_apply_pending flag; append the cleared
    /// record once (failure = a sticky banner on the next boot, by design).
    pub pdo_flag_clear_needed: bool,
    pub display_failure_handled: bool,
    /// Capability-read pacing. Get_Source_Cap triggers a renegotiation the
    /// bench charger sometimes answers with a VBUS hard reset, and reads
    /// fired close together (or overlapping the entry repaint / a previous
    /// read's still-settling renegotiation) reboot the board far more often
    /// than the identical read issued alone over USB. So: wait after
    /// entering the screen before the first read, and space the single
    /// retry well past any in-flight PD message sequence.
    pub pd_list_failures: u8,
    pub pd_list_not_before: u16,
    pub was_on_pd_source: bool,
    /// Deferred SOUR:WAVE:CHn:RUN ack: reply only once the builtin engine is
    /// actually running (or has faulted), mirroring the ARB start contract.
    pub pending_awg_ack: Option<u8>,
    pub last_waveform_tick: u16,
    /// True while the PD bus was recently perturbed (a Get_Source_Cap or
    /// sink reprofile was transmitted, or the contract was observed lost):
    /// the renegotiation it triggers may still be in flight, and the source
    /// may answer it with a VBUS hard reset. STUSB NVM writes are refused
    /// until this settles — this is the real signal the (never-set)
    /// `pd_negotiating` flag pretended to be. Cleared wrap-safely every
    /// pass by `pd_step`.
    pub pd_disturbed: bool,
    pub pd_quiet_after: u16,
    /// Armed after a successful PDO apply: if no VBUS reset arrives within
    /// the settle window, the renegotiation resolved in place (possibly to
    /// the identical contract, which emits no PD event) and the journaled
    /// reboot-router flag is cleared via `PdoApplySettled`.
    pub pdo_settle_armed: bool,
    pub pdo_settle_after: u16,
}

/// How long the PD bus is treated as disturbed after a transmission that
/// restarts negotiation. Renegotiation completes within tens of ms; the
/// observed hard-reset responses land well inside a second.
const PD_SETTLE_MS: u16 = 1_000;

impl LoopState {
    pub(crate) fn mark_pd_disturbed(&mut self) {
        self.pd_disturbed = true;
        self.pd_quiet_after = monotonic_ms().wrapping_add(PD_SETTLE_MS);
    }
}

pub(crate) fn monotonic_awg_tick() -> u16 {
    unsafe { (*pac::TIM14::ptr()).cnt.read().cnt().bits() }
}

/// A display DMA failure during real rendering latches has_failed(), which
/// keeps boot fail-closed without requiring a serial cable.
pub(crate) fn render_step(app: &mut FirmwareApp, power: &mut FirmwarePower, ls: &mut LoopState) {
    if display_dma::begin_full_render() {
        app.render_full();
        display_dma::finish_full_render();
    }
    if display_dma::has_failed() && !ls.display_failure_handled {
        ls.display_failure_handled = true;
        // Deliberately harsher than the confirmed idiom: with the display
        // dead the panel cannot show state, so the raw all-off runs even
        // after a successful verified shutdown.
        let _ = confirmed_global_shutdown(app, power);
        unsafe { raw_emergency_shutdown() };
        dispatch_app(app, power, Action::BootRecoveryStatus(false));
    }
}

pub(crate) fn pd_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    pd_bus: &mut PdI2c,
    pd_service: &mut PdService,
    ls: &mut LoopState,
    elapsed_ms: u16,
    awg_hot: bool,
) {
    let now = monotonic_ms();
    // Wrap-safe settle bookkeeping: these run every pass, so each deadline
    // is observed well within its 32.7 s half-range.
    if ls.pd_disturbed && dma_deadline_reached(now, ls.pd_quiet_after) {
        ls.pd_disturbed = false;
    }
    if ls.pdo_settle_armed && dma_deadline_reached(now, ls.pdo_settle_after) {
        ls.pdo_settle_armed = false;
        if app.state().pdo_apply_pending_mv != 0 {
            // The apply survived its settle window without a VBUS reset;
            // clearing the flag in RAM lets the next journal write clear
            // flash, so no spurious banner boot follows a same-contract
            // apply (which produces no PD event at all).
            dispatch_app(app, power, Action::PdoApplySettled);
        }
    }
    let outputs_off = app.state().outputs_inactive();
    ls.pd_deferred_elapsed_ms = ls.pd_deferred_elapsed_ms.saturating_add(elapsed_ms);
    let pd_events = if !awg_hot {
        pd_service.tick(
            core::mem::take(&mut ls.pd_deferred_elapsed_ms),
            now,
            outputs_off,
            app.state().sink_current_limit_ma,
            app.state().sink.valid.then_some(app.state().sink.millivolts),
            &mut SoftPdBus::new(pd_bus, power.delay_mut()),
        )
    } else {
        [None, None]
    };
    for event in pd_events.into_iter().flatten() {
        let (action, pd_event) = match event {
            PdServiceEvent::NegotiationStarted => (Action::PdNegotiationStarted, None),
            PdServiceEvent::Pd(benchvolt_pd::pd::PdEvent::Negotiated(contract)) => (
                Action::PdNegotiated(contract),
                Some(benchvolt_pd::pd::PdEvent::Negotiated(contract)),
            ),
            PdServiceEvent::Pd(benchvolt_pd::pd::PdEvent::Lost(error)) => {
                // Observed contract churn: hold NVM writes until it settles.
                // Only when a contract actually existed — the read-only
                // passive-discovery poll on a non-PD source emits Lost every
                // 500 ms (< the 1 s settle window) and perturbs nothing;
                // marking on it would latch the flag forever and permanently
                // refuse SYST:PD:NEGOTIATE, the legacy-source recovery
                // command whose whole purpose is the no-contract case.
                if app.state().pd_contract.is_some() {
                    ls.mark_pd_disturbed();
                }
                (
                    Action::PdFailed(error),
                    Some(benchvolt_pd::pd::PdEvent::Lost(error)),
                )
            }
        };
        dispatch_app(app, power, action);
        if let Some(pd_event) = pd_event {
            if let Some(result) = pd_service.take_command_completion(pd_event) {
                queue_usb_response(pd_completion_response(result));
            }
        }
    }
}

/// PD Source screen: one bounded capability read per boot (per entry until
/// one succeeds). The read transmits Get_Source_Cap, which restarts
/// negotiation — a brief SINK_READY exit the PD watchdog can see as contract
/// loss, and a request some sources answer with a VBUS hard reset — so it
/// also waits for every output to be inactive, matching Apply's admission
/// rule. Failures render an error row and retry once, well spaced.
pub(crate) fn pd_source_list_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    pd_bus: &mut PdI2c,
    pd_service: &PdService,
    ls: &mut LoopState,
) {
    let on_pd_source = app.state().screen == Screen::PdSource;
    if on_pd_source && !ls.was_on_pd_source {
        ls.pd_list_not_before = monotonic_ms().wrapping_add(400);
        ls.pd_list_failures = 0;
    }
    ls.was_on_pd_source = on_pd_source;
    let gates_open = on_pd_source
        && app.state().pd_source_stale
        && app.state().outputs_inactive()
        && app.state().awg_status != AwgStatus::Running
        && !pd_service.command_pending();
    if !gates_open {
        // Keep the pacing deadline fresh while blocked: the u16 half-range
        // comparison below is only wrap-safe within 32.7 s of arming, and a
        // gate (outputs on, PD command pending) can hold this closed for
        // longer. Re-arming also gives the bus 400 ms of quiet after the
        // blocking condition clears before the read fires.
        ls.pd_list_not_before = monotonic_ms().wrapping_add(400);
        return;
    }
    if dma_deadline_reached(monotonic_ms(), ls.pd_list_not_before) {
        let result = benchvolt_pd::pd::read_source_capabilities(&mut SoftPdBus::new(
            pd_bus,
            power.delay_mut(),
        ));
        // The read (when it transmitted) restarts negotiation; hold NVM
        // writes until the exchange settles.
        ls.mark_pd_disturbed();
        if result.is_err() && ls.pd_list_failures < 1 {
            ls.pd_list_failures += 1;
            ls.pd_list_not_before = monotonic_ms().wrapping_add(1_500);
        } else {
            ls.pd_list_failures = 0;
            let mut pdos = [benchvolt_pd::app::NO_PDO; benchvolt_pd::app::PD_SOURCE_MAX_PDOS];
            let mut count = 0u8;
            let error = match result {
                Ok((raw_pdos, raw_count)) => {
                    for (index, raw) in raw_pdos[..raw_count].iter().enumerate() {
                        // Same filtering as the USB PdList path, plus the
                        // 20 V board input ceiling: never offer a row the
                        // sink cannot request.
                        let Some(pdo) = benchvolt_pd::pd::decode_fixed_pdo(*raw, index as u8 + 1)
                        else {
                            continue;
                        };
                        if pdo.millivolts <= 20_000 && usize::from(count) < pdos.len() {
                            pdos[usize::from(count)] = pdo;
                            count += 1;
                        }
                    }
                    false
                }
                Err(_) => true,
            };
            dispatch_app(app, power, Action::PdSourceListLoaded { pdos, count, error });
        }
    }
}

/// Service a front-panel PDO apply: journal the pending record before
/// touching the STUSB — a downward contract transition cold-boots this
/// VBUS-powered board, and that record is what routes the next boot back to
/// the result banner.
pub(crate) fn pdo_apply_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    pd_bus: &mut PdI2c,
    cadence: &ServiceCadence,
    settings_store: &mut SettingsStore,
    settings_effect: &mut SettingsDebouncer,
    ls: &mut LoopState,
) {
    let Some(request) = app.state().pd_apply_request else {
        return;
    };
    // TRANSIENT conditions defer the apply — the request stays armed and is
    // retried next pass — they must NOT consume it as a failure. The screen
    // entry's own capability read opens the settle window, so an Apply
    // clicked within a second of entry would otherwise always latch the
    // PD ERROR state (observed on hardware). While deferred, the reducer
    // keeps blocking output enables, so the admission cannot rot.
    if ls.pd_disturbed || power.is_busy() {
        return;
    }
    let outputs_physically_off = app.state().outputs_physically_off();
    // HARD conditions fail the apply: the STUSB NVM program below must not
    // run with the contract lost, and outputs cannot have appeared while
    // the request was pending.
    let mut ok = app.state().outputs_inactive() && app.state().pd_contract.is_some();
    if ok {
        let settings = PersistentSettings::from_state(app.state());
        ok = persist_settings(
            settings_store,
            settings,
            outputs_physically_off,
            cadence.healthy_for(3_000),
        );
        if ok {
            settings_effect.mark_saved(settings);
            ok = benchvolt_pd::pd::set_sink_pdo(
                &mut SoftPdBus::new(pd_bus, power.delay_mut()),
                3,
                request.millivolts,
                request.milliamps,
            )
            .is_ok();
        }
    }
    if ok {
        // The reprofile re-advertises capabilities: negotiation in flight,
        // and the source's hard-reset answer can lag by seconds — hold the
        // NVM-write lockout for the same 5 s the settle-timeout below
        // models, not just the standard 1 s window.
        ls.pd_disturbed = true;
        ls.pd_quiet_after = monotonic_ms().wrapping_add(5_000);
        // Arm the settle-timeout completion for the journaled flag (a
        // same-contract renegotiation emits no PD event to clear it).
        ls.pdo_settle_armed = true;
        ls.pdo_settle_after = ls.pd_quiet_after;
    }
    dispatch_app(app, power, Action::PdoApplyFinished(ok));
}

/// One waveform-scheduler tick plus the resulting engine directive.
pub(crate) fn waveform_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    waveform_service: &mut WaveformService,
    ls: &mut LoopState,
) {
    let waveform_status = app.state().awg_status;
    let waveform_source = app.state().awg_source;
    let waveform_config = app.state().awg;
    let waveform_tick = monotonic_awg_tick();
    diagnostics::record_loop_gap(waveform_tick.wrapping_sub(ls.last_waveform_tick));
    ls.last_waveform_tick = waveform_tick;
    let waveform_directive =
        if waveform_status == AwgStatus::Running && waveform_source == AwgSource::Arbitrary {
            crate::arb_runtime::with_buffer(|buffer| {
                waveform_service.tick(
                    waveform_status,
                    waveform_source,
                    waveform_config,
                    waveform_tick,
                    Some(buffer),
                )
            })
        } else {
            waveform_service.tick(
                waveform_status,
                waveform_source,
                waveform_config,
                waveform_tick,
                None,
            )
        };
    crate::arb_runtime::update_status(waveform_service.arb_status());
    match waveform_directive {
        WaveformDirective::None => {}
        WaveformDirective::Sample(millivolts) => {
            dispatch_app(app, power, Action::AwgSample(millivolts));
        }
        WaveformDirective::PrepareStart => {
            if confirmed_global_shutdown(app, power) {
                dispatch_app(app, power, Action::AwgStartPrepared);
            }
        }
        WaveformDirective::Stop => {
            let _ = confirmed_global_shutdown(app, power);
        }
        WaveformDirective::Finished | WaveformDirective::FailSafeShutdown => {
            if confirmed_global_shutdown(app, power) {
                dispatch_app(app, power, Action::AwgStopped);
            }
        }
        WaveformDirective::FaultShutdown => {
            // The fault action was already dispatched by the engine's owner;
            // this only guarantees the hardware followed. State bookkeeping
            // arrives through the usual completion paths.
            if execute_global_shutdown(power).is_err() {
                unsafe { raw_emergency_shutdown() };
            }
        }
    }
}

/// Settle deferred USB acks that wait on the engine actually running.
pub(crate) fn awg_ack_step(
    app: &FirmwareApp,
    waveform_service: &mut WaveformService,
    ls: &mut LoopState,
) {
    if ls.pending_awg_ack.is_some() {
        if app.state().awg_status == AwgStatus::Running
            && app.state().awg_source == AwgSource::Builtin
        {
            queue_usb_response(b"OK:WAVE_STARTED\r\n");
            ls.pending_awg_ack = None;
        } else if matches!(
            app.state().awg_status,
            AwgStatus::Fault | AwgStatus::Stopped
        ) {
            queue_usb_response(b"ERR:HARDWARE\r\n");
            ls.pending_awg_ack = None;
        }
    }

    if let Some(start) = waveform_service.pending_arb_ack() {
        if app.state().awg_status == AwgStatus::Running
            && app.state().awg_source == AwgSource::Arbitrary
        {
            use core::fmt::Write as _;
            let mut response: heapless::String<64> = heapless::String::new();
            write!(
                &mut response,
                "OK:CH{}_ARB_STARTED_PTS:{}\r\n",
                u32::from(start.channel) + 1,
                start.count
            )
            .ok();
            queue_usb_response(response.as_bytes());
            waveform_service.take_pending_arb_ack();
        } else if matches!(
            app.state().awg_status,
            AwgStatus::Fault | AwgStatus::Stopped
        ) {
            queue_usb_response(b"ERR:HARDWARE\r\n");
            waveform_service.take_pending_arb_ack();
        }
    }
}

pub(crate) fn temperature_step(app: &mut FirmwareApp, power: &mut FirmwarePower) {
    let temperature = power.read_temperature();
    dispatch_app(app, power, Action::Temperature(temperature));
    if let Some(fault) = ProtectionService::temperature_fault(temperature) {
        for action in ProtectionService::temperature_trip_actions(app.state(), fault)
            .into_iter()
            .flatten()
        {
            dispatch_app(app, power, action);
        }
        // Belt over the per-channel trip actions above: those carry their
        // own completion tokens, so no global completion is dispatched here.
        let _ = execute_global_shutdown(power);
    }
}

/// The full 20 ms protection and measurement pass.
pub(crate) fn measurement_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    protection: &mut ProtectionService,
    bank: &mut AdcBank,
    measurement_windows: &mut MeasurementWindows,
    cadence: &mut ServiceCadence,
    due: &mut Due,
) {
    for (rail, channels) in [(Rail::Dc1, [0u8, 1]), (Rail::Dc2, [2u8, 3])] {
        let active = channels.into_iter().any(|channel| {
            let output = &app.state().channels[usize::from(channel)];
            output.requested_enabled || output.physical_enabled
        });
        let observation = if active {
            match power.read_rail_status(rail) {
                Ok(status) => TpsStatusObservation::Value(status),
                Err(_) => TpsStatusObservation::ReadError,
            }
        } else {
            TpsStatusObservation::Inactive
        };
        for action in protection
            .observe_shared_status(app.state(), rail, observation)
            .into_iter()
            .flatten()
        {
            dispatch_app(app, power, action);
        }
    }
    let ch5_status = if app.state().channels[4].physical_enabled {
        match power.read_ch5_status() {
            Ok(status) => {
                diagnostics::record_ch5_tps_status(status);
                TpsStatusObservation::Value(status)
            }
            Err(_) => {
                diagnostics::record_ch5_tps_status(0xff);
                TpsStatusObservation::ReadError
            }
        }
    } else {
        TpsStatusObservation::Inactive
    };
    if let Some(action) = protection.observe_ch5_status(app.state(), ch5_status) {
        dispatch_app(app, power, action);
    }
    let measurements = bank.read_outputs();
    let sink_measurement = bank.read_sink();
    for rail in [Rail::Dc1, Rail::Dc2] {
        for action in protection
            .observe_shared_current(app.state(), &measurements, rail)
            .into_iter()
            .flatten()
        {
            dispatch_app(app, power, action);
        }
    }
    if let Some(action) = protection.observe_sink(app.state(), sink_measurement) {
        dispatch_app(app, power, action);
    }
    if !measurement_windows.record(app.state(), measurements, sink_measurement) {
        cadence.invalidate_awg_window(due);
    }
    for channel in 0..5u8 {
        let measurement = measurements[usize::from(channel)];
        if let Some(action) = protection.observe_channel(app.state(), channel, measurement) {
            dispatch_app(app, power, action);
        }
    }
    for channel in 3..=4u8 {
        if app.state().awg_status != AwgStatus::Running
            || channel != app.state().active_awg_channel()
        {
            dispatch_app(
                app,
                power,
                Action::RegulateChannel {
                    channel,
                    measurement: measurements[usize::from(channel)],
                },
            );
        }
    }
}

pub(crate) fn display_measurement_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    measurement_windows: &mut MeasurementWindows,
) {
    let (measurements, sink_measurement) = measurement_windows.take_display();
    dispatch_app(app, power, Action::Measurements(measurements));
    dispatch_app(app, power, Action::SinkMeasurement(sink_measurement));
}

/// Advance one bounded power-executor stage and settle its USB transaction.
pub(crate) fn executor_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    usb_output: &mut OutputTransaction,
) {
    if let Some(action) = power.service(monotonic_ms(), app.state()) {
        dispatch_app(app, power, action);
        if let Some(result) = usb_output.observe_completion(&action) {
            queue_usb_response(output_completion_response(result));
        }
    }
    if usb_output.cancel_if_idle(power.is_busy()) {
        queue_usb_response(b"ERR:CANCELLED\r\n");
    }
}

pub(crate) fn persistence_step(
    app: &FirmwareApp,
    settings_effect: &mut SettingsDebouncer,
    settings_store: &mut SettingsStore,
    elapsed_ms: u16,
    allow_compaction: bool,
) {
    let current_settings = PersistentSettings::from_state(app.state());
    let outputs_stable = app.state().output_transitions_stable();
    let outputs_physically_off = app.state().outputs_physically_off();
    if let Some(settings) = settings_effect.tick(
        current_settings,
        outputs_stable,
        outputs_physically_off,
        elapsed_ms,
    ) {
        if persist_settings(
            settings_store,
            settings,
            outputs_physically_off,
            allow_compaction,
        ) {
            settings_effect.mark_saved(settings);
        }
    }
}

/// One-shot maintenance once the loop has proven healthy for three seconds
/// with every output physically off — i.e. safely outside the boot attach
/// window:
/// 1. compact a full settings journal (deferred from boot: an interrupted
///    page erase would blank the only settings page);
/// 2. append the cleared record for a journaled PDO-apply flag (one
///    attempt; failure leaves a sticky banner on the next boot, which the
///    design doc calls annoying but safe);
/// 3. the STUSB4500 USB_COMM_CAPABLE NVM check, so PD requests declare USB
///    data support and macOS keeps the port's data connection alive.
pub(crate) fn maintenance_step(
    app: &mut FirmwareApp,
    power: &mut FirmwarePower,
    pd_bus: &mut PdI2c,
    cadence: &ServiceCadence,
    ls: &mut LoopState,
    settings_store: &mut SettingsStore,
    settings_effect: &mut SettingsDebouncer,
) {
    if !cadence.healthy_for(3_000) || !app.state().outputs_physically_off() {
        return;
    }
    if !ls.journal_maintenance_done {
        ls.journal_maintenance_done = true;
        if settings_store.next_slot >= crate::boot::SETTINGS_SLOTS {
            let _ = crate::boot::compact_settings_store(settings_store);
        }
        if ls.pdo_flag_clear_needed {
            ls.pdo_flag_clear_needed = false;
            // Live state carries pdo_apply_pending_mv = 0 (apply_to never
            // restores it), so this record is the cleared one.
            let settings = PersistentSettings::from_state(app.state());
            if persist_settings(settings_store, settings, true, true) {
                settings_effect.mark_saved(settings);
                // The apply-in-progress note is out of flash, so an
                // automatic capability read can no longer boot-loop a
                // source that hard-resets on Get_Source_Cap: reload the
                // list the reboot landed the user in front of.
                dispatch_app(app, power, Action::PdSourceRefresh);
            }
        }
    }
    // The comm-capable check writes STUSB NVM: require an established,
    // settled contract too, not just uptime — the attach churn on a
    // hard-resetting source can outlast the health window.
    if !ls.comm_capable_checked
        && app.state().temp_valid
        && app.state().pd_contract.is_some()
        && !ls.pd_disturbed
        && display_dma::ready_for_seal()
    {
        ls.comm_capable_checked = true;
        let _ = benchvolt_pd::pd::configure_usb_comm_capable(&mut SoftPdBus::new(
            pd_bus,
            power.delay_mut(),
        ));
    }
}
