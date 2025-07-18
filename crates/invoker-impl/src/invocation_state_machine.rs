// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use super::*;

use restate_types::journal::Completion;
use restate_types::journal_v2::raw::RawEntry;
use restate_types::retries;
use restate_types::service_protocol::ServiceProtocolVersion;
use std::fmt;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// Component encapsulating the business logic of the invocation state machine
#[derive(Debug)]
pub(super) struct InvocationStateMachine {
    pub(super) invocation_target: InvocationTarget,
    pub(super) invocation_epoch: InvocationEpoch,
    selected_service_protocol: Option<ServiceProtocolVersion>,
    invocation_state: InvocationState,
    retry_iter: retries::RetryIter<'static>,
    /// This retry count is passed in the StartMessage.
    /// For more details of when we bump it, see [`InvokerError::should_bump_start_message_retry_count_since_last_stored_entry`].
    pub(super) start_message_retry_count_since_last_stored_command: u32,
}

/// This struct tracks which commands the invocation task generates,
/// and which ones have been already stored and acked by the partition processor.
/// This information is used to decide when it's safe to retry.
///
/// Every time the invocation task generates a new command, the index is notified to this struct with
/// [`JournalTracker::notify_command_sent_to_partition_processor`], and every time the invoker receives
/// [`InputCommand::StoredCommandAck`], the index is notified to this struct with [`JournalTracker::notify_acked_command_from_partition_processor`].
/// Similar story applies to notifications, because we need to wait for the self-proposed notifications from the SDK to be propagated to the
/// partition processor before retrying.
///
/// After the retry timer is fired, we can check whether we can retry immediately or not with [`JournalTracker::can_retry`].
///
/// **Note**: For Journal V1 Command == Entry
#[derive(Default, Debug, Clone)]
struct JournalTracker {
    last_acked_command_from_partition_processor: Option<CommandIndex>,
    last_command_sent_to_partition_processor: Option<CommandIndex>,
    last_acked_notifications_from_partition_processor: HashSet<NotificationId>,
    last_notifications_proposed_to_partition_processor: HashSet<NotificationId>,
}

impl JournalTracker {
    fn notify_acked_command_from_partition_processor(&mut self, idx: CommandIndex) {
        self.last_acked_command_from_partition_processor =
            cmp::max(Some(idx), self.last_acked_command_from_partition_processor)
    }

    fn notify_command_sent_to_partition_processor(&mut self, idx: CommandIndex) {
        self.last_command_sent_to_partition_processor =
            cmp::max(Some(idx), self.last_command_sent_to_partition_processor)
    }

    fn notify_acked_notification_from_partition_processor(&mut self, id: NotificationId) {
        self.last_acked_notifications_from_partition_processor
            .insert(id);
    }

    fn notify_notification_proposed_to_partition_processor(&mut self, id: NotificationId) {
        self.last_notifications_proposed_to_partition_processor
            .insert(id);
    }

    fn can_retry(&self) -> bool {
        let commands_condition = match (
            self.last_acked_command_from_partition_processor,
            self.last_command_sent_to_partition_processor,
        ) {
            (_, None) => {
                // The invocation task didn't generate new commands.
                // We're always good to retry in this case.
                true
            }
            (Some(last_acked_command), Some(last_proposed_command)) => {
                // Last acked must be higher than last sent,
                // otherwise we'll end up retrying when not all the commands have been stored.
                last_acked_command >= last_proposed_command
            }
            _ => false,
        };
        if !commands_condition {
            return false;
        }

        // If the notifications sent from PP to invoker contains all the ones sent from Invoker to PP, we're good to retry.
        let notifications_condition = self
            .last_notifications_proposed_to_partition_processor
            .is_subset(&self.last_acked_notifications_from_partition_processor);

        commands_condition && notifications_condition
    }
}

enum InvocationState {
    New,

