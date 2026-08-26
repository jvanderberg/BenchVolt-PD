use crate::app::{LoadMeasurement, Measurement};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadAccumulator {
    current_squared_sum: u64,
    power_microwatts_sum: u64,
    samples: u16,
    valid: bool,
}

impl LoadAccumulator {
    pub const fn new() -> Self {
        Self {
            current_squared_sum: 0,
            power_microwatts_sum: 0,
            samples: 0,
            valid: true,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn push(&mut self, measurement: Measurement) {
        self.valid &= measurement.valid;
        let milliamps = u64::from(measurement.milliamps);
        self.current_squared_sum = self
            .current_squared_sum
            .saturating_add(milliamps.saturating_mul(milliamps));
        self.power_microwatts_sum = self
            .power_microwatts_sum
            .saturating_add(u64::from(measurement.millivolts).saturating_mul(milliamps));
        self.samples = self.samples.saturating_add(1);
    }

    pub fn take(&mut self) -> LoadMeasurement {
        let result = if self.valid && self.samples != 0 {
            let samples = u64::from(self.samples);
            LoadMeasurement {
                milliamps_rms: integer_sqrt(
                    crate::math::div_rem_u64(self.current_squared_sum, samples).0,
                )
                .min(u64::from(u16::MAX)) as u16,
                milliwatts_average: crate::math::div_rem_u64(
                    self.power_microwatts_sum,
                    samples * 1_000,
                )
                .0
                .min(u64::from(u32::MAX)) as u32,
                valid: true,
            }
        } else {
            LoadMeasurement::INVALID
        };
        self.reset();
        result
    }
}

impl Default for LoadAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

fn integer_sqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    let mut estimate = 1u64 << (64 - u64::from(value.leading_zeros())).div_ceil(2);
    loop {
        let next = (estimate + crate::math::div_rem_u64(value, estimate).0) >> 1;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_rms_current_and_average_real_power_from_pairs() {
        let mut accumulator = LoadAccumulator::new();
        accumulator.push(Measurement {
            millivolts: 1_000,
            milliamps: 100,
            valid: true,
        });
        accumulator.push(Measurement {
            millivolts: 3_000,
            milliamps: 300,
            valid: true,
        });

        assert_eq!(
            accumulator.take(),
            LoadMeasurement {
                milliamps_rms: 223,
                milliwatts_average: 500,
                valid: true,
            }
        );
    }

    #[test]
    fn one_invalid_sample_invalidates_the_window_and_take_resets_it() {
        let mut accumulator = LoadAccumulator::new();
        accumulator.push(Measurement {
            millivolts: 1_000,
            milliamps: 100,
            valid: false,
        });
        assert_eq!(accumulator.take(), LoadMeasurement::INVALID);

        accumulator.push(Measurement {
            millivolts: 2_000,
            milliamps: 250,
            valid: true,
        });
        assert_eq!(
            accumulator.take(),
            LoadMeasurement {
                milliamps_rms: 250,
                milliwatts_average: 500,
                valid: true,
            }
        );
    }
}
