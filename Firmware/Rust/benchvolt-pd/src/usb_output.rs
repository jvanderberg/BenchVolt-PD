use crate::app::{Action, ChannelSnapshot, Fault, OutputTransition};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Proceed,
    ProceedAfterCancellation,
    Busy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestResult {
    Pending,
    Complete(Result<(), Fault>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pending {
    channel: u8,
    operation: u16,
}

/// Owns the lifetime of one deferred USB output command.
///
/// Hardware execution remains outside this type. It only decides whether a
/// command may enter the reducer and pairs the resulting operation token with
/// its eventual terminal response.
pub struct OutputTransaction {
    pending: Option<Pending>,
}

impl Default for OutputTransaction {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputTransaction {
    pub const fn new() -> Self {
        Self { pending: None }
    }

    pub fn begin_request(
        &mut self,
        channel: u8,
        enabled: bool,
        power_busy: bool,
        pd_pending: bool,
    ) -> Admission {
        if enabled && (power_busy || pd_pending) {
            return Admission::Busy;
        }
        match self.pending {
            Some(pending) if pending.channel == channel && !enabled => {
                self.pending = None;
                Admission::ProceedAfterCancellation
            }
            Some(_) => Admission::Busy,
            None => Admission::Proceed,
        }
    }

    pub fn record_request(
        &mut self,
        channel: u8,
        enabled: bool,
        output: &ChannelSnapshot,
    ) -> RequestResult {
        let expected_transition = if enabled {
            OutputTransition::Enabling(output.operation)
        } else {
            OutputTransition::Disabling(output.operation)
        };
        if output.transition == expected_transition && output.requested_enabled == enabled {
            self.pending = Some(Pending {
                channel,
                operation: output.operation,
            });
            RequestResult::Pending
        } else if output.physical_enabled == enabled
            && output.requested_enabled == enabled
            && (!enabled || output.fault == Fault::None)
        {
            RequestResult::Complete(Ok(()))
        } else {
            RequestResult::Complete(Err(output.fault))
        }
    }

    pub fn observe_completion(&mut self, action: &Action) -> Option<Result<(), Fault>> {
        let (channel, operation, result) = match *action {
            Action::OutputApplied {
                channel, operation, ..
            } => (channel, operation, Ok(())),
            Action::OutputFailed {
                channel,
                operation,
                fault,
            } => (channel, operation, Err(fault)),
            _ => return None,
        };
        if self.pending == Some(Pending { channel, operation }) {
            self.pending = None;
            Some(result)
        } else {
            None
        }
    }

    pub fn cancel_if_idle(&mut self, power_busy: bool) -> bool {
        if self.pending.is_some() && !power_busy {
            self.pending = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;

    fn output() -> ChannelSnapshot {
        AppState::new(false, None).channels[0]
    }

    #[test]
    fn admission_preserves_busy_and_same_channel_cancel_ordering() {
        let mut transaction = OutputTransaction::new();
        let mut channel = output();
        channel.operation = 7;
        channel.requested_enabled = true;
        channel.transition = OutputTransition::Enabling(7);
        assert_eq!(
            transaction.record_request(2, true, &channel),
            RequestResult::Pending
        );

        assert_eq!(
            transaction.begin_request(2, true, true, false),
            Admission::Busy
        );
        assert_eq!(
            transaction.begin_request(1, false, false, false),
            Admission::Busy
        );
        assert_eq!(
            transaction.begin_request(2, false, true, true),
            Admission::ProceedAfterCancellation
        );
        assert!(!transaction.cancel_if_idle(false));
    }

    #[test]
    fn reducer_result_is_classified_as_pending_success_or_fault() {
        let mut transaction = OutputTransaction::new();
        let mut channel = output();
        channel.operation = 11;
        channel.requested_enabled = true;
        channel.transition = OutputTransition::Enabling(11);
        assert_eq!(
            transaction.record_request(0, true, &channel),
            RequestResult::Pending
        );

        let mut enabled = output();
        enabled.requested_enabled = true;
        enabled.physical_enabled = true;
        assert_eq!(
            OutputTransaction::new().record_request(0, true, &enabled),
            RequestResult::Complete(Ok(()))
        );

        let mut failed = output();
        failed.fault = Fault::OverCurrent;
        assert_eq!(
            OutputTransaction::new().record_request(0, true, &failed),
            RequestResult::Complete(Err(Fault::OverCurrent))
        );

        failed.fault = Fault::Hardware;
        assert_eq!(
            OutputTransaction::new().record_request(0, false, &failed),
            RequestResult::Complete(Ok(()))
        );

        failed.requested_enabled = true;
        assert_eq!(
            OutputTransaction::new().record_request(0, false, &failed),
            RequestResult::Complete(Err(Fault::Hardware))
        );
    }

    #[test]
    fn only_the_exact_operation_completion_gets_the_deferred_reply() {
        let mut transaction = OutputTransaction::new();
        let mut channel = output();
        channel.operation = 19;
        channel.requested_enabled = false;
        channel.physical_enabled = true;
        channel.transition = OutputTransition::Disabling(19);
        assert_eq!(
            transaction.record_request(4, false, &channel),
            RequestResult::Pending
        );

        assert_eq!(
            transaction.observe_completion(&Action::OutputApplied {
                channel: 3,
                operation: 19,
                enabled: false,
            }),
            None
        );
        assert!(!transaction.cancel_if_idle(true));
        assert_eq!(
            transaction.observe_completion(&Action::OutputFailed {
                channel: 4,
                operation: 19,
                fault: Fault::Hardware,
            }),
            Some(Err(Fault::Hardware))
        );
        assert!(!transaction.cancel_if_idle(false));
    }

    #[test]
    fn matching_success_wins_over_idle_cancellation() {
        let mut transaction = OutputTransaction::new();
        let mut channel = output();
        channel.operation = 23;
        channel.requested_enabled = true;
        channel.transition = OutputTransition::Enabling(23);
        transaction.record_request(1, true, &channel);

        assert_eq!(
            transaction.observe_completion(&Action::OutputApplied {
                channel: 1,
                operation: 23,
                enabled: true,
            }),
            Some(Ok(()))
        );
        assert!(!transaction.cancel_if_idle(false));
        assert_eq!(
            transaction.observe_completion(&Action::OutputApplied {
                channel: 1,
                operation: 23,
                enabled: true,
            }),
            None
        );
    }

    #[test]
    fn an_idle_executor_cancels_an_orphaned_request_once() {
        let mut transaction = OutputTransaction::new();
        let mut channel = output();
        channel.operation = 3;
        channel.requested_enabled = true;
        channel.transition = OutputTransition::Enabling(3);
        transaction.record_request(0, true, &channel);

        assert!(transaction.cancel_if_idle(false));
        assert!(!transaction.cancel_if_idle(false));
    }
}
