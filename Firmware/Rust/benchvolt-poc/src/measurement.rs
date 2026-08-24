//! Measurement aggregation windows independent of ADC hardware.

use crate::{
    app::{AppState, AwgStatus, LoadMeasurement, Measurement},
    load::LoadAccumulator,
};

#[derive(Clone, Copy)]
struct MeasurementAccumulator {
    millivolts: u64,
    milliamps: u64,
    samples: u32,
    valid: bool,
}

impl MeasurementAccumulator {
    const fn new() -> Self {
        Self {
            millivolts: 0,
            milliamps: 0,
            samples: 0,
            valid: true,
        }
    }

    fn push(&mut self, measurement: Measurement) {
        self.valid &= measurement.valid;
        self.millivolts = self
            .millivolts
            .saturating_add(u64::from(measurement.millivolts));
        self.milliamps = self
            .milliamps
            .saturating_add(u64::from(measurement.milliamps));
        self.samples = self.samples.saturating_add(1);
    }

    fn take(&mut self) -> Measurement {
        let result = if self.valid && self.samples > 0 {
            Measurement {
                millivolts: (self.millivolts / u64::from(self.samples)).min(u64::from(u16::MAX))
                    as u16,
                milliamps: (self.milliamps / u64::from(self.samples)).min(u64::from(u16::MAX))
                    as u16,
                valid: true,
            }
        } else {
            Measurement {
                millivolts: 0,
                milliamps: 0,
                valid: false,
            }
        };
        *self = Self::new();
        result
    }
}

pub struct MeasurementWindows {
    channels: [MeasurementAccumulator; 5],
    sink: MeasurementAccumulator,
    awg_load: LoadAccumulator,
}

impl MeasurementWindows {
    pub const fn new() -> Self {
        Self {
            channels: [MeasurementAccumulator::new(); 5],
            sink: MeasurementAccumulator::new(),
            awg_load: LoadAccumulator::new(),
        }
    }

    /// Record one protection-cycle frame. Returns whether an AWG load window
    /// remains active; callers use false to reset its publication cadence.
    pub fn record(
        &mut self,
        state: &AppState,
        channels: [Measurement; 5],
        sink: Measurement,
    ) -> bool {
        for (accumulator, measurement) in self.channels.iter_mut().zip(channels) {
            accumulator.push(measurement);
        }
        self.sink.push(sink);
        if state.awg_status == AwgStatus::Running {
            self.awg_load
                .push(channels[usize::from(state.active_awg_channel())]);
            true
        } else {
            self.awg_load.reset();
            false
        }
    }

    pub fn take_display(&mut self) -> ([Measurement; 5], Measurement) {
        (
            core::array::from_fn(|index| self.channels[index].take()),
            self.sink.take(),
        )
    }

    pub fn take_awg_load(&mut self) -> LoadMeasurement {
        self.awg_load.take()
    }
}

impl Default for MeasurementWindows {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn measurement(millivolts: u16, milliamps: u16, valid: bool) -> Measurement {
        Measurement {
            millivolts,
            milliamps,
            valid,
        }
    }

    #[test]
    fn display_windows_average_and_reset_and_invalid_samples_poison_one_window() {
        let state = AppState::new(true, None);
        let mut windows = MeasurementWindows::new();
        let first = core::array::from_fn(|index| measurement(1_000 + index as u16, 100, true));
        let mut second = core::array::from_fn(|index| measurement(3_000 + index as u16, 300, true));
        second[2].valid = false;
        assert!(!windows.record(&state, first, measurement(5_000, 500, true)));
        assert!(!windows.record(&state, second, measurement(7_000, 700, true)));

        let (channels, sink) = windows.take_display();
        assert!(channels[0] == measurement(2_000, 200, true));
        assert!(!channels[2].valid);
        assert!(sink == measurement(6_000, 600, true));
        assert!(!windows.take_display().0[0].valid);
    }

    #[test]
    fn awg_window_tracks_the_selected_channel_and_stopped_state_clears_it() {
        let mut state = AppState::new(true, None);
        state.awg_status = AwgStatus::Running;
        state.awg.channel = 4;
        let mut windows = MeasurementWindows::new();
        let channels = core::array::from_fn(|index| measurement(1_000, index as u16 * 100, true));
        assert!(windows.record(&state, channels, measurement(0, 0, true)));
        assert_eq!(windows.take_awg_load().milliamps_rms, 400);

        assert!(windows.record(&state, channels, measurement(0, 0, true)));
        state.awg_status = AwgStatus::Stopped;
        assert!(!windows.record(&state, channels, measurement(0, 0, true)));
        assert!(!windows.take_awg_load().valid);
    }

    #[test]
    fn long_windows_do_not_saturate_the_sample_count_or_overflow_sums() {
        let state = AppState::new(true, None);
        let mut windows = MeasurementWindows::new();
        let maximum = measurement(u16::MAX, u16::MAX, true);
        for _ in 0..1_000 {
            windows.record(&state, [maximum; 5], maximum);
        }
        let (channels, sink) = windows.take_display();
        assert!(channels[4] == maximum);
        assert!(sink == maximum);
    }
}
