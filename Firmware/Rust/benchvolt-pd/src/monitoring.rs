//! Stateful runtime protection policy, separated from hardware sampling.

use crate::{
    app::{Action, AppState, Fault, Measurement},
    power::{
        protection_output, tps55289_status_fault, ProtectionMonitor, Rail,
        SharedRailProtectionMonitor, SinkProtectionEvent, SinkProtectionMonitor,
        OVERTEMPERATURE_TRIP_SIXTEENTHS_C,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TpsStatusObservation {
    Inactive,
    Value(u8),
    ReadError,
}

#[derive(Default)]
pub struct ProtectionService {
    channels: [ProtectionMonitor; 5],
    shared_rails: SharedRailProtectionMonitor,
    sink: SinkProtectionMonitor,
    pending_ch5_status_fault: Option<Fault>,
    pending_shared_status_faults: [Option<Fault>; 2],
}

impl ProtectionService {
    pub fn channel_monitors(&self) -> &[ProtectionMonitor; 5] {
        &self.channels
    }

    pub const fn temperature_fault(sample: Option<i16>) -> Option<Fault> {
        match sample {
            Some(raw) if raw >= OVERTEMPERATURE_TRIP_SIXTEENTHS_C => Some(Fault::OverTemperature),
            None => Some(Fault::Sensor),
            _ => None,
        }
    }

    pub fn temperature_trip_actions(state: &AppState, fault: Fault) -> [Option<Action>; 5] {
        core::array::from_fn(|index| {
            let output = &state.channels[index];
            (output.requested_enabled || output.physical_enabled).then_some(
                Action::ProtectionTrip {
                    channel: index as u8,
                    fault,
                },
            )
        })
    }

    /// TPS STATUS is latched and read-to-clear. Require the same fault to
    /// reassert on the next active-rail poll before tripping either sibling.
    pub fn observe_shared_status(
        &mut self,
        state: &AppState,
        rail: Rail,
        observation: TpsStatusObservation,
    ) -> [Option<Action>; 2] {
        let (rail_index, channels) = rail_channels(rail);
        let active = channels.map(|channel| {
            let output = &state.channels[usize::from(channel)];
            output.requested_enabled || output.physical_enabled
        });
        if observation == TpsStatusObservation::Inactive || !active.into_iter().any(|value| value) {
            self.pending_shared_status_faults[rail_index] = None;
            return [None; 2];
        }
        let fault = match observation {
            TpsStatusObservation::Value(status) => tps55289_status_fault(status),
            TpsStatusObservation::ReadError => Some(Fault::Hardware),
            TpsStatusObservation::Inactive => None,
        };
        let Some(fault) = fault else {
            self.pending_shared_status_faults[rail_index] = None;
            return [None; 2];
        };
        if self.pending_shared_status_faults[rail_index] != Some(fault) {
            self.pending_shared_status_faults[rail_index] = Some(fault);
            return [None; 2];
        }
        self.pending_shared_status_faults[rail_index] = None;
        core::array::from_fn(|index| {
            active[index].then_some(Action::ProtectionTrip {
                channel: channels[index],
                fault,
            })
        })
    }

    /// CH5 bus failures fail closed immediately; latched STATUS fault bits use
    /// the same two-read confirmation as the shared converters.
    pub fn observe_ch5_status(
        &mut self,
        state: &AppState,
        observation: TpsStatusObservation,
    ) -> Option<Action> {
        let output = &state.channels[4];
        if observation == TpsStatusObservation::Inactive
            || !(output.requested_enabled || output.physical_enabled)
        {
            self.pending_ch5_status_fault = None;
            return None;
        }
        if observation == TpsStatusObservation::ReadError {
            self.pending_ch5_status_fault = None;
            return Some(Action::ProtectionTrip {
                channel: 4,
                fault: Fault::Hardware,
            });
        }
        let TpsStatusObservation::Value(status) = observation else {
            return None;
        };
        let Some(fault) = tps55289_status_fault(status) else {
            self.pending_ch5_status_fault = None;
            return None;
        };
        if self.pending_ch5_status_fault == Some(fault) {
            self.pending_ch5_status_fault = None;
            Some(Action::ProtectionTrip { channel: 4, fault })
        } else {
            self.pending_ch5_status_fault = Some(fault);
            None
        }
    }

    pub fn observe_shared_current(
        &mut self,
        state: &AppState,
        measurements: &[Measurement; 5],
        rail: Rail,
    ) -> [Option<Action>; 2] {
        let (_, channels) = rail_channels(rail);
        let Some(fault) = self.shared_rails.observe(state, measurements, rail) else {
            return [None; 2];
        };
        core::array::from_fn(|index| {
            let channel = channels[index];
            let output = &state.channels[usize::from(channel)];
            (output.requested_enabled || output.physical_enabled)
                .then_some(Action::ProtectionTrip { channel, fault })
        })
    }

    pub fn observe_sink(&mut self, state: &AppState, measurement: Measurement) -> Option<Action> {
        self.sink
            .observe(state, measurement)
            .map(|event| match event {
                SinkProtectionEvent::Trip(fault) => Action::SinkProtectionTrip(fault),
                SinkProtectionEvent::Recovered => Action::SinkProtectionRecovered,
            })
    }

    pub fn observe_channel(
        &mut self,
        state: &AppState,
        channel: u8,
        measurement: Measurement,
    ) -> Option<Action> {
        let voltage_tracking = !(state.awg_status == crate::app::AwgStatus::Running
            && channel == state.active_awg_channel());
        self.channels[usize::from(channel)]
            .observe_with_voltage_tracking(
                &protection_output(state, channel),
                measurement,
                voltage_tracking,
            )
            .map(|fault| Action::ProtectionTrip { channel, fault })
    }
}

const fn rail_channels(rail: Rail) -> (usize, [u8; 2]) {
    match rail {
        Rail::Dc1 => (0, [0, 1]),
        Rail::Dc2 => (1, [2, 3]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_state(channel: usize) -> AppState {
        let mut state = AppState::new(true, Some(25 * 16));
        state.channels[channel].requested_enabled = true;
        state.channels[channel].physical_enabled = true;
        state
    }

    #[test]
    fn shared_status_requires_same_fault_to_reassert() {
        let state = active_state(0);
        let mut service = ProtectionService::default();

        assert!(service
            .observe_shared_status(&state, Rail::Dc1, TpsStatusObservation::Value(0x40))
            .iter()
            .all(Option::is_none));
        let actions =
            service.observe_shared_status(&state, Rail::Dc1, TpsStatusObservation::Value(0x40));
        assert!(matches!(
            actions[0],
            Some(Action::ProtectionTrip {
                channel: 0,
                fault: Fault::OverCurrent
            })
        ));
        assert!(actions[1].is_none());
    }

    #[test]
    fn status_clear_breaks_confirmation_and_ch5_read_failure_is_immediate() {
        let shared = active_state(0);
        let mut service = ProtectionService::default();
        let _ =
            service.observe_shared_status(&shared, Rail::Dc1, TpsStatusObservation::Value(0x40));
        let _ =
            service.observe_shared_status(&shared, Rail::Dc1, TpsStatusObservation::Value(0x00));
        assert!(service
            .observe_shared_status(&shared, Rail::Dc1, TpsStatusObservation::Value(0x40))
            .iter()
            .all(Option::is_none));

        let ch5 = active_state(4);
        assert!(matches!(
            service.observe_ch5_status(&ch5, TpsStatusObservation::ReadError),
            Some(Action::ProtectionTrip {
                channel: 4,
                fault: Fault::Hardware
            })
        ));
    }

    #[test]
    fn temperature_failures_trip_only_active_outputs() {
        let mut state = active_state(1);
        state.channels[4].requested_enabled = true;
        assert_eq!(
            ProtectionService::temperature_fault(Some(75 * 16)),
            Some(Fault::OverTemperature)
        );
        assert_eq!(
            ProtectionService::temperature_fault(None),
            Some(Fault::Sensor)
        );
        assert_eq!(ProtectionService::temperature_fault(Some(74 * 16)), None);

        let actions = ProtectionService::temperature_trip_actions(&state, Fault::Sensor);
        assert!(actions[0].is_none());
        assert!(matches!(
            actions[1],
            Some(Action::ProtectionTrip {
                channel: 1,
                fault: Fault::Sensor
            })
        ));
        assert!(matches!(
            actions[4],
            Some(Action::ProtectionTrip {
                channel: 4,
                fault: Fault::Sensor
            })
        ));
    }
}