    InFlight {
        // If there is no completions_tx,
        // then the stream is open in request/response mode
        notifications_tx: Option<mpsc::UnboundedSender<Notification>>,
        journal_tracker: JournalTracker,
        abort_handle: AbortHandle,

        // Acks that should be propagated back to the SDK
        command_acks_to_propagate: HashSet<CommandIndex>,

        // If Some, we need to notify the deployment id to the partition processor
        pinned_deployment: Option<PinnedDeployment>,
    },

    WaitingRetry {
        timer_fired: bool,
        journal_tracker: JournalTracker,
    },
}

impl fmt::Debug for InvocationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvocationState::New => f.write_str("New"),
            InvocationState::InFlight {
                journal_tracker,
                abort_handle,
                notifications_tx,
                ..
            } => f
                .debug_struct("InFlight")
                .field("journal_tracker", journal_tracker)
                .field("abort_handle", abort_handle)
                .field(
                    "notifications_tx_open",
                    &notifications_tx
                        .as_ref()
                        .map(|s| !s.is_closed())
                        .unwrap_or(false),
                )
                .finish(),
            InvocationState::WaitingRetry {
                journal_tracker,
                timer_fired,
            } => f
                .debug_struct("WaitingRetry")
                .field("journal_tracker", journal_tracker)
                .field("timer_fired", timer_fired)
                .finish(),
        }
    }
}

impl InvocationStateMachine {
    pub(super) fn create(
        invocation_target: InvocationTarget,
        invocation_epoch: InvocationEpoch,
        retry_policy: RetryPolicy,
    ) -> InvocationStateMachine {
        Self {
            invocation_target,
            invocation_epoch,
            selected_service_protocol: None,
            invocation_state: InvocationState::New,
            retry_iter: retry_policy.into_iter(),
            start_message_retry_count_since_last_stored_command: 0,
        }
    }

    pub(super) fn start(
        &mut self,
        abort_handle: AbortHandle,
        notifications_tx: mpsc::UnboundedSender<Notification>,
    ) {
        debug_assert!(matches!(
            &self.invocation_state,
            InvocationState::New | InvocationState::WaitingRetry { .. }
        ));

        self.invocation_state = InvocationState::InFlight {
            notifications_tx: Some(notifications_tx),
            journal_tracker: Default::default(),
            abort_handle,
            command_acks_to_propagate: Default::default(),
            pinned_deployment: None,
        };
    }

    pub(super) fn abort(&mut self) {
        if let InvocationState::InFlight { abort_handle, .. } = &mut self.invocation_state {
            abort_handle.abort();
        }
    }

    pub(super) fn notify_pinned_deployment(&mut self, deployment: PinnedDeployment) {
        debug_assert!(matches!(
            &self.invocation_state,
            InvocationState::InFlight {
                pinned_deployment: None,
                ..
            }
        ));

        if let InvocationState::InFlight {
            pinned_deployment, ..
        } = &mut self.invocation_state
        {
            *pinned_deployment = Some(deployment);
        }
    }

    pub(super) fn notify_selected_service_protocol(
        &mut self,
        service_protocol_version: ServiceProtocolVersion,
    ) {
        self.selected_service_protocol = Some(service_protocol_version);
    }

    pub(super) fn selected_service_protocol(&self) -> Option<ServiceProtocolVersion> {
        self.selected_service_protocol
    }

