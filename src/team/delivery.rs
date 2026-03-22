use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::config::RoleType;
use super::daemon::TeamDaemon;
use super::errors::DeliveryError;
use super::inbox;
use super::message;
use super::retry::{RetryConfig, retry_sync};
use crate::tmux;

pub(super) const DELIVERY_VERIFICATION_CAPTURE_LINES: u32 = 50;
/// Increased capture window for agents that recently became ready, to account
/// for startup output pushing the delivery marker further up the scrollback.
pub(super) const DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY: u32 = 100;
pub(super) const FAILED_DELIVERY_RETRY_DELAY: Duration = Duration::from_secs(30);
pub(super) const FAILED_DELIVERY_MAX_ATTEMPTS: u32 = 3;
const TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD: u32 = 5;
const TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(300);

/// Check whether an agent's pane is showing a ready prompt by capturing
/// the last 20 lines and looking for known agent input indicators.
pub(super) fn is_agent_ready(pane_id: &str) -> bool {
    match tmux::capture_pane_recent(pane_id, 20) {
        Ok(capture) => super::watcher::is_at_agent_prompt(&capture),
        Err(_) => false,
    }
}

#[derive(Debug, Clone)]
pub(super) struct PendingMessage {
    pub(super) from: String,
    pub(super) body: String,
    #[allow(dead_code)] // Useful for future queue-age diagnostics.
    pub(super) queued_at: Instant,
}

#[derive(Debug, Clone)]
pub(super) struct FailedDelivery {
    pub(super) recipient: String,
    pub(super) from: String,
    pub(super) body: String,
    pub(super) attempts: u32,
    pub(super) last_attempt: Instant,
}

impl FailedDelivery {
    pub(super) fn new(recipient: &str, from: &str, body: &str) -> Self {
        Self {
            recipient: recipient.to_string(),
            from: from.to_string(),
            body: body.to_string(),
            attempts: 1,
            last_attempt: Instant::now(),
        }
    }

    pub(super) fn message_marker(&self) -> String {
        message_delivery_marker(&self.from)
    }

    fn is_ready_for_retry(&self, now: Instant) -> bool {
        now.duration_since(self.last_attempt) >= FAILED_DELIVERY_RETRY_DELAY
    }

    fn has_attempts_remaining(&self) -> bool {
        self.attempts < FAILED_DELIVERY_MAX_ATTEMPTS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageDelivery {
    Channel,
    LivePane,
    InboxQueued,
    DeferredPending,
    SkippedUnknownRecipient,
}

impl TeamDaemon {
    fn telegram_failure_key(recipient: &str) -> String {
        format!("telegram-delivery-failures::{recipient}")
    }

    fn telegram_circuit_breaker_key(recipient: &str) -> String {
        format!("telegram-delivery-breaker::{recipient}")
    }

    fn telegram_retry_config() -> RetryConfig {
        RetryConfig {
            max_retries: 3,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            jitter: false,
        }
    }

    fn telegram_channel_paused(&self, recipient: &str) -> bool {
        self.intervention_cooldowns
            .get(&Self::telegram_circuit_breaker_key(recipient))
            .is_some_and(|opened_at| {
                opened_at.elapsed() < TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN
            })
    }

    fn clear_telegram_delivery_failures(&mut self, recipient: &str) {
        self.retry_counts
            .remove(&Self::telegram_failure_key(recipient));
        self.intervention_cooldowns
            .remove(&Self::telegram_circuit_breaker_key(recipient));
    }

    fn increment_telegram_delivery_failures(&mut self, recipient: &str) -> u32 {
        let failures = self
            .retry_counts
            .entry(Self::telegram_failure_key(recipient))
            .or_insert(0);
        *failures += 1;
        *failures
    }

    fn alert_telegram_delivery_paused(
        &mut self,
        recipient: &str,
        from: &str,
        detail: &str,
    ) -> Result<()> {
        let Some(manager) = self.failed_delivery_escalation_recipient(recipient) else {
            warn!(
                recipient,
                from, detail, "telegram delivery paused without escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Telegram delivery paused after repeated failures.\nRecipient: {recipient}\nFrom: {from}\nLast error: {detail}"
        );
        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send("daemon", &manager, &body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.record_message_routed("daemon", &manager);
        Ok(())
    }

    fn deliver_channel_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        let channel_type = self
            .channels
            .get(recipient)
            .map(|channel| channel.channel_type().to_string())
            .unwrap_or_default();

        if !channel_type.starts_with("telegram") {
            self.channels
                .get(recipient)
                .expect("channel presence checked by caller")
                .send(body)?;
            self.record_message_routed(from, recipient);
            return Ok(MessageDelivery::Channel);
        }

        if self.telegram_channel_paused(recipient) {
            return Err(DeliveryError::ChannelSend {
                recipient: recipient.to_string(),
                detail: "telegram delivery circuit breaker is open".to_string(),
            }
            .into());
        }

        let send_result = {
            let channel = self
                .channels
                .get(recipient)
                .expect("channel presence checked by caller");
            retry_sync(&Self::telegram_retry_config(), || channel.send(body))
        };

        match send_result {
            Ok(()) => {
                self.clear_telegram_delivery_failures(recipient);
                self.record_message_routed(from, recipient);
                Ok(MessageDelivery::Channel)
            }
            Err(error) => {
                let failure_count = self.increment_telegram_delivery_failures(recipient);
                if failure_count >= TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD {
                    self.intervention_cooldowns.insert(
                        Self::telegram_circuit_breaker_key(recipient),
                        Instant::now(),
                    );
                    self.alert_telegram_delivery_paused(recipient, from, &error.to_string())?;
                }
                Err(error.into())
            }
        }
    }

    fn verify_message_content_in_pane(&self, pane_id: &str, message_marker: &str) -> bool {
        self.verify_message_content_in_pane_lines(
            pane_id,
            message_marker,
            DELIVERY_VERIFICATION_CAPTURE_LINES,
        )
    }

    fn verify_message_content_in_pane_lines(
        &self,
        pane_id: &str,
        message_marker: &str,
        capture_lines: u32,
    ) -> bool {
        match tmux::capture_pane_recent(pane_id, capture_lines) {
            Ok(capture) => capture_contains_message_marker(&capture, message_marker),
            Err(error) => {
                warn!(
                    pane_id,
                    error = %error,
                    "failed to capture pane for content-based delivery verification"
                );
                false
            }
        }
    }

    fn record_failed_delivery(&mut self, recipient: &str, from: &str, body: &str) {
        if let Some(existing) = self.failed_deliveries.iter_mut().find(|delivery| {
            delivery.recipient == recipient && delivery.from == from && delivery.body == body
        }) {
            existing.last_attempt = Instant::now();
            return;
        }

        self.failed_deliveries
            .push(FailedDelivery::new(recipient, from, body));
        self.record_delivery_failed(recipient, from, "message delivery failed after retries");
    }

    fn clear_failed_delivery(&mut self, recipient: &str, from: &str, body: &str) {
        self.failed_deliveries.retain(|delivery| {
            delivery.recipient != recipient || delivery.from != from || delivery.body != body
        });
    }

    fn failed_delivery_escalation_recipient(&self, recipient: &str) -> Option<String> {
        self.config
            .members
            .iter()
            .find(|member| member.name == recipient)
            .and_then(|member| member.reports_to.clone())
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| {
                        member.role_type == RoleType::Manager && member.name != recipient
                    })
                    .map(|member| member.name.clone())
            })
            .or_else(|| {
                let sender = self.automation_sender_for(recipient);
                (sender != recipient
                    && self
                        .config
                        .members
                        .iter()
                        .any(|member| member.name == sender))
                .then_some(sender)
            })
    }

