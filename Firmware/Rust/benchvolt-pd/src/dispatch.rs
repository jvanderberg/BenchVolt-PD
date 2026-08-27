//! Action dispatch glue: runs the reducer, derives effects, and feeds
//! completion/failure actions back in until the state settles. Pure over the
//! `PowerDriver` trait so it is host-testable.

use crate::app::{Action, AppReducer, AppState};
use crate::power::{execute_global_shutdown, FirmwareEffectPlanner, PowerDriver, PowerExecutor};
use reducto::EffectApp;

pub fn dispatch_app<V, D, const Q: usize>(
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
                    // Invariant #5 escalation: a failed verified shutdown
                    // falls back to the raw register-level all-off, exactly
                    // like every synchronous shutdown site. Target-only —
                    // the host test harness has no hardware to slam and
                    // models this case through the mock driver instead.
                    #[cfg(target_arch = "arm")]
                    unsafe {
                        crate::early_shutdown::raw_emergency_shutdown()
                    };
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
