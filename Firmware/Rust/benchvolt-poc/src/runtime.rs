use benchvolt_poc::{
    app::{Action, AppReducer, AppState, ProfileRequest, ProfileStatus},
    power::{execute_global_shutdown, FirmwareEffectPlanner, PowerDriver, PowerExecutor},
    settings::{PersistentSettings, RecordKind},
};
use reducto::EffectApp;

use crate::boot::{persist_settings_record, SettingsStore};

pub(crate) fn dispatch_app<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
    action: Action,
) -> bool
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    let mut pending_action = Some(action);
    let mut changed = false;
    while let Some(action) = pending_action.take() {
        let outcome = app.dispatch(action);
        changed |= outcome.changed();
        pending_action = match outcome.effect() {
            Some(effect) if effect.global_shutdown => {
                Some(if execute_global_shutdown(power_driver).is_ok() {
                    Action::GlobalShutdownApplied
                } else {
                    Action::GlobalShutdownFailed
                })
            }
            Some(effect) => effect
                .power
                .and_then(|power| power_driver.submit(app.state(), power)),
            None => None,
        };
    }
    changed
}

pub(crate) fn service_profile_request<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut PowerExecutor<D>,
    settings_store: &mut SettingsStore,
) where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    match app.state().profile_request {
        ProfileRequest::None => {}
        ProfileRequest::Save(slot) => {
            let outputs_physically_off = app
                .state()
                .channels
                .iter()
                .all(|channel| !channel.physical_enabled);
            let settings = PersistentSettings::from_state(app.state());
            let status = if persist_settings_record(
                settings_store,
                RecordKind::Profile(slot),
                settings,
                outputs_physically_off,
            ) {
                ProfileStatus::Saved(slot)
            } else {
                ProfileStatus::Failed
            };
            dispatch_app(app, power_driver, Action::ProfileOperationFinished(status));
        }
        ProfileRequest::Load(slot) => {
            if let Some(record) = settings_store.profiles[usize::from(slot)] {
                if execute_global_shutdown(power_driver).is_ok() {
                    dispatch_app(
                        app,
                        power_driver,
                        Action::ApplyProfile(record.settings, ProfileStatus::Loaded(slot)),
                    );
                } else {
                    dispatch_app(app, power_driver, Action::GlobalShutdownFailed);
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
            if execute_global_shutdown(power_driver).is_ok() {
                let defaults = AppState::new(app.state().recovery_armed, None);
                dispatch_app(
                    app,
                    power_driver,
                    Action::ApplyProfile(
                        PersistentSettings::from_state(&defaults),
                        ProfileStatus::DefaultsLoaded,
                    ),
                );
            } else {
                dispatch_app(app, power_driver, Action::GlobalShutdownFailed);
                dispatch_app(
                    app,
                    power_driver,
                    Action::ProfileOperationFinished(ProfileStatus::Failed),
                );
            }
        }
    }
}