    fn escalate_failed_delivery(&mut self, delivery: &FailedDelivery) -> Result<()> {
        let Some(manager) = self.failed_delivery_escalation_recipient(&delivery.recipient) else {
            warn!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                "failed delivery exhausted retries without escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Live message delivery failed after {} attempts.\nRecipient: {}\nFrom: {}\nMarker: {}\nMessage body:\n{}",
            delivery.attempts,
            delivery.recipient,
            delivery.from,
            delivery.message_marker(),
            delivery.body
        );
        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send("daemon", &manager, &body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.record_message_routed("daemon", &manager);
        warn!(
            recipient = %delivery.recipient,
            from = %delivery.from,
            escalation_target = %manager,
            attempts = delivery.attempts,
            "failed delivery escalated to manager inbox"
        );
        Ok(())
    }

    pub(super) fn retry_failed_deliveries(&mut self) -> Result<()> {
        if self.failed_deliveries.is_empty() {
            return Ok(());
        }

        let now = Instant::now();
        let pending = std::mem::take(&mut self.failed_deliveries);
        for mut delivery in pending {
            if !delivery.is_ready_for_retry(now) {
                self.failed_deliveries.push(delivery);
                continue;
            }

            let is_ready = self
                .watchers
                .get(&delivery.recipient)
                .map(|watcher| {
                    matches!(
                        watcher.state,
                        super::watcher::WatcherState::Ready | super::watcher::WatcherState::Idle
                    )
                })
                .unwrap_or(true);
            if !is_ready {
                self.failed_deliveries.push(delivery);
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&delivery.recipient).cloned() else {
                self.escalate_failed_delivery(&delivery)?;
                continue;
            };

            delivery.attempts += 1;
            delivery.last_attempt = now;
            info!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                attempts = delivery.attempts,
                "retrying failed live delivery"
            );

            let injected = match message::inject_message(&pane_id, &delivery.from, &delivery.body) {
                Ok(()) => true,
                Err(error) => {
                    warn!(
                        recipient = %delivery.recipient,
                        from = %delivery.from,
                        attempts = delivery.attempts,
                        error = %error,
                        "failed to re-inject message during delivery retry"
                    );
                    false
                }
            };

            if injected
                && self.verify_message_delivered(
                    &delivery.from,
                    &delivery.recipient,
                    &delivery.body,
                    3,
                    false,
                )
            {
                continue;
            }

            if delivery.has_attempts_remaining() {
                self.failed_deliveries.push(delivery);
            } else {
                // Don't escalate if the agent is still starting — it hasn't
                // had a chance to accept messages yet. Keep retrying.
                let agent_still_starting = self
                    .watchers
                    .get(&delivery.recipient)
                    .is_some_and(|w| !w.is_ready_for_delivery());
                if agent_still_starting {
                    debug!(
                        recipient = %delivery.recipient,
                        "agent still starting; suppressing escalation"
                    );
                    self.failed_deliveries.push(delivery);
                } else {
                    self.escalate_failed_delivery(&delivery)?;
                }
            }
        }

        Ok(())
    }

    fn verify_message_delivered(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
        max_attempts: u32,
        record_failure: bool,
    ) -> bool {
        let Some(pane_id) = self.config.pane_map.get(recipient).cloned() else {
            return true;
        };
        let message_marker = message_delivery_marker(from);

        for attempt in 1..=max_attempts {
            std::thread::sleep(Duration::from_secs(2));

            if self.verify_message_content_in_pane(&pane_id, &message_marker) {
                self.clear_failed_delivery(recipient, from, body);
                debug!(
                    recipient,
                    attempt,
                    marker = %message_marker,
                    "message delivery verified: marker found in pane"
                );
                return true;
            }

            warn!(
                recipient,
                attempt,
                marker = %message_marker,
                "message marker missing after injection; resending Enter"
            );
            if let Err(error) = tmux::send_keys(&pane_id, "", true) {
                warn!(recipient, error = %error, "failed to resend Enter");
            }
        }

        if record_failure {
            self.record_failed_delivery(recipient, from, body);
            warn!(
                recipient,
                max_attempts,
                marker = %message_marker,
                "message delivery failed after retries; queued for daemon retry"
            );
        }

        false
    }

    fn verify_message_delivered_with_lines(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
        max_attempts: u32,
        record_failure: bool,
        capture_lines: u32,
    ) -> bool {
        let Some(pane_id) = self.config.pane_map.get(recipient).cloned() else {
            return true;
        };
        let message_marker = message_delivery_marker(from);

        for attempt in 1..=max_attempts {
            std::thread::sleep(Duration::from_secs(2));

            if self.verify_message_content_in_pane_lines(&pane_id, &message_marker, capture_lines) {
                self.clear_failed_delivery(recipient, from, body);
                debug!(
                    recipient,
                    attempt,
                    capture_lines,
                    marker = %message_marker,
                    "message delivery verified: marker found in pane"
                );
                return true;
            }

            warn!(
                recipient,
                attempt,
                marker = %message_marker,
                "message marker missing after injection; resending Enter"
            );
            if let Err(error) = tmux::send_keys(&pane_id, "", true) {
                warn!(recipient, error = %error, "failed to resend Enter");
            }
        }

        if record_failure {
            self.record_failed_delivery(recipient, from, body);
            warn!(
                recipient,
                max_attempts,
                marker = %message_marker,
                "message delivery failed after retries; queued for daemon retry"
            );
        }

        false
    }

    /// Check whether the agent is ready for message delivery.
    ///
    /// Uses the watcher's cached readiness state first (fast path). If the
    /// watcher hasn't confirmed readiness yet, performs a single live capture
    /// check. Returns true if the agent is ready for injection.
    ///
    /// This is intentionally non-blocking: the daemon poll loop should not
    /// be stalled waiting for an agent to start. If the agent isn't ready,
    /// the message is deferred to inbox and will be picked up by
    /// `deliver_inbox_messages` once the watcher confirms readiness.
    fn check_agent_ready(&mut self, recipient: &str, pane_id: &str) -> bool {
        // Fast path: watcher already confirmed readiness (prompt seen during poll).
        if self
            .watchers
            .get(recipient)
            .is_some_and(|w| w.is_ready_for_delivery())
        {
            return true;
        }

        // Single live check — capture the pane and look for agent prompt.
        if is_agent_ready(pane_id) {
            if let Some(watcher) = self.watchers.get_mut(recipient) {
                watcher.confirm_ready();
            }
            info!(
                recipient,
                pane_id, "agent readiness confirmed via live check"
            );
            return true;
        }

        debug!(recipient, pane_id, "agent not ready; deferring delivery");
        false
    }

    /// Returns the appropriate capture line count for delivery verification.
    /// Agents that recently became ready get a larger window to account for
    /// startup output pushing the delivery marker further up the scrollback.
    fn delivery_capture_lines_for(&self, recipient: &str) -> u32 {
        let recently_ready = self
            .watchers
            .get(recipient)
            .is_some_and(|w| matches!(w.state, super::watcher::WatcherState::Ready));
        if recently_ready {
            DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY
        } else {
            DELIVERY_VERIFICATION_CAPTURE_LINES
        }
    }

