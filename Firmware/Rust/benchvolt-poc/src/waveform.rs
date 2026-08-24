use crate::{
    app::{AwgConfig, AwgSource, AwgStatus},
    arb::{
        Buffer, RuntimeDirective, RuntimeState, Scheduler as ArbScheduler, SchedulerStatus, Start,
        Tick,
    },
    awg::Scheduler as BuiltinScheduler,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Directive {
    None,
    Sample(u16),
    PrepareStart,
    Stop,
    Finished,
    FailSafeShutdown,
    FaultShutdown,
}

pub struct Service {
    builtin: BuiltinScheduler,
    arbitrary: ArbScheduler,
    runtime: RuntimeState,
}

impl Service {
    pub const fn new() -> Self {
        Self {
            builtin: BuiltinScheduler::new(),
            arbitrary: ArbScheduler::new(),
            runtime: RuntimeState::new(),
        }
    }

    pub fn arm_arbitrary(&mut self, start: Start) {
        self.arbitrary.stop();
        self.runtime.arm(start);
    }

    pub fn cancel_arbitrary(&mut self, channel: u8) -> bool {
        let cancelled = self.runtime.cancel(channel);
        if cancelled {
            self.arbitrary.stop();
        }
        cancelled
    }

    pub fn stop_arbitrary(&mut self) {
        self.arbitrary.stop();
    }

    pub fn pending_arb_ack(&self) -> Option<Start> {
        self.runtime.pending_ack()
    }

    pub fn take_pending_arb_ack(&mut self) -> Option<Start> {
        self.runtime.take_pending_ack()
    }

    pub fn arb_status(&self) -> SchedulerStatus {
        self.arbitrary.status()
    }

    pub fn tick(
        &mut self,
        status: AwgStatus,
        source: AwgSource,
        config: AwgConfig,
        now: u16,
        buffer: Option<&Buffer>,
    ) -> Directive {
        match status {
            AwgStatus::StartRequested => {
                self.builtin.stop();
                self.arbitrary.stop();
                Directive::PrepareStart
            }
            AwgStatus::StopRequested => {
                self.builtin.stop();
                self.arbitrary.stop();
                if source == AwgSource::Arbitrary {
                    self.runtime.clear();
                }
                Directive::Stop
            }
            AwgStatus::Running => match source {
                AwgSource::Builtin => {
                    self.arbitrary.stop();
                    self.builtin
                        .tick(now, config)
                        .map_or(Directive::None, Directive::Sample)
                }
                AwgSource::Arbitrary => {
                    self.builtin.stop();
                    match self.runtime.directive() {
                        RuntimeDirective::Run(start) => {
                            let Some(buffer) = buffer else {
                                self.arbitrary.stop();
                                return Directive::FailSafeShutdown;
                            };
                            match self.arbitrary.tick(now, start, buffer) {
                                Some(Tick::Sample(millivolts)) => Directive::Sample(millivolts),
                                Some(Tick::Finished) => {
                                    self.runtime.finish();
                                    Directive::Finished
                                }
                                None => Directive::None,
                            }
                        }
                        RuntimeDirective::Shutdown => {
                            self.arbitrary.stop();
                            Directive::FailSafeShutdown
                        }
                    }
                }
            },
            AwgStatus::Fault => {
                let active = self.builtin.is_active() || self.arbitrary.is_active();
                self.builtin.stop();
                self.arbitrary.stop();
                if active {
                    Directive::FaultShutdown
                } else {
                    Directive::None
                }
            }
            AwgStatus::Stopped => {
                self.builtin.stop();
                self.arbitrary.stop();
                Directive::None
            }
            AwgStatus::Starting => Directive::None,
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{AwgConfig, AwgSource, AwgStatus},
        arb::{Buffer, Start},
    };

    use super::*;

    fn start() -> Start {
        Start {
            channel: 3,
            count: 1,
            multiplier_ticks: 1,
            repetitions: 1,
        }
    }

    #[test]
    fn arbitrary_running_without_session_metadata_demands_physical_shutdown() {
        let mut service = Service::new();
        assert_eq!(
            service.tick(
                AwgStatus::Running,
                AwgSource::Arbitrary,
                AwgConfig::default(),
                0,
                Some(&Buffer::new()),
            ),
            Directive::FailSafeShutdown
        );
    }

    #[test]
    fn explicit_stop_clears_the_deferred_start_ack() {
        let mut service = Service::new();
        service.arm_arbitrary(start());
        assert_eq!(
            service.tick(
                AwgStatus::StopRequested,
                AwgSource::Arbitrary,
                AwgConfig::default(),
                0,
                None,
            ),
            Directive::Stop
        );
        assert_eq!(service.take_pending_arb_ack(), None);
    }

    #[test]
    fn start_preparation_and_builtin_samples_are_single_directives() {
        let mut service = Service::new();
        assert_eq!(
            service.tick(
                AwgStatus::StartRequested,
                AwgSource::Builtin,
                AwgConfig::default(),
                0,
                None,
            ),
            Directive::PrepareStart
        );
        assert_eq!(
            service.tick(
                AwgStatus::Running,
                AwgSource::Builtin,
                AwgConfig::default(),
                0,
                None,
            ),
            Directive::Sample(AwgConfig::default().low_mv)
        );
    }

    #[test]
    fn active_arbitrary_session_without_buffer_access_fails_safe() {
        let mut service = Service::new();
        service.arm_arbitrary(start());
        assert_eq!(
            service.tick(
                AwgStatus::Running,
                AwgSource::Arbitrary,
                AwgConfig::default(),
                0,
                None,
            ),
            Directive::FailSafeShutdown
        );
    }
}