    pub(super) fn pinned_deployment_to_notify(&mut self) -> Option<PinnedDeployment> {
        debug_assert!(matches!(
            &self.invocation_state,
            InvocationState::InFlight { .. }
        ));

        if let InvocationState::InFlight {
            pinned_deployment, ..
        } = &mut self.invocation_state
        {
            if let Some(pinned_deployment) = pinned_deployment.take() {
                // When notifying the pinned deployment, we also set the protocol version
                self.selected_service_protocol = Some(pinned_deployment.service_protocol_version);
                Some(pinned_deployment)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub(super) fn notify_new_command(&mut self, command_index: CommandIndex, requires_ack: bool) {
        debug_assert!(matches!(
            &self.invocation_state,
            InvocationState::InFlight { .. }
        ));

        self.start_message_retry_count_since_last_stored_command = 0;

        if let InvocationState::InFlight {
            journal_tracker,
            command_acks_to_propagate: entries_to_ack,
            ..
        } = &mut self.invocation_state
        {
            if requires_ack {
                entries_to_ack.insert(command_index);
            }
            journal_tracker.notify_command_sent_to_partition_processor(command_index);
        }
    }

    pub(super) fn notify_new_notification_proposal(&mut self, notification_id: NotificationId) {
        debug_assert!(matches!(
            &self.invocation_state,
            InvocationState::InFlight { .. }
        ));

        if let InvocationState::InFlight {
            journal_tracker, ..
        } = &mut self.invocation_state
        {
            journal_tracker.notify_notification_proposed_to_partition_processor(notification_id);
        }
    }

    pub(super) fn notify_stored_ack(&mut self, command_index: CommandIndex) {
        match &mut self.invocation_state {
            InvocationState::InFlight {
                journal_tracker,
                command_acks_to_propagate,
                notifications_tx,
                ..
            } => {
                if command_acks_to_propagate.remove(&command_index) {
                    Self::try_send_notification(notifications_tx, Notification::Ack(command_index));
                }
                journal_tracker.notify_acked_command_from_partition_processor(command_index);
            }
            InvocationState::WaitingRetry {
                journal_tracker, ..
            } => {
                journal_tracker.notify_acked_command_from_partition_processor(command_index);
            }
            _ => {}
        }
    }

    pub(super) fn notify_completion(&mut self, completion: Completion) {
        if let InvocationState::InFlight {
            notifications_tx, ..
        } = &mut self.invocation_state
        {
            Self::try_send_notification(notifications_tx, Notification::Completion(completion));
        }
    }

    pub(super) fn notify_entry(&mut self, entry: RawEntry) {
        match &mut self.invocation_state {
            InvocationState::InFlight {
                journal_tracker,
                notifications_tx,
                ..
            } => {
                if let journal_v2::raw::RawEntry::Notification(notif) = &entry {
                    journal_tracker.notify_acked_notification_from_partition_processor(notif.id());
                }

                Self::try_send_notification(notifications_tx, Notification::Entry(entry));
            }
            InvocationState::WaitingRetry {
                journal_tracker, ..
            } => {
                if let journal_v2::raw::RawEntry::Notification(notif) = &entry {
                    journal_tracker.notify_acked_notification_from_partition_processor(notif.id());
                }
            }
            _ => {}
        }
    }

    pub(super) fn try_send_notification(
        notifications_tx: &mut Option<mpsc::UnboundedSender<Notification>>,
        notification: Notification,
    ) {
        *notifications_tx = notifications_tx.take().and_then(move |sender| {
            if sender.send(notification).is_ok() {
                Some(sender)
            } else {
                None
            }
        });
    }

    pub(super) fn notify_retry_timer_fired(&mut self) {
        debug_assert!(matches!(
            &self.invocation_state,
            InvocationState::WaitingRetry { .. }
        ));

        if let InvocationState::WaitingRetry { timer_fired, .. } = &mut self.invocation_state {
            *timer_fired = true;
        }
    }

    /// Returns Some() with the timer for the next retry, otherwise None if retry limit exhausted
    pub(super) fn handle_task_error(
        &mut self,
        next_retry_interval_override: Option<Duration>,
        should_bump_start_message_retry_count_since_last_stored_command: bool,
    ) -> Option<Duration> {
        let journal_tracker = match &self.invocation_state {
            InvocationState::InFlight {
                journal_tracker, ..
            } => journal_tracker.clone(),
            InvocationState::New => JournalTracker::default(),
            InvocationState::WaitingRetry {
                journal_tracker,
                timer_fired,
            } => {
                // TODO: https://github.com/restatedev/restate/issues/538
                assert!(
                    timer_fired,
                    "Restate does not support multiple retry timers yet. This would require \
                        deduplicating timers by some mean (e.g. fencing them off, overwriting \
                        old timers, not registering a new timer if an old timer has not fired yet, etc.)"
                );
                journal_tracker.clone()
            }
        };

        let next_timer = next_retry_interval_override.or_else(|| self.retry_iter.next());
        if next_timer.is_some() {
            if should_bump_start_message_retry_count_since_last_stored_command {
                self.start_message_retry_count_since_last_stored_command += 1;
            }
            self.invocation_state = InvocationState::WaitingRetry {
                timer_fired: false,
                journal_tracker,
            };
            next_timer
        } else {
            None
        }
    }

    pub(super) fn is_ready_to_retry(&self) -> bool {
        match &self.invocation_state {
            InvocationState::WaitingRetry {
                timer_fired,
                journal_tracker,
            } => *timer_fired && journal_tracker.can_retry(),
            _ => false,
        }
    }

    #[inline]
    pub(super) fn invocation_state_debug(&self) -> impl fmt::Debug + '_ {
        &self.invocation_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use googletest::matchers::{eq, some};
    use googletest::prelude::err;
    use googletest::{assert_that, pat};
    use std::time::Duration;
    use test_log::test;
    use tokio::sync::mpsc::error::TryRecvError;

    use restate_test_util::{assert, check, let_assert};
    use restate_types::journal_v2::{CompletionType, NotificationType};

    #[test]
    fn handle_error_when_waiting_for_retry() {
        let mut invocation_state_machine = InvocationStateMachine::create(
            InvocationTarget::mock_virtual_object(),
            0,
            RetryPolicy::fixed_delay(Duration::from_secs(1), Some(10)),
        );

        assert!(
            invocation_state_machine
                .handle_task_error(None, true)
                .is_some()
        );
        check!(let InvocationState::WaitingRetry { .. } = invocation_state_machine.invocation_state);

        invocation_state_machine.notify_retry_timer_fired();

        // We stay in `WaitingForRetry`
        assert!(
            invocation_state_machine
                .handle_task_error(None, true)
                .is_some()
        );
        check!(let InvocationState::WaitingRetry { .. } = invocation_state_machine.invocation_state);
    }

    #[test(tokio::test)]
    async fn handle_error_counts_attempts_on_same_entry() {
        let mut invocation_state_machine = InvocationStateMachine::create(
            InvocationTarget::mock_virtual_object(),
            0,
            RetryPolicy::fixed_delay(Duration::from_secs(1), Some(10)),
        );

        // Start invocation
        invocation_state_machine.start(
            tokio::spawn(async {}).abort_handle(),
            mpsc::unbounded_channel().0,
        );

        // Notify error
        assert!(
            invocation_state_machine
                .handle_task_error(None, true)
                .is_some()
        );
        assert_eq!(
            invocation_state_machine.start_message_retry_count_since_last_stored_command,
            1
        );

        // Try to start again
        invocation_state_machine.start(
            tokio::spawn(async {}).abort_handle(),
            mpsc::unbounded_channel().0,
        );

        // Get error again
        assert!(
            invocation_state_machine
                .handle_task_error(None, true)
                .is_some()
        );
        assert_eq!(
            invocation_state_machine.start_message_retry_count_since_last_stored_command,
            2
        );

        // Try to start again
        invocation_state_machine.start(
            tokio::spawn(async {}).abort_handle(),
            mpsc::unbounded_channel().0,
        );
        assert_eq!(
            invocation_state_machine.start_message_retry_count_since_last_stored_command,
            2
        );

        // Now complete the entry
        invocation_state_machine.notify_new_command(1, false);
        assert_eq!(
            invocation_state_machine.start_message_retry_count_since_last_stored_command,
            0
        );
    }

    #[test(tokio::test)]
    async fn handle_requires_ack() {
        let mut invocation_state_machine = InvocationStateMachine::create(
            InvocationTarget::mock_virtual_object(),
            0,
            RetryPolicy::fixed_delay(Duration::from_secs(1), Some(10)),
        );

        let abort_handle = tokio::spawn(async {}).abort_handle();
        let (tx, mut rx) = mpsc::unbounded_channel();

        invocation_state_machine.start(abort_handle, tx);
        invocation_state_machine.notify_new_command(1, true);
        invocation_state_machine.notify_new_command(2, false);
        invocation_state_machine.notify_new_command(3, true);

        invocation_state_machine.notify_stored_ack(1);
        invocation_state_machine.notify_stored_ack(2);
        invocation_state_machine.notify_stored_ack(3);

        // Check notification was sent for ack 1 and 3
        let notification = rx.recv().await;
        assert_that!(notification, some(pat!(Notification::Ack(eq(1)))));
        let notification = rx.recv().await;
        assert_that!(notification, some(pat!(Notification::Ack(eq(3)))));

        // Channel should be empty
        let try_recv = rx.try_recv();
        assert_that!(try_recv, err(eq(TryRecvError::Empty)));
    }

    #[test(tokio::test)]
    async fn journal_tracker_correctly_tracks_commands() {
        let mut invocation_state_machine = InvocationStateMachine::create(
            InvocationTarget::mock_service(),
            0,
            RetryPolicy::fixed_delay(Duration::from_secs(1), Some(10)),
        );

        let abort_handle = tokio::spawn(async {}).abort_handle();
        let (tx, _rx) = mpsc::unbounded_channel();

        invocation_state_machine.start(abort_handle, tx);

        // Invoker generates entry 1
        invocation_state_machine.notify_new_command(1, false);
        let_assert!(Some(_) = invocation_state_machine.handle_task_error(None, true));

        // PP sends ack for command 1
        invocation_state_machine.notify_stored_ack(1);

        // Still waiting retry timer fired
        assert!(!invocation_state_machine.is_ready_to_retry());
        assert!(let InvocationState::WaitingRetry { .. } = invocation_state_machine.invocation_state);

        // After the retry timer fires, we're ready to retry
        invocation_state_machine.notify_retry_timer_fired();
        assert!(invocation_state_machine.is_ready_to_retry());
    }

    #[test(tokio::test)]
    async fn journal_tracker_correctly_tracks_notification_proposals() {
        let mut invocation_state_machine = InvocationStateMachine::create(
            InvocationTarget::mock_service(),
            0,
            RetryPolicy::fixed_delay(Duration::from_secs(1), Some(10)),
        );

        let abort_handle = tokio::spawn(async {}).abort_handle();
        let (tx, _rx) = mpsc::unbounded_channel();

        invocation_state_machine.start(abort_handle, tx);
        invocation_state_machine.notify_new_notification_proposal(NotificationId::SignalIndex(18));
        invocation_state_machine.notify_new_notification_proposal(NotificationId::CompletionId(1));
        let_assert!(Some(_) = invocation_state_machine.handle_task_error(None, true));

        // Waiting notifications acks and retry timer fired
        assert!(!invocation_state_machine.is_ready_to_retry());
        assert!(let InvocationState::WaitingRetry { .. } = invocation_state_machine.invocation_state);

        // Got signal 18
        invocation_state_machine.notify_entry(RawEntry::Notification(RawNotification::new(
            NotificationType::Signal,
            NotificationId::SignalIndex(18),
            Bytes::default(),
        )));

        // Retry timer fired
        invocation_state_machine.notify_retry_timer_fired();

        // Waiting notifications acks
        assert!(!invocation_state_machine.is_ready_to_retry());
        assert!(let InvocationState::WaitingRetry { .. } = invocation_state_machine.invocation_state);

        // For whatever reason notification index 2
        invocation_state_machine.notify_entry(RawEntry::Notification(RawNotification::new(
            NotificationType::Completion(CompletionType::Run),
            NotificationId::CompletionId(2),
            Bytes::default(),
        )));

        // Still waiting completion id 1
        assert!(!invocation_state_machine.is_ready_to_retry());
        assert!(let InvocationState::WaitingRetry { .. } = invocation_state_machine.invocation_state);

        // Send notification index 1
        invocation_state_machine.notify_entry(RawEntry::Notification(RawNotification::new(
            NotificationType::Completion(CompletionType::Run),
            NotificationId::CompletionId(1),
            Bytes::default(),
        )));

        // Ready to retry
        assert!(invocation_state_machine.is_ready_to_retry());
    }
}