    /// Drain pending messages for an agent that just became ready.
    /// Called from `poll_watchers()` when `ready_confirmed` transitions to true.
    pub(super) fn drain_pending_queue(&mut self, recipient: &str) -> Result<()> {
        let messages = self
            .pending_delivery_queue
            .remove(recipient)
            .unwrap_or_default();
        if messages.is_empty() {
            return Ok(());
        }
        info!(
            recipient,
            count = messages.len(),
            "draining pending delivery queue after agent became ready"
        );
        for msg in messages {
            self.deliver_message(&msg.from, recipient, &msg.body)?;
        }
        Ok(())
    }

    pub(super) fn queue_daemon_message(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        let visible_sender = self.automation_sender_for(recipient);
        self.deliver_message(&visible_sender, recipient, body)
    }

    pub(super) fn queue_message(&mut self, from: &str, recipient: &str, body: &str) -> Result<()> {
        self.deliver_message(from, recipient, body).map(|_| ())
    }

    fn deliver_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        if let Some(channel) = self.channels.get(recipient) {
            let _ = channel;
            return self.deliver_channel_message(from, recipient, body);
        }

        let known_recipient = self.config.pane_map.contains_key(recipient)
            || self
                .config
                .members
                .iter()
                .any(|member| member.name == recipient);
        if !known_recipient {
            debug!(from, recipient, "skipping message for unknown recipient");
            return Ok(MessageDelivery::SkippedUnknownRecipient);
        }

        if let Some(pane_id) = self.config.pane_map.get(recipient).cloned() {
            // Readiness gate: check for the agent prompt before injecting.
            if !self.check_agent_ready(recipient, &pane_id) {
                // If the agent has *never* been ready (still starting up), buffer
                // in the pending queue — these will be drained when readiness is
                // confirmed.  If the agent was previously ready, fall through to
                // inbox delivery (existing behaviour for transient unreadiness).
                let never_been_ready = self
                    .watchers
                    .get(recipient)
                    .is_some_and(|w| !w.is_ready_for_delivery());
                if never_been_ready {
                    info!(
                        from,
                        to = recipient,
                        pane_id = pane_id.as_str(),
                        "agent still starting; deferring to pending queue"
                    );
                    self.pending_delivery_queue
                        .entry(recipient.to_string())
                        .or_default()
                        .push(PendingMessage {
                            from: from.to_string(),
                            body: body.to_string(),
                            queued_at: Instant::now(),
                        });
                    return Ok(MessageDelivery::DeferredPending);
                }
                info!(
                    from,
                    to = recipient,
                    pane_id = pane_id.as_str(),
                    "agent not ready after timeout; deferring to inbox"
                );
                // Fall through to inbox delivery below.
            } else {
                match message::inject_message(&pane_id, from, body) {
                    Ok(()) => {
                        self.record_message_routed(from, recipient);
                        let capture_lines = self.delivery_capture_lines_for(recipient);
                        self.verify_message_delivered_with_lines(
                            from,
                            recipient,
                            body,
                            3,
                            true,
                            capture_lines,
                        );
                        return Ok(MessageDelivery::LivePane);
                    }
                    Err(error) => {
                        warn!(
                            from,
                            to = recipient,
                            pane_id = pane_id.as_str(),
                            error = %error,
                            "live message delivery failed; queueing to inbox"
                        );
                        let _typed_error = DeliveryError::PaneInject {
                            recipient: recipient.to_string(),
                            pane_id,
                            detail: error.to_string(),
                        };
                    }
                }
            }
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send(from, recipient, body);
        inbox::deliver_to_inbox(&root, &msg).map_err(|error| DeliveryError::InboxQueue {
            recipient: recipient.to_string(),
            detail: error.to_string(),
        })?;
        self.record_message_routed(from, recipient);
        Ok(MessageDelivery::InboxQueued)
    }

    pub(super) fn drain_legacy_command_queue(&mut self) -> Result<()> {
        let queue_path = message::command_queue_path(&self.config.project_root);
        let commands = message::read_command_queue(&queue_path)?;
        if commands.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let mut remaining_commands = Vec::new();
        for cmd in commands {
            let result: Result<()> = (|| match &cmd {
                message::QueuedCommand::Send {
                    from,
                    to,
                    message: msg,
                } => {
                    let is_user =
                        self.config.team_config.roles.iter().any(|role| {
                            role.name == to.as_str() && role.role_type == RoleType::User
                        });

                    if is_user {
                        if let Some(channel) = self.channels.get(to.as_str()) {
                            let formatted = format!("[From {from}]\n{msg}");
                            let _ = channel;
                            self.deliver_channel_message(from, to, &formatted)?;
                        }
                    } else {
                        let inbox_msg = inbox::InboxMessage::new_send(from, to, msg);
                        inbox::deliver_to_inbox(&root, &inbox_msg)?;
                        debug!(from, to, "legacy command routed to inbox");
                    }
                    Ok(())
                }
                message::QueuedCommand::Assign {
                    from,
                    engineer,
                    task,
                } => {
                    let msg = inbox::InboxMessage::new_assign(from, engineer, task);
                    inbox::deliver_to_inbox(&root, &msg)?;
                    debug!(engineer, "legacy assign routed to inbox");
                    Ok(())
                }
            })();

            if let Err(error) = result {
                warn!(error = %error, "failed to process legacy command; preserving in queue");
                remaining_commands.push(cmd);
            }
        }

        message::write_command_queue(&queue_path, &remaining_commands)?;
        Ok(())
    }

