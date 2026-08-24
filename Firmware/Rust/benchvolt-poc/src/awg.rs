use crate::app::{AwgConfig, AwgWaveform};

pub const TICKS_PER_SECOND: u32 = 2_000;

pub struct Scheduler {
    phase: u32,
    phase_remainder: u64,
    last_phase_tick: u16,
    next_update_tick: u16,
    active: bool,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            phase: 0,
            phase_remainder: 0,
            last_phase_tick: 0,
            next_update_tick: 0,
            active: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn stop(&mut self) {
        self.active = false;
        self.phase = 0;
        self.phase_remainder = 0;
    }

    pub fn tick(&mut self, now: u16, config: AwgConfig) -> Option<u16> {
        // Built-in waveforms are phase-generated locally, so they are not bound
        // by the stock uploaded-ARB format's 4 ms dwell. A dedicated 2 kHz
        // timer gives a 120 Hz shaped waveform roughly 17 setpoints per cycle.
        // Late service still skips stale samples and remains phase-correct.
        let interval = 1u16;
        if !self.active {
            self.active = true;
            self.phase = if config.waveform == AwgWaveform::Sine {
                0xc000_0000
            } else {
                0
            };
            self.phase_remainder = 0;
            self.last_phase_tick = now;
            self.next_update_tick = now.wrapping_add(interval);
            return Some(config.low_mv);
        }
        if (now.wrapping_sub(self.next_update_tick) as i16) < 0 {
            return None;
        }

        // Phase follows actual monotonic elapsed time. Deadlines advance from
        // their prior absolute origin, so late service emits one current sample
        // rather than a burst of stale catch-up writes.
        let elapsed = now.wrapping_sub(self.last_phase_tick);
        self.last_phase_tick = now;
        let late = now.wrapping_sub(self.next_update_tick);
        let deadlines = late / interval + 1;
        self.next_update_tick = self
            .next_update_tick
            .wrapping_add(deadlines.wrapping_mul(interval));

        // Accumulate the division remainder instead of truncating a tuning word
        // once per millisecond. This makes exact cycle landmarks exact (for
        // example, 1 Hz wraps at 1000 ms rather than one service tick later).
        let phase_denominator = u64::from(TICKS_PER_SECOND) * 1_000;
        let cycle_numerator = u64::from(config.frequency_millihz) * u64::from(elapsed);
        let fractional_cycle_numerator = cycle_numerator % phase_denominator;
        let scaled = (fractional_cycle_numerator << 32) + self.phase_remainder;
        self.phase = self.phase.wrapping_add((scaled / phase_denominator) as u32);
        self.phase_remainder = scaled % phase_denominator;
        let x = (self.phase >> 16) as u16;
        let normalized = match config.waveform {
            AwgWaveform::Square => {
                // Begin each cycle LOW, then remain HIGH for the configured
                // percentage. This preserves the existing 50% phase convention.
                let high_threshold = (u64::from(100 - config.duty_percent) << 32) / 100;
                if u64::from(self.phase) >= high_threshold {
                    u16::MAX
                } else {
                    0
                }
            }
            AwgWaveform::Ramp => x,
            AwgWaveform::Triangle => {
                if x < 0x8000 {
                    x.saturating_mul(2)
                } else {
                    (u16::MAX - x).saturating_mul(2)
                }
            }
            AwgWaveform::Sine => {
                const SINE: [u16; 32] = [
                    32768, 39160, 45307, 50972, 55938, 59980, 62910, 64685, 65535, 64685, 62910,
                    59980, 55938, 50972, 45307, 39160, 32768, 26375, 20228, 14563, 9597, 5555,
                    2625, 850, 0, 850, 2625, 5555, 9597, 14563, 20228, 26375,
                ];
                let index = (self.phase >> 27) as usize;
                let fraction = ((self.phase >> 11) & 0xffff) as i32;
                let start = i32::from(SINE[index]);
                let end = i32::from(SINE[(index + 1) & 31]);
                (start + (((end - start) * fraction) >> 16)).clamp(0, 65_535) as u16
            }
        };
        Some(
            config.low_mv
                + ((u32::from(config.high_mv - config.low_mv) * u32::from(normalized))
                    / u32::from(u16::MAX)) as u16,
        )
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_service_emits_one_phase_correct_sample() {
        let config = AwgConfig {
            channel: 3,
            waveform: AwgWaveform::Square,
            frequency_millihz: 1_000,
            duty_percent: 50,
            low_mv: 1_000,
            high_mv: 5_000,
        };
        let mut scheduler = Scheduler::new();
        assert_eq!(scheduler.tick(0, config), Some(1_000));
        assert_eq!(scheduler.tick(0, config), None);
        assert_eq!(scheduler.tick(1_040, config), Some(5_000));
        assert_eq!(scheduler.tick(1_040, config), None);
    }

    #[test]
    fn every_builtin_waveform_stays_inside_voltage_bounds_across_wrap() {
        for waveform in [
            AwgWaveform::Square,
            AwgWaveform::Triangle,
            AwgWaveform::Ramp,
            AwgWaveform::Sine,
        ] {
            let config = AwgConfig {
                waveform,
                ..AwgConfig::default()
            };
            let mut scheduler = Scheduler::new();
            for tick in (0..=65_000u16).step_by(20) {
                if let Some(sample) = scheduler.tick(tick, config) {
                    assert!((config.low_mv..=config.high_mv).contains(&sample));
                }
            }
        }
    }

    #[test]
    fn stop_resets_to_a_safe_low_first_sample() {
        let config = AwgConfig::default();
        let mut scheduler = Scheduler::new();
        scheduler.tick(0, config);
        scheduler.tick(800, config);
        scheduler.stop();
        assert_eq!(scheduler.tick(900, config), Some(config.low_mv));
    }

    #[test]
    fn one_hertz_sine_has_hundreds_of_interpolated_steps() {
        let config = AwgConfig {
            waveform: AwgWaveform::Sine,
            ..AwgConfig::default()
        };
        let mut scheduler = Scheduler::new();
        let mut previous = None;
        let mut changes = 0;
        for tick in 0..2_000u16 {
            if let Some(sample) = scheduler.tick(tick, config) {
                if previous.is_some() && previous != Some(sample) {
                    changes += 1;
                }
                previous = Some(sample);
            }
        }
        assert!(changes > 450);
    }

    #[test]
    fn thirty_hertz_sine_has_at_least_thirty_distinct_samples_per_cycle() {
        let config = AwgConfig {
            waveform: AwgWaveform::Sine,
            frequency_millihz: 30_000,
            ..AwgConfig::default()
        };
        let mut scheduler = Scheduler::new();
        let mut previous = None;
        let mut changes = 0;
        for tick in 0..=67u16 {
            if let Some(sample) = scheduler.tick(tick, config) {
                if previous.is_some() && previous != Some(sample) {
                    changes += 1;
                }
                previous = Some(sample);
            }
        }
        assert!(changes >= 60);
    }

    #[test]
    fn one_hundred_twenty_hz_sine_has_at_least_sixteen_samples_per_cycle() {
        let config = AwgConfig {
            waveform: AwgWaveform::Sine,
            frequency_millihz: 120_000,
            ..AwgConfig::default()
        };
        let mut scheduler = Scheduler::new();
        let samples = (0..=17u16)
            .filter_map(|tick| scheduler.tick(tick, config))
            .count();
        assert!(samples >= 17);
    }

    fn landmarks(waveform: AwgWaveform) -> [u16; 5] {
        let config = AwgConfig {
            waveform,
            frequency_millihz: 1_000,
            low_mv: 1_000,
            high_mv: 5_000,
            ..AwgConfig::default()
        };
        let mut scheduler = Scheduler::new();
        [0, 500, 1_000, 1_500, 2_000].map(|tick| scheduler.tick(tick, config).unwrap())
    }

    #[test]
    fn builtins_match_exact_quarter_cycle_landmarks() {
        assert_eq!(
            landmarks(AwgWaveform::Square),
            [1_000, 1_000, 5_000, 5_000, 1_000]
        );
        assert_eq!(
            landmarks(AwgWaveform::Ramp),
            [1_000, 2_000, 3_000, 4_000, 1_000]
        );
        for waveform in [AwgWaveform::Triangle, AwgWaveform::Sine] {
            for (actual, expected) in landmarks(waveform)
                .into_iter()
                .zip([1_000u16, 3_000, 5_000, 3_000, 1_000])
            {
                assert!(actual.abs_diff(expected) <= 1);
            }
        }
    }

    #[test]
    fn square_duty_controls_high_time_and_other_shapes_ignore_it() {
        let config = AwgConfig {
            duty_percent: 25,
            ..AwgConfig::default()
        };
        let mut square = Scheduler::new();
        assert_eq!(square.tick(0, config), Some(config.low_mv));
        assert_eq!(square.tick(1_499, config), Some(config.low_mv));
        assert_eq!(square.tick(1_500, config), Some(config.high_mv));
        assert_eq!(square.tick(1_999, config), Some(config.high_mv));
        assert_eq!(square.tick(2_000, config), Some(config.low_mv));

        for waveform in [AwgWaveform::Triangle, AwgWaveform::Ramp, AwgWaveform::Sine] {
            let low_duty = AwgConfig {
                waveform,
                duty_percent: 1,
                ..AwgConfig::default()
            };
            let high_duty = AwgConfig {
                duty_percent: 99,
                ..low_duty
            };
            let mut first = Scheduler::new();
            let mut second = Scheduler::new();
            for tick in (0..=2_000u16).step_by(17) {
                assert_eq!(first.tick(tick, low_duty), second.tick(tick, high_duty));
            }
        }
    }
}
