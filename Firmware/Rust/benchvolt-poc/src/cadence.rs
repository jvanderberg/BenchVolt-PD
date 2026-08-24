#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Due {
    pub temperature: bool,
    pub measurement: bool,
    pub display_measurement: bool,
    pub awg_load: bool,
}

pub const PROTECTION_STALE_MS: u16 = 100;

#[derive(Default)]
pub struct ServiceCadence {
    temperature_ms: u16,
    measurement_ms: u16,
    display_measurement_ms: u16,
    awg_load_ms: u16,
    measurement_age_ms: u16,
    protection_late_ms: u16,
    health_ms: u32,
}

impl ServiceCadence {
    #[inline(always)]
    pub fn advance(&mut self, elapsed_ms: u16) -> Due {
        self.health_ms = self.health_ms.saturating_add(u32::from(elapsed_ms));
        self.measurement_age_ms = self.measurement_age_ms.saturating_add(elapsed_ms);
        let due = Due {
            temperature: tick(&mut self.temperature_ms, elapsed_ms, 100),
            measurement: tick(&mut self.measurement_ms, elapsed_ms, 20),
            display_measurement: tick(&mut self.display_measurement_ms, elapsed_ms, 200),
            awg_load: tick(&mut self.awg_load_ms, elapsed_ms, 1_000),
        };
        if due.measurement {
            if self.measurement_age_ms > 20 {
                self.protection_late_ms = self
                    .protection_late_ms
                    .saturating_add(self.measurement_age_ms);
            } else {
                self.protection_late_ms = 0;
            }
            self.measurement_age_ms = 0;
        }
        due
    }

    #[inline(always)]
    pub fn invalidate_awg_window(&mut self, due: &mut Due) {
        self.awg_load_ms = 0;
        due.awg_load = false;
    }

    #[inline(always)]
    pub const fn healthy_for(&self, milliseconds: u32) -> bool {
        self.health_ms >= milliseconds
    }

    pub fn protection_stale(&mut self, outputs_active: bool) -> bool {
        if !outputs_active {
            self.measurement_age_ms = 0;
            self.protection_late_ms = 0;
            return false;
        }
        self.protection_late_ms >= PROTECTION_STALE_MS
    }
}

#[inline(always)]
fn tick(elapsed: &mut u16, delta: u16, period: u16) -> bool {
    let total = u32::from(*elapsed) + u32::from(delta);
    *elapsed = (total % u32::from(period)) as u16;
    total >= u32::from(period)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_periods_match_the_foreground_loop_contract() {
        let mut cadence = ServiceCadence::default();
        assert_eq!(cadence.advance(19), Due::default());
        assert_eq!(
            cadence.advance(1),
            Due {
                measurement: true,
                ..Due::default()
            }
        );
        assert_eq!(cadence.advance(19), Due::default());
        assert!(cadence.advance(1).measurement);

        let mut cadence = ServiceCadence::default();
        assert_eq!(
            cadence.advance(1_000),
            Due {
                temperature: true,
                measurement: true,
                display_measurement: true,
                awg_load: true,
            }
        );
        assert!(cadence.healthy_for(1_000));
        assert!(!cadence.healthy_for(1_001));
    }

    #[test]
    fn late_service_preserves_phase_without_bursting_catch_up_work() {
        let mut cadence = ServiceCadence::default();
        let late = cadence.advance(99);
        assert!(late.measurement);
        assert!(!late.temperature);

        // One late pass emits only one measurement, but retains the 19 ms
        // remainder so the next millisecond reaches the original 20 ms phase.
        let next = cadence.advance(1);
        assert!(next.measurement);
        assert!(next.temperature);
        assert!(!cadence.advance(19).measurement);
        assert!(cadence.advance(1).measurement);
    }

    #[test]
    fn repeated_late_measurements_fail_closed_but_one_late_pass_can_recover() {
        let mut cadence = ServiceCadence::default();
        assert!(cadence.advance(99).measurement);
        assert!(!cadence.protection_stale(true));
        assert!(cadence.advance(1).measurement);
        assert!(!cadence.protection_stale(true));

        assert!(cadence.advance(99).measurement);
        assert!(!cadence.protection_stale(true));
        assert!(cadence.advance(99).measurement);
        assert!(cadence.protection_stale(true));

        // Timing history accumulated while powered down cannot fault a later
        // output request.
        assert!(!cadence.protection_stale(false));
        assert!(!cadence.protection_stale(true));
    }

    #[test]
    fn invalid_awg_window_restarts_only_the_load_period() {
        let mut cadence = ServiceCadence::default();
        let mut due = cadence.advance(1_000);
        cadence.invalidate_awg_window(&mut due);

        assert!(due.temperature);
        assert!(due.measurement);
        assert!(due.display_measurement);
        assert!(!due.awg_load);
        assert!(!cadence.advance(999).awg_load);
        assert!(cadence.advance(1).awg_load);
    }

    #[test]
    fn health_duration_saturates_instead_of_wrapping() {
        let mut cadence = ServiceCadence {
            health_ms: u32::MAX - 1,
            ..ServiceCadence::default()
        };
        cadence.advance(10);
        assert!(cadence.healthy_for(u32::MAX));
    }
}