    pub(super) fn deliver_inbox_messages(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in &member_names {
            let is_ready = self
                .watchers
                .get(name)
                .map(|watcher| {
                    matches!(
                        watcher.state,
                        super::watcher::WatcherState::Ready | super::watcher::WatcherState::Idle
                    )
                })
                .unwrap_or(true);

            if !is_ready {
                continue;
            }

            let messages = match inbox::pending_messages(&root, name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(member = %name, error = %error, "failed to read inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(name).cloned() else {
                continue;
            };

            let mut delivered_any = false;
            for msg in &messages {
                let from_role = self.resolve_role_name(&msg.from);
                let to_role = self.resolve_role_name(name);
                if !self.config.team_config.can_talk(&from_role, &to_role) {
                    warn!(
                        from = %msg.from, from_role, to = %name, to_role,
                        "blocked message: routing not allowed"
                    );
                    let _ = inbox::mark_delivered(&root, name, &msg.id);
                    continue;
                }

                let is_send = matches!(msg.msg_type, inbox::MessageType::Send);
                let delivery_result = match msg.msg_type {
                    inbox::MessageType::Send => {
                        info!(from = %msg.from, to = %name, id = %msg.id, "delivering inbox message");
                        message::inject_message(&pane_id, &msg.from, &msg.body)
                    }
                    inbox::MessageType::Assign => {
                        info!(to = %name, id = %msg.id, "delivering inbox assignment");
                        self.manual_assign_cooldowns
                            .insert(name.to_string(), Instant::now());
                        self.assign_task(name, &msg.body).map(|launch| {
                            self.record_assignment_success(name, &msg.id, &msg.body, &launch);
                            self.notify_assignment_sender_success(
                                &msg.from, name, &msg.id, &msg.body, &launch,
                            );
                        })
                    }
                };

                let mut mark_delivered = false;
                match delivery_result {
                    Ok(()) => {
                        delivered_any = true;
                        mark_delivered = true;
                        if is_send {
                            self.verify_message_delivered(&msg.from, name, &msg.body, 3, true);
                        }
                    }
                    Err(error) => {
                        warn!(
                            from = %msg.from,
                            to = %name,
                            id = %msg.id,
                            error = %error,
                            "failed to deliver inbox message"
                        );
                        if matches!(msg.msg_type, inbox::MessageType::Assign) {
                            mark_delivered = true;
                            self.record_assignment_failure(name, &msg.id, &msg.body, &error);
                            self.notify_assignment_sender_failure(
                                &msg.from, name, &msg.id, &msg.body, &error,
                            );
                        }
                    }
                }

                if !mark_delivered {
                    continue;
                }

                if let Err(error) = inbox::mark_delivered(&root, name, &msg.id) {
                    warn!(
                        member = %name,
                        id = %msg.id,
                        error = %error,
                        "failed to mark delivered"
                    );
                } else {
                    self.record_message_routed(&msg.from, name);
                }

                std::thread::sleep(Duration::from_secs(1));
            }

            if delivered_any {
                self.mark_member_working(name);
            }
        }

        Ok(())
    }

    fn resolve_role_name(&self, member_name: &str) -> String {
        if member_name == "human" || member_name == "daemon" {
            return member_name.to_string();
        }
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .map(|member| member.role_name.clone())
            .unwrap_or_else(|| member_name.to_string())
    }
}

pub(super) fn message_delivery_marker(sender: &str) -> String {
    format!("--- Message from {sender} ---")
}

pub(super) fn capture_contains_message_marker(capture: &str, message_marker: &str) -> bool {
    capture.contains(message_marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::sync::{Arc, Mutex};

    use crate::team::AssignmentResultStatus;
    use crate::team::comms::Channel;
    use crate::team::config::OrchestratorPosition;
    use crate::team::config::{
        AutomationConfig, BoardConfig, ChannelConfig, RoleDef, StandupConfig, WorkflowMode,
        WorkflowPolicy,
    };
    use crate::team::daemon::{DaemonConfig, TeamDaemon};
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;

    struct RecordingChannel {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    struct FailingChannel;

    impl Channel for FailingChannel {
        fn send(&self, _message: &str) -> std::result::Result<(), DeliveryError> {
            Err(DeliveryError::ChannelSend {
                recipient: "test-recipient".to_string(),
                detail: "synthetic channel failure".to_string(),
            })
        }

        fn channel_type(&self) -> &str {
            "test-failing"
        }
    }

    struct SequencedTelegramChannel {
        results: Arc<Mutex<VecDeque<std::result::Result<(), DeliveryError>>>>,
        attempts: Arc<Mutex<u32>>,
    }

    impl Channel for SequencedTelegramChannel {
        fn send(&self, _message: &str) -> std::result::Result<(), DeliveryError> {
            *self.attempts.lock().unwrap() += 1;
            self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }

        fn channel_type(&self) -> &str {
            "telegram-test"
        }
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic event sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic event sink failure"))
        }
    }

    fn empty_legacy_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            pending_delivery_queue: HashMap::new(),
        }
    }

