pub(crate) use benchvolt_pd::dispatch::dispatch_app;
use benchvolt_pd::{
    app::{
        Action, AppReducer, AppState, AwgStatus, Fault, ProfileRequest, ProfileStatus,
        RegulationMode,
    },
    power::{execute_global_shutdown, FirmwareEffectPlanner, PowerDriver, PowerExecutor},
    settings::{PersistentSettings, RecordKind},
};
use reducto::EffectApp;

use crate::boot::{persist_settings_record, SettingsStore};

/// The single confirmed global-shutdown idiom (README invariant #5): run the
/// verified all-off sequence; on failure escalate to the raw register-level
/// emergency shutdown; dispatch the matching completion either way so state
/// never lies about the hardware. Every synchronous shutdown site funnels
/// through here — returns whether the verified shutdown succeeded so callers
/// can choose their follow-up (USB reply, next sequence step).
pub(crate) fn confirmed_global_shutdown<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
) -> bool
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    if execute_global_shutdown(power_driver).is_ok() {
        dispatch_app(app, power_driver, Action::GlobalShutdownApplied);
        true
    } else {
        unsafe { benchvolt_pd::early_shutdown::raw_emergency_shutdown() };
        dispatch_app(app, power_driver, Action::GlobalShutdownFailed);
        false
    }
}

/// Confirmed AWG stop: verified shutdown, then acknowledge the stopped
/// engine. Returns the USB reply bytes so the three remote stop paths stay
/// one line each.
pub(crate) fn stop_awg_confirmed<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
) -> &'static [u8]
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    if confirmed_global_shutdown(app, power_driver) {
        dispatch_app(app, power_driver, Action::AwgStopped);
        b"OK\r\n"
    } else {
        b"ERR:HARDWARE\r\n"
    }
}

/// A request the reducer rejected because the channel is mid-transition or
/// AWG is active failed for a transient reason, not a hardware fault.
fn transiently_busy(state: &AppState, channel: u8) -> bool {
    use benchvolt_pd::app::OutputTransition;
    !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
        || state
            .channels
            .get(usize::from(channel))
            .is_some_and(|output| output.transition != OutputTransition::Stable)
}

#[inline(always)]
pub(crate) fn set_current_limit<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
    channel: u8,
    milliamps: u16,
) -> &'static [u8]
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    if power_driver.is_busy() && app.state().channels[usize::from(channel)].physical_enabled {
        return b"ERR:BUSY\r\n";
    }
    dispatch_app(
        app,
        power_driver,
        Action::SetCurrentLimit { channel, milliamps },
    );
    let output = &app.state().channels[usize::from(channel)];
    if output.current_limit_ma == milliamps && output.fault != Fault::Hardware {
        b"OK\r\n"
    } else if transiently_busy(app.state(), channel) {
        b"ERR:BUSY\r\n"
    } else {
        b"ERR:HARDWARE\r\n"
    }
}

#[inline(always)]
pub(crate) fn set_voltage<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
    channel: u8,
    millivolts: u16,
) -> &'static [u8]
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    if power_driver.is_busy() && app.state().channels[usize::from(channel)].physical_enabled {
        return b"ERR:BUSY\r\n";
    }
    dispatch_app(
        app,
        power_driver,
        Action::SetVoltage {
            channel,
            millivolts,
        },
    );
    let output = &app.state().channels[usize::from(channel)];
    if output.setpoint_mv == millivolts && output.fault != Fault::Hardware {
        b"OK\r\n"
    } else if transiently_busy(app.state(), channel) {
        b"ERR:BUSY\r\n"
    } else {
        b"ERR:HARDWARE\r\n"
    }
}

#[inline(always)]
pub(crate) fn set_regulation_mode<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
    channel: u8,
    mode: RegulationMode,
) -> &'static [u8]
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    if power_driver.is_busy() && app.state().channels[usize::from(channel)].physical_enabled {
        return b"ERR:BUSY\r\n";
    }
    if channel == app.state().active_awg_channel()
        && !matches!(
            app.state().awg_status,
            AwgStatus::Stopped | AwgStatus::Fault
        )
    {
        return b"ERR:BUSY\r\n";
    }
    dispatch_app(
        app,
        power_driver,
        Action::SetRegulationMode { channel, mode },
    );
    let output = &app.state().channels[usize::from(channel)];
    if output.regulation_mode == mode && output.fault != Fault::Hardware {
        b"OK\r\n"
    } else if transiently_busy(app.state(), channel) {
        b"ERR:BUSY\r\n"
    } else {
        b"ERR:HARDWARE\r\n"
    }
}

/// Returns true when factory defaults were just applied, so the caller can
/// also restore the STUSB4500 NVM (which needs the PD bus this module does
/// not own).
pub(crate) fn service_profile_request<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
    settings_store: &mut SettingsStore,
    allow_compaction: bool,
) -> bool
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    let mut factory_defaults_applied = false;
    match app.state().profile_request {
        ProfileRequest::None => {}
        ProfileRequest::Save(slot) => {
            let outputs_physically_off = app.state().outputs_physically_off();
            let settings = PersistentSettings::from_state(app.state());
            let status = if persist_settings_record(
                settings_store,
                RecordKind::Profile(slot),
                settings,
                outputs_physically_off,
                allow_compaction,
            ) {
                ProfileStatus::Saved(slot)
            } else {
                ProfileStatus::Failed
            };
            dispatch_app(app, power_driver, Action::ProfileOperationFinished(status));
        }
        ProfileRequest::Load(slot) => {
            if let Some(record) = settings_store.profiles[usize::from(slot)] {
                if confirmed_global_shutdown(app, power_driver) {
                    dispatch_app(
                        app,
                        power_driver,
                        Action::ApplyProfile(record.settings, ProfileStatus::Loaded(slot)),
                    );
                } else {
                    dispatch_app(
                        app,
                        power_driver,
                        Action::ProfileOperationFinished(ProfileStatus::Failed),
                    );
                }
            } else {
                dispatch_app(
                    app,
                    power_driver,
                    Action::ProfileOperationFinished(ProfileStatus::Empty(slot)),
                );
            }
        }
        ProfileRequest::FactoryDefaults => {
            if confirmed_global_shutdown(app, power_driver) {
                let defaults = AppState::new(app.state().recovery_armed, None);
                dispatch_app(
                    app,
                    power_driver,
                    Action::ApplyProfile(
                        PersistentSettings::from_state(&defaults),
                        ProfileStatus::DefaultsLoaded,
                    ),
                );
                factory_defaults_applied = true;
            } else {
                dispatch_app(
                    app,
                    power_driver,
                    Action::ProfileOperationFinished(ProfileStatus::Failed),
                );
            }
        }
    }
    factory_defaults_applied
}