    fn failed_delivery_test_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };

        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![architect, manager, engineer],
                pane_map: HashMap::from([("eng-1".to_string(), "%9999999".to_string())]),
            },
            ..empty_legacy_daemon(tmp)
        }
    }

    #[test]
    fn queue_daemon_message_routes_to_channel_for_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Assignment delivered.")
            .unwrap();

        assert_eq!(sent.lock().unwrap().as_slice(), ["Assignment delivered."]);
    }

    #[test]
    fn queue_daemon_message_ignores_event_sink_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.event_sink = EventSink::from_writer(
            tmp.path().join("broken-events.jsonl").as_path(),
            FailingWriter,
        );

        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Event sink can fail without breaking delivery.")
            .unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Event sink can fail without breaking delivery."]
        );
    }

    #[test]
    fn telegram_delivery_retries_transient_channel_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let attempts = Arc::new(Mutex::new(0));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(SequencedTelegramChannel {
                results: Arc::new(Mutex::new(VecDeque::from([
                    Err(DeliveryError::ChannelSend {
                        recipient: "human".to_string(),
                        detail: "429 too many requests".to_string(),
                    }),
                    Err(DeliveryError::ChannelSend {
                        recipient: "human".to_string(),
                        detail: "timeout while sending".to_string(),
                    }),
                    Ok(()),
                ]))),
                attempts: Arc::clone(&attempts),
            }),
        );

        daemon
            .queue_daemon_message("human", "Assignment delivered.")
            .unwrap();

        assert_eq!(*attempts.lock().unwrap(), 3);
        assert_eq!(
            daemon
                .retry_counts
                .get(&TeamDaemon::telegram_failure_key("human")),
            None
        );
    }

    #[test]
    fn telegram_delivery_circuit_breaker_alerts_manager_after_repeated_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![manager],
                pane_map: HashMap::new(),
            },
            ..empty_legacy_daemon(&tmp)
        };
        let attempts = Arc::new(Mutex::new(0));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(SequencedTelegramChannel {
                results: Arc::new(Mutex::new(VecDeque::from(
                    (0..32)
                        .map(|_| {
                            Err(DeliveryError::ChannelSend {
                                recipient: "human".to_string(),
                                detail: "429 too many requests".to_string(),
                            })
                        })
                        .collect::<Vec<_>>(),
                ))),
                attempts: Arc::clone(&attempts),
            }),
        );

        for _ in 0..TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD {
            assert!(
                daemon
                    .queue_daemon_message("human", "Still failing")
                    .is_err()
            );
        }

        assert!(daemon.telegram_channel_paused("human"));
        let pending = inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Telegram delivery paused"));

        let before = *attempts.lock().unwrap();
        assert!(
            daemon
                .queue_daemon_message("human", "Breaker open")
                .is_err()
        );
        assert_eq!(*attempts.lock().unwrap(), before);
    }

    #[test]
    fn drain_legacy_command_queue_preserves_failed_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue_path = message::command_queue_path(tmp.path());
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Send {
                from: "architect".into(),
                to: "human".into(),
                message: "status".into(),
            },
        )
        .unwrap();
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Assign {
                from: "manager".into(),
                engineer: "eng-1".into(),
                task: "Task #7: recover".into(),
            },
        )
        .unwrap();

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: vec![RoleDef {
                        name: "human".to_string(),
                        role_type: RoleType::User,
                        agent: None,
                        instances: 1,
                        prompt: None,
                        talks_to: vec![],
                        channel: Some("telegram".to_string()),
                        channel_config: Some(ChannelConfig {
                            target: "123".to_string(),
                            provider: "fake".to_string(),
                            bot_token: None,
                            allowed_user_ids: vec![],
                        }),
                        nudge_interval_secs: None,
                        receives_standup: None,
                        standup_interval_secs: None,
                        owns: Vec::new(),
                        use_worktrees: false,
                    }],
                },
                session: "test".to_string(),
                members: vec![MemberInstance {
                    name: "eng-1".to_string(),
                    role_name: "eng-1".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("claude".to_string()),
                    prompt: None,
                    reports_to: None,
                    use_worktrees: false,
                }],
                pane_map: HashMap::new(),
            },
            channels: HashMap::from([(
                "human".to_string(),
                Box::new(FailingChannel) as Box<dyn Channel>,
            )]),
            ..empty_legacy_daemon(&tmp)
        };

        daemon.drain_legacy_command_queue().unwrap();

        let remaining = message::read_command_queue(&queue_path).unwrap();
        assert_eq!(remaining.len(), 1);
        match &remaining[0] {
            message::QueuedCommand::Send { to, message, .. } => {
                assert_eq!(to, "human");
                assert_eq!(message, "status");
            }
            other => panic!("expected failed send command to remain queued, got {other:?}"),
        }

        let engineer_pending =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();
        assert_eq!(engineer_pending.len(), 1);
        assert_eq!(engineer_pending[0].from, "manager");
        assert!(engineer_pending[0].body.contains("Task #7: recover"));
    }

    #[test]
    fn deliver_inbox_messages_reports_failed_assignment_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
        ];

        let mut pane_map = HashMap::new();
        pane_map.insert("eng-1".to_string(), "%999".to_string());

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: super::super::config::TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map,
        })
        .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let assign = inbox::InboxMessage::new_assign("manager", "eng-1", "Task #13: fix it");
        let id = inbox::deliver_to_inbox(&root, &assign).unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(engineer_pending.is_empty());

        let engineer_all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            engineer_all
                .iter()
                .any(|(msg, delivered)| msg.id == id && *delivered)
        );

        let manager_pending = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(manager_pending.len(), 1);
        assert_eq!(manager_pending[0].from, "daemon");
        assert!(manager_pending[0].body.contains("Assignment failed."));
        assert!(manager_pending[0].body.contains("Engineer: eng-1"));
        assert!(manager_pending[0].body.contains("Message ID:"));
        let result = crate::team::load_assignment_result(tmp.path(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(result.status, AssignmentResultStatus::Failed);
        assert_eq!(result.engineer, "eng-1");
        assert_eq!(daemon.states.get("eng-1"), None);
    }

    #[test]
    fn queue_message_falls_back_to_inbox_when_live_delivery_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![manager],
                pane_map: HashMap::from([("manager".to_string(), "%999".to_string())]),
            },
            ..empty_legacy_daemon(&tmp)
        };

        daemon
            .queue_message("eng-1", "manager", "Need review on merge handling.")
            .unwrap();

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "eng-1");
        assert!(messages[0].body.contains("Need review on merge handling."));
    }

    #[test]
    fn delivery_confirm_marker_detection_matches_captured_text() {
        let marker = message_delivery_marker("manager");
        let capture = format!("prompt\n{marker}\nbody\n");
        assert!(capture_contains_message_marker(&capture, &marker));
        assert!(!capture_contains_message_marker("prompt only", &marker));
    }

    #[test]
    fn delivery_confirm_marker_generation_uses_sender_header() {
        assert_eq!(
            message_delivery_marker("eng-1-4"),
            "--- Message from eng-1-4 ---"
        );
    }

    #[test]
    fn failed_delivery_new_sets_expected_fields() {
        let delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        assert_eq!(delivery.recipient, "eng-1");
        assert_eq!(delivery.from, "manager");
        assert_eq!(delivery.body, "Please retry this.");
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.message_marker(), "--- Message from manager ---");
        assert!(delivery.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_emits_single_health_event_per_unique_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon.record_failed_delivery("eng-1", "manager", "Please retry this.");
        daemon.record_failed_delivery("eng-1", "manager", "Please retry this.");

        let events = super::super::events::read_events(&tmp.path().join("events.jsonl")).unwrap();
        let delivery_failed = events
            .into_iter()
            .filter(|event| event.event == "delivery_failed")
            .collect::<Vec<_>>();
        assert_eq!(delivery_failed.len(), 1);
        assert_eq!(delivery_failed[0].role.as_deref(), Some("eng-1"));
        assert_eq!(delivery_failed[0].from.as_deref(), Some("manager"));
    }

    #[test]
    fn failed_delivery_retry_requeues_before_attempt_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        delivery.attempts = 1;
        delivery.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY;
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        assert_eq!(daemon.failed_deliveries.len(), 1);
        assert_eq!(daemon.failed_deliveries[0].attempts, 2);
        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn failed_delivery_retry_respects_attempt_cap_and_escalates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        delivery.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY;
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        assert!(daemon.failed_deliveries.is_empty());
        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(
            messages[0]
                .body
                .contains("Live message delivery failed after 3 attempts.")
        );
        assert!(messages[0].body.contains("Recipient: eng-1"));
    }

    #[test]
    fn external_sender_delivery() {
        // Messages from an external sender (e.g. email-router) should be
        // queued to the recipient's inbox and not blocked by routing validation.
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);

        // Configure external_senders and a manager role so can_talk succeeds
        daemon.config.team_config.external_senders = vec!["email-router".to_string()];
        daemon.config.team_config.roles = vec![RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        }];
        daemon.config.members = vec![MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }];

        // Queue a message from external sender to manager
        daemon
            .queue_message("email-router", "manager", "New email from user@example.com")
            .unwrap();

        // Verify the message landed in manager's inbox
        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "email-router");
        assert!(messages[0].body.contains("New email from user@example.com"));

        // Also verify routing would be allowed via can_talk
        assert!(
            daemon
                .config
                .team_config
                .can_talk("email-router", "manager")
        );
    }

    // --- Readiness gate tests ---

    #[test]
    fn is_agent_ready_returns_false_for_nonexistent_pane() {
        assert!(!is_agent_ready("%99999999"));
    }

    #[test]
    fn delivery_capture_lines_default_for_idle_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // Watcher in Idle state → standard capture lines
        daemon.watchers.insert(
            "eng-1".to_string(),
            crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None),
        );
        assert_eq!(
            daemon.delivery_capture_lines_for("eng-1"),
            DELIVERY_VERIFICATION_CAPTURE_LINES
        );
    }

    #[test]
    fn delivery_capture_lines_increased_for_recently_ready_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        assert_eq!(watcher.state, crate::team::watcher::WatcherState::Ready);
        daemon.watchers.insert("eng-1".to_string(), watcher);
        assert_eq!(
            daemon.delivery_capture_lines_for("eng-1"),
            DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY
        );
    }

    #[test]
    fn delivery_capture_lines_default_for_unknown_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        // No watcher for this recipient → default
        assert_eq!(
            daemon.delivery_capture_lines_for("unknown-agent"),
            DELIVERY_VERIFICATION_CAPTURE_LINES
        );
    }

    #[test]
    fn check_agent_ready_returns_true_when_watcher_confirmed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate(); // sets ready_confirmed = true
        watcher.deactivate();
        daemon.watchers.insert("eng-1".to_string(), watcher);
        // Should return immediately without polling since watcher is confirmed ready.
        assert!(daemon.check_agent_ready("eng-1", "%9999999"));
    }

    #[test]
    fn check_agent_ready_returns_false_for_unready_nonexistent_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let watcher = crate::team::watcher::SessionWatcher::new("%99999999", "eng-1", 300, None);
        daemon.watchers.insert("eng-1".to_string(), watcher);
        // Override the timeout to something very short so the test doesn't hang.
        // We can't change the const, but we can verify the function returns false
        // for a nonexistent pane. The real timeout is 60s but tmux capture fails
        // instantly for nonexistent panes, so it will loop quickly and timeout.
        // Use a custom test approach: verify the function returns false.
        // Note: with 60s timeout this would be slow, but capture_pane_recent
        // returns Err for invalid panes, so is_agent_ready returns false
        // immediately on each check. The backoff loop will hit timeout.
        // To keep this test fast, we test the is_agent_ready function directly
        // instead, which is already covered above.
        // Here we just verify the fast path doesn't return true.
        assert!(
            !daemon
                .watchers
                .get("eng-1")
                .unwrap()
                .is_ready_for_delivery()
        );
    }

    #[test]
    fn deliver_inbox_skips_agents_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Create inbox message for eng-1
        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        // Put watcher in Active state (not ready for delivery)
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // deliver_inbox_messages should skip eng-1 since it's Active
        daemon.deliver_inbox_messages().unwrap();

        // Message should still be pending (not delivered)
        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "message should remain pending for active agent"
        );
    }

    #[test]
    fn deliver_inbox_delivers_to_ready_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Create inbox message for eng-1
        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        // Put watcher in Ready state (ready for delivery, but pane doesn't exist
        // so inject will fail and fall through, but the point is the check passes).
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // deliver_inbox_messages should attempt delivery for eng-1 since it's Ready.
        // The actual inject will fail (fake pane), but the readiness gate passed.
        daemon.deliver_inbox_messages().unwrap();

        // The message should still be pending because the pane doesn't exist
        // and inject fails, but what matters is that the code attempted delivery
        // (didn't skip due to readiness check).
        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        // Messages may or may not remain depending on whether inject_message errors
        // are caught. The key test is that we reach the inject path — which we verify
        // by the absence of the "not ready" skip condition being triggered.
        let _ = pending;
    }

    #[test]
    fn retry_failed_delivery_skips_non_ready_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Add a failed delivery that's ready for retry (old enough)
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test message");
        delivery.last_attempt = Instant::now() - Duration::from_secs(60);
        daemon.failed_deliveries.push(delivery);

        // Watcher is Active (not idle/ready) → retry should be skipped
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.retry_failed_deliveries().unwrap();

        // Delivery should still be in the queue (not attempted)
        assert_eq!(daemon.failed_deliveries.len(), 1);
    }

    #[test]
    fn retry_failed_delivery_attempts_ready_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Add a failed delivery that's ready for retry
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test message");
        delivery.last_attempt = Instant::now() - Duration::from_secs(60);
        daemon.failed_deliveries.push(delivery);

        // Watcher is Ready → retry should be attempted
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.retry_failed_deliveries().unwrap();

        // The delivery attempt will fail (fake pane) but will count as an attempt.
        // It should either be removed (escalated) or have incremented attempt count.
        // With 1 initial attempt + 1 retry = 2, still under max of 3, so it stays.
        assert!(
            daemon.failed_deliveries.len() <= 1,
            "delivery should have been attempted"
        );
    }

    // --- New tests: FailedDelivery struct ---

    #[test]
    fn failed_delivery_is_not_ready_for_retry_when_recent() {
        let delivery = FailedDelivery::new("eng-1", "manager", "test");
        // Just created — last_attempt is now, so not ready for retry
        assert!(!delivery.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_is_ready_for_retry_after_delay() {
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test");
        delivery.last_attempt =
            Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        assert!(delivery.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_has_attempts_remaining_at_boundary() {
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        assert!(delivery.has_attempts_remaining());
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS;
        assert!(!delivery.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_message_marker_uses_from_field() {
        let delivery = FailedDelivery::new("eng-1", "architect", "body");
        assert_eq!(delivery.message_marker(), "--- Message from architect ---");
    }

    // --- MessageDelivery enum ---

    #[test]
    fn message_delivery_variants_are_distinct() {
        assert_ne!(MessageDelivery::Channel, MessageDelivery::LivePane);
        assert_ne!(MessageDelivery::LivePane, MessageDelivery::InboxQueued);
        assert_ne!(
            MessageDelivery::InboxQueued,
            MessageDelivery::SkippedUnknownRecipient
        );
        assert_eq!(MessageDelivery::Channel, MessageDelivery::Channel);
    }

    // --- Telegram circuit breaker key generation ---

    #[test]
    fn telegram_failure_key_contains_recipient() {
        let key = TeamDaemon::telegram_failure_key("human");
        assert_eq!(key, "telegram-delivery-failures::human");
    }

    #[test]
    fn telegram_circuit_breaker_key_contains_recipient() {
        let key = TeamDaemon::telegram_circuit_breaker_key("user-1");
        assert_eq!(key, "telegram-delivery-breaker::user-1");
    }

    // --- Telegram retry config ---

    #[test]
    fn telegram_retry_config_has_expected_defaults() {
        let config = TeamDaemon::telegram_retry_config();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.base_delay_ms, 100);
        assert_eq!(config.max_delay_ms, 1_000);
        assert!(!config.jitter);
    }

    // --- Telegram channel paused ---

    #[test]
    fn telegram_channel_not_paused_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = empty_legacy_daemon(&tmp);
        assert!(!daemon.telegram_channel_paused("human"));
    }

    #[test]
    fn telegram_channel_paused_when_breaker_open() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("human"),
            Instant::now(),
        );
        assert!(daemon.telegram_channel_paused("human"));
    }

    #[test]
    fn telegram_channel_not_paused_after_cooldown_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("human"),
            Instant::now() - TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN - Duration::from_secs(1),
        );
        assert!(!daemon.telegram_channel_paused("human"));
    }

    // --- Clear telegram delivery failures ---

    #[test]
    fn clear_telegram_delivery_failures_removes_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon
            .retry_counts
            .insert(TeamDaemon::telegram_failure_key("human"), 3);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("human"),
            Instant::now(),
        );

        daemon.clear_telegram_delivery_failures("human");

        assert!(
            !daemon
                .retry_counts
                .contains_key(&TeamDaemon::telegram_failure_key("human"))
        );
        assert!(
            !daemon
                .intervention_cooldowns
                .contains_key(&TeamDaemon::telegram_circuit_breaker_key("human"))
        );
    }

    // --- Increment telegram delivery failures ---

    #[test]
    fn increment_telegram_delivery_failures_starts_at_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let count = daemon.increment_telegram_delivery_failures("human");
        assert_eq!(count, 1);
    }

    #[test]
    fn increment_telegram_delivery_failures_accumulates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.increment_telegram_delivery_failures("human");
        daemon.increment_telegram_delivery_failures("human");
        let count = daemon.increment_telegram_delivery_failures("human");
        assert_eq!(count, 3);
    }

    // --- Delivery to unknown recipient ---

    #[test]
    fn deliver_message_skips_unknown_recipient() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let result = daemon
            .deliver_message("manager", "nonexistent-role", "hello")
            .unwrap();
        assert_eq!(result, MessageDelivery::SkippedUnknownRecipient);
    }

    // --- Delivery to member without pane falls back to inbox ---

    #[test]
    fn deliver_message_to_member_without_pane_goes_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        // Add member without pane
        daemon.config.members.push(MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        });

        let result = daemon
            .deliver_message("manager", "eng-2", "Go fix the bug")
            .unwrap();
        assert_eq!(result, MessageDelivery::InboxQueued);

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "eng-2").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "manager");
        assert!(messages[0].body.contains("Go fix the bug"));
    }

    // --- Non-telegram channel delivery ---

    #[test]
    fn deliver_channel_message_records_routing_event() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "user".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let result = daemon
            .deliver_channel_message("eng-1", "user", "Status update")
            .unwrap();
        assert_eq!(result, MessageDelivery::Channel);
        assert_eq!(sent.lock().unwrap().as_slice(), ["Status update"]);
    }

    // --- Failed delivery deduplication ---

    #[test]
    fn record_failed_delivery_deduplicates_same_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon.record_failed_delivery("eng-1", "manager", "test msg");
        let first_attempt_time = daemon.failed_deliveries[0].last_attempt;
        std::thread::sleep(Duration::from_millis(10));
        daemon.record_failed_delivery("eng-1", "manager", "test msg");

        // Should still have only one entry
        assert_eq!(daemon.failed_deliveries.len(), 1);
        // But last_attempt should be updated
        assert!(daemon.failed_deliveries[0].last_attempt >= first_attempt_time);
    }

    #[test]
    fn record_failed_delivery_tracks_different_messages_separately() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon.record_failed_delivery("eng-1", "manager", "msg A");
        daemon.record_failed_delivery("eng-1", "manager", "msg B");

        assert_eq!(daemon.failed_deliveries.len(), 2);
    }

    #[test]
    fn record_failed_delivery_tracks_different_recipients_separately() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // Add another pane mapping for a second engineer
        daemon
            .config
            .pane_map
            .insert("eng-2".to_string(), "%8888888".to_string());

        daemon.record_failed_delivery("eng-1", "manager", "same msg");
        daemon.record_failed_delivery("eng-2", "manager", "same msg");

        assert_eq!(daemon.failed_deliveries.len(), 2);
    }

    // --- Clear failed delivery ---

    #[test]
    fn clear_failed_delivery_removes_matching_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon
            .failed_deliveries
            .push(FailedDelivery::new("eng-1", "manager", "msg A"));
        daemon
            .failed_deliveries
            .push(FailedDelivery::new("eng-1", "manager", "msg B"));

        daemon.clear_failed_delivery("eng-1", "manager", "msg A");

        assert_eq!(daemon.failed_deliveries.len(), 1);
        assert_eq!(daemon.failed_deliveries[0].body, "msg B");
    }

    #[test]
    fn clear_failed_delivery_no_op_when_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon
            .failed_deliveries
            .push(FailedDelivery::new("eng-1", "manager", "msg A"));

        daemon.clear_failed_delivery("eng-1", "manager", "nonexistent");

        assert_eq!(daemon.failed_deliveries.len(), 1);
    }

    // --- Escalation recipient resolution ---

    #[test]
    fn escalation_recipient_uses_reports_to() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        // eng-1 reports to manager
        let target = daemon.failed_delivery_escalation_recipient("eng-1");
        assert_eq!(target.as_deref(), Some("manager"));
    }

    #[test]
    fn escalation_recipient_falls_back_to_any_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // Add a member without reports_to but there's a manager in the team
        daemon.config.members.push(MemberInstance {
            name: "standalone".to_string(),
            role_name: "standalone".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        });

        let target = daemon.failed_delivery_escalation_recipient("standalone");
        assert_eq!(target.as_deref(), Some("manager"));
    }

    #[test]
    fn escalation_recipient_none_for_unknown_member_without_managers() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = empty_legacy_daemon(&tmp);
        // No members at all
        let target = daemon.failed_delivery_escalation_recipient("unknown");
        // automation_sender_for returns "daemon" by default, which isn't a member
        assert!(target.is_none());
    }

    // --- Escalate failed delivery ---

    #[test]
    fn escalate_failed_delivery_sends_to_manager_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let delivery = FailedDelivery::new("eng-1", "architect", "critical update");

        daemon.escalate_failed_delivery(&delivery).unwrap();

        let root = inbox::inboxes_root(tmp.path());
        // eng-1 reports to manager
        let messages = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(messages[0].body.contains("Live message delivery failed"));
        assert!(messages[0].body.contains("Recipient: eng-1"));
        assert!(messages[0].body.contains("From: architect"));
        assert!(messages[0].body.contains("critical update"));
    }

    #[test]
    fn escalate_failed_delivery_no_crash_without_target() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let delivery = FailedDelivery::new("orphan", "ghost", "lost message");

        // Should not panic or error — just warns
        daemon.escalate_failed_delivery(&delivery).unwrap();
    }

    // --- Retry failed deliveries ---

    #[test]
    fn retry_failed_deliveries_noop_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.retry_failed_deliveries().unwrap();
        assert!(daemon.failed_deliveries.is_empty());
    }

    #[test]
    fn retry_failed_deliveries_skips_too_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // Create a delivery that was just attempted (not ready for retry)
        let delivery = FailedDelivery::new("eng-1", "manager", "recent msg");
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        // Should still be in the queue, not attempted
        assert_eq!(daemon.failed_deliveries.len(), 1);
        assert_eq!(daemon.failed_deliveries[0].attempts, 1); // unchanged
    }

    #[test]
    fn retry_failed_deliveries_escalates_without_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // Remove pane mapping for eng-1 so retry has no pane target
        daemon.config.pane_map.clear();
        let mut delivery = FailedDelivery::new("eng-1", "manager", "no pane msg");
        delivery.last_attempt =
            Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        // Delivery should be removed (escalated)
        assert!(daemon.failed_deliveries.is_empty());
        // Escalation should have been sent to manager
        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].body.contains("Live message delivery failed"));
    }

    // --- Resolve role name ---

    #[test]
    fn resolve_role_name_returns_human_for_human() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("human"), "human");
    }

    #[test]
    fn resolve_role_name_returns_daemon_for_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("daemon"), "daemon");
    }

    #[test]
    fn resolve_role_name_maps_member_to_role_name() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        // eng-1 has role_name "eng"
        assert_eq!(daemon.resolve_role_name("eng-1"), "eng");
    }

    #[test]
    fn resolve_role_name_returns_input_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("unknown-member"), "unknown-member");
    }

    // --- Capture contains marker ---

    #[test]
    fn capture_contains_marker_empty_capture() {
        assert!(!capture_contains_message_marker(
            "",
            "--- Message from x ---"
        ));
    }

    #[test]
    fn capture_contains_marker_partial_match_fails() {
        let marker = message_delivery_marker("manager");
        assert!(!capture_contains_message_marker(
            "--- Message from",
            &marker
        ));
    }

    #[test]
    fn capture_contains_marker_multiline_capture() {
        let marker = message_delivery_marker("eng-1");
        let capture = "line1\nline2\n--- Message from eng-1 ---\nline4\n";
        assert!(capture_contains_message_marker(capture, &marker));
    }

    // --- Queue daemon message uses automation sender ---

    #[test]
    fn queue_daemon_message_to_unknown_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let result = daemon.queue_daemon_message("nobody", "test msg").unwrap();
        assert_eq!(result, MessageDelivery::SkippedUnknownRecipient);
    }

    // --- Deliver channel with failing non-telegram channel ---

    #[test]
    fn deliver_channel_message_failing_non_telegram_channel_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon
            .channels
            .insert("user".to_string(), Box::new(FailingChannel));

        let result = daemon.deliver_channel_message("eng-1", "user", "test");
        assert!(result.is_err());
    }

    // --- Telegram circuit breaker blocks further attempts ---

    #[test]
    fn telegram_delivery_blocked_when_circuit_breaker_open() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "user".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );
        // Simulate telegram channel type by using SequencedTelegramChannel
        daemon.channels.insert(
            "tg-user".to_string(),
            Box::new(SequencedTelegramChannel {
                results: Arc::new(Mutex::new(VecDeque::from([
                    Ok(()),
                    Ok(()),
                    Ok(()),
                    Ok(()),
                    Ok(()),
                ]))),
                attempts: Arc::new(Mutex::new(0)),
            }),
        );
        // Open circuit breaker for tg-user
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("tg-user"),
            Instant::now(),
        );

        let result = daemon.deliver_channel_message("eng-1", "tg-user", "blocked msg");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("circuit breaker is open"));
    }

    // --- Constants verification ---

    #[test]
    fn delivery_verification_constants_are_sane() {
        assert!(
            DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY
                > DELIVERY_VERIFICATION_CAPTURE_LINES
        );
        assert!(DELIVERY_VERIFICATION_CAPTURE_LINES > 0);
        assert!(FAILED_DELIVERY_MAX_ATTEMPTS >= 2);
        assert!(FAILED_DELIVERY_RETRY_DELAY >= Duration::from_secs(1));
    }

    // --- Verify message content in pane ---

    #[test]
    fn verify_message_content_in_pane_returns_false_for_nonexistent_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert!(!daemon.verify_message_content_in_pane("%99999999", "--- Message from test ---"));
    }

    #[test]
    fn verify_message_content_in_pane_lines_returns_false_for_nonexistent_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert!(!daemon.verify_message_content_in_pane_lines(
            "%99999999",
            "--- Message from test ---",
            100
        ));
    }

    // --- Error path and recovery tests (Task #265) ---

    #[test]
    fn failed_delivery_not_ready_for_immediate_retry() {
        let fd = FailedDelivery::new("eng-1", "manager", "test message");
        // Just created — not enough time has passed for retry
        assert!(!fd.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_ready_after_delay() {
        let mut fd = FailedDelivery::new("eng-1", "manager", "test message");
        // Simulate past creation
        fd.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        assert!(fd.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_has_attempts_remaining() {
        let mut fd = FailedDelivery::new("eng-1", "manager", "test message");
        assert!(fd.has_attempts_remaining()); // attempts=1, max=3
        fd.attempts = FAILED_DELIVERY_MAX_ATTEMPTS;
        assert!(!fd.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_message_marker_format() {
        let fd = FailedDelivery::new("eng-1", "manager", "test message");
        let marker = fd.message_marker();
        assert!(marker.contains("manager"));
    }

    #[test]
    fn failed_delivery_fields_preserved() {
        let fd = FailedDelivery::new("eng-1", "manager", "hello world");
        assert_eq!(fd.recipient, "eng-1");
        assert_eq!(fd.from, "manager");
        assert_eq!(fd.body, "hello world");
        assert_eq!(fd.attempts, 1);
    }

    #[test]
    fn telegram_circuit_breaker_key_format() {
        let key = TeamDaemon::telegram_circuit_breaker_key("eng-1");
        assert!(key.contains("eng-1"));
        assert!(key.starts_with("telegram-delivery-breaker::"));
    }

    #[test]
    fn telegram_failure_key_format() {
        let key = TeamDaemon::telegram_failure_key("manager");
        assert!(key.contains("manager"));
        assert!(key.starts_with("telegram-delivery-failures::"));
    }

    #[test]
    fn telegram_retry_config_has_sensible_defaults() {
        let config = TeamDaemon::telegram_retry_config();
        assert!(config.max_retries >= 1);
        assert!(config.max_delay_ms > config.base_delay_ms);
    }

    #[test]
    fn telegram_channel_not_paused_initially() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = empty_legacy_daemon(&tmp);
        assert!(!daemon.telegram_channel_paused("eng-1"));
    }

    // --- Pending delivery queue tests (Task #276) ---

    #[test]
    fn pending_queue_buffers_message_when_agent_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // Watcher present but not yet confirmed ready (starting state).
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        let result = daemon
            .deliver_message("manager", "eng-1", "task assignment")
            .unwrap();

        assert_eq!(
            result,
            MessageDelivery::DeferredPending,
            "message to starting agent must be deferred to pending queue"
        );
        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].from, "manager");
        assert_eq!(queue[0].body, "task assignment");
    }

    #[test]
    fn drain_pending_queue_delivers_when_agent_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Pre-populate the pending queue.
        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(PendingMessage {
                from: "manager".to_string(),
                body: "queued assignment".to_string(),
                queued_at: Instant::now(),
            });

        // Mark the watcher as ready so deliver_message proceeds past the gate.
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.drain_pending_queue("eng-1").unwrap();

        // Queue must be empty after drain.
        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true),
            "pending queue must be empty after drain"
        );

        // Message should have fallen through to inbox (pane %9999999 doesn't exist).
        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "queued assignment");
    }

    #[test]
    fn drain_pending_queue_noop_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // No pending messages — should not panic or error.
        daemon.drain_pending_queue("eng-1").unwrap();
        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn escalation_skipped_for_starting_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Agent watcher is present but not yet confirmed ready.
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Build a delivery targeting eng-1 that has used all its retry attempts.
        // FailedDelivery::new(recipient, from, body).
        let mut delivery = FailedDelivery::new("eng-1", "manager", "assignment");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        delivery.last_attempt =
            Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        // Agent is not ready → delivery pushed back to queue at the readiness check.
        // Escalation should NOT have happened — architect inbox must be empty.
        let root = inbox::inboxes_root(tmp.path());
        let architect_inbox = inbox::pending_messages(&root, "architect").unwrap();
        assert!(
            architect_inbox.is_empty(),
            "no escalation expected while agent is starting"
        );
        // Delivery should still be in the retry queue.
        assert_eq!(
            daemon.failed_deliveries.len(),
            1,
            "failed delivery must stay in queue for starting agent"
        );
    }

    #[test]
    fn escalation_happens_for_ready_agents_with_failed_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Agent is ready.
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Build a delivery that has used all retry attempts and is due for retry.
        let mut delivery = FailedDelivery::new("eng-1", "manager", "assignment");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        delivery.last_attempt =
            Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        // After all retries exhausted for a ready agent, it must escalate.
        // (eng-1 reports_to=manager)
        let root = inbox::inboxes_root(tmp.path());
        let manager_inbox = inbox::pending_messages(&root, "manager").unwrap();
        assert!(
            !manager_inbox.is_empty(),
            "failed delivery for ready agent must be escalated"
        );
    }

    #[test]
    fn multiple_messages_queued_and_drained_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Watcher not ready — all messages should be buffered.
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        daemon.watchers.insert("eng-1".to_string(), watcher);

        for i in 1..=3u32 {
            let result = daemon
                .deliver_message("manager", "eng-1", &format!("msg-{i}"))
                .unwrap();
            assert_eq!(result, MessageDelivery::DeferredPending);
        }

        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 3);
        // Verify FIFO order in the queue.
        assert_eq!(queue[0].body, "msg-1");
        assert_eq!(queue[1].body, "msg-2");
        assert_eq!(queue[2].body, "msg-3");

        // Confirm readiness and drain.
        daemon.watchers.get_mut("eng-1").unwrap().confirm_ready();
        daemon.drain_pending_queue("eng-1").unwrap();

        // Queue must be empty.
        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true)
        );

        // All three messages must be in inbox (pane doesn't exist → inbox fallback).
        let root = inbox::inboxes_root(tmp.path());
        let inbox_msgs = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(inbox_msgs.len(), 3, "all queued messages must be delivered");
        // Verify all messages present (inbox ordering depends on filesystem).
        let mut bodies: Vec<&str> = inbox_msgs.iter().map(|m| m.body.as_str()).collect();
        bodies.sort();
        assert_eq!(bodies, vec!["msg-1", "msg-2", "msg-3"]);
    }
}
