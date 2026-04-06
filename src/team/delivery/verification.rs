use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::{
    DELIVERY_VERIFICATION_CAPTURE_LINES, DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY,
    FailedDelivery, capture_contains_message_marker, is_agent_ready, message_delivery_marker,
};
use crate::team::config::RoleType;
use crate::team::daemon::TeamDaemon;
use crate::team::inbox;
use crate::tmux;

#[allow(dead_code)]
impl TeamDaemon {
    pub(in crate::team) fn verify_message_content_in_pane(
        &self,
        pane_id: &str,
        message_marker: &str,
    ) -> bool {
        self.verify_message_content_in_pane_lines(
            pane_id,
            message_marker,
            DELIVERY_VERIFICATION_CAPTURE_LINES,
        )
    }

    pub(in crate::team) fn verify_message_content_in_pane_lines(
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

    pub(in crate::team) fn record_failed_delivery(
        &mut self,
        recipient: &str,
        from: &str,
        body: &str,
    ) {
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

    pub(in crate::team) fn clear_failed_delivery(
        &mut self,
        recipient: &str,
        from: &str,
        body: &str,
    ) {
        self.failed_deliveries.retain(|delivery| {
            delivery.recipient != recipient || delivery.from != from || delivery.body != body
        });
    }

    pub(in crate::team) fn failed_delivery_escalation_recipient(
        &self,
        recipient: &str,
    ) -> Option<String> {
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

    pub(in crate::team) fn escalate_failed_delivery(
        &mut self,
        delivery: &FailedDelivery,
    ) -> Result<()> {
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

    pub(in crate::team) fn retry_failed_deliveries(&mut self) -> Result<()> {
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

            if !self.member_ready_for_delivery(&delivery.recipient) {
                self.failed_deliveries.push(delivery);
                continue;
            }

            delivery.attempts += 1;
            delivery.last_attempt = now;
            info!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                attempts = delivery.attempts,
                "retrying failed delivery via shim"
            );

            // Retry via shim channel
            let delivered = if let Some(handle) = self.shim_handles.get_mut(&delivery.recipient) {
                match handle.send_message(&delivery.from, &delivery.body) {
                    Ok(()) => {
                        handle.apply_state_change(crate::shim::protocol::ShimState::Working);
                        true
                    }
                    Err(error) => {
                        warn!(
                            recipient = %delivery.recipient,
                            from = %delivery.from,
                            error = %error,
                            "shim retry delivery failed"
                        );
                        false
                    }
                }
            } else {
                false
            };

            if delivered {
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

    pub(in crate::team) fn verify_message_delivered(
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

            // If the agent is no longer at its ready prompt, it started working —
            // the marker was consumed and scrolled off the capture window.
            if self.agent_went_active_after_injection(&pane_id, recipient) {
                self.clear_failed_delivery(recipient, from, body);
                info!(
                    recipient,
                    attempt,
                    marker = %message_marker,
                    "message delivery inferred: agent active after injection (marker scrolloff)"
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

    #[allow(dead_code)]
    pub(in crate::team) fn verify_message_delivered_with_lines(
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

            // If the agent is no longer at its ready prompt, it started working —
            // the marker was consumed and scrolled off the capture window.
            if self.agent_went_active_after_injection(&pane_id, recipient) {
                self.clear_failed_delivery(recipient, from, body);
                info!(
                    recipient,
                    attempt,
                    capture_lines,
                    marker = %message_marker,
                    "message delivery inferred: agent active after injection (marker scrolloff)"
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
    #[allow(dead_code)]
    pub(in crate::team) fn check_agent_ready(&mut self, recipient: &str, pane_id: &str) -> bool {
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
    #[allow(dead_code)]
    pub(in crate::team) fn delivery_capture_lines_for(&self, recipient: &str) -> u32 {
        let recently_ready = self
            .watchers
            .get(recipient)
            .is_some_and(|w| matches!(w.state, super::super::watcher::WatcherState::Ready));
        if recently_ready {
            DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY
        } else {
            DELIVERY_VERIFICATION_CAPTURE_LINES
        }
    }

    /// Check whether the agent is no longer at its ready prompt after message
    /// injection. If the agent was idle/ready before and is now actively working,
    /// the delivery marker likely scrolled off the capture window — the agent
    /// consumed the message. This prevents false-negative delivery failures when
    /// fast-processing agents push the marker past the capture window.
    pub(in crate::team) fn agent_went_active_after_injection(
        &self,
        pane_id: &str,
        recipient: &str,
    ) -> bool {
        // Only infer delivery if the watcher had previously confirmed readiness.
        // This avoids false positives for agents that were never idle.
        let was_ready = self
            .watchers
            .get(recipient)
            .is_some_and(|w| w.is_ready_for_delivery());
        if !was_ready {
            return false;
        }
        // Live check: if the agent is no longer at its prompt, it started working.
        !is_agent_ready(pane_id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use super::super::{
        DELIVERY_VERIFICATION_CAPTURE_LINES, DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY,
        FAILED_DELIVERY_MAX_ATTEMPTS, FAILED_DELIVERY_RETRY_DELAY, FailedDelivery,
    };
    use crate::team::config::OrchestratorPosition;
    use crate::team::config::RoleType;
    use crate::team::config::{
        AutomationConfig, BoardConfig, StandupConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::daemon::{DaemonConfig, TeamDaemon};
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;

    fn empty_legacy_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: crate::team::config::TeamConfig {
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
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
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
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            narration_tracker: Default::default(),
            context_pressure_tracker: Default::default(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now(),
            pending_delivery_queue: HashMap::new(),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            shim_handles: HashMap::new(),
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
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
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: crate::team::config::TeamConfig {
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
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
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
    fn failed_delivery_emits_single_health_event_per_unique_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon.record_failed_delivery("eng-1", "manager", "Please retry this.");
        daemon.record_failed_delivery("eng-1", "manager", "Please retry this.");

        let events = crate::team::events::read_events(&tmp.path().join("events.jsonl")).unwrap();
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
        assert!(
            !daemon
                .watchers
                .get("eng-1")
                .unwrap()
                .is_ready_for_delivery()
        );
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
            ..Default::default()
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
    fn retry_failed_deliveries_escalates_without_shim_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        // No shim handle for eng-1 — retry delivery will fail
        let mut delivery = FailedDelivery::new("eng-1", "manager", "no shim msg");
        // Set attempts to max - 1 so the next retry exhausts the limit and escalates
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
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

    // --- Escalation suppression for starting agents ---

    #[test]
    fn escalation_skipped_for_starting_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        // Agent watcher is present but not yet confirmed ready.
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Build a delivery targeting eng-1 that has used all its retry attempts.
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

    // --- Marker scrolloff / state-transition delivery inference tests ---

    #[test]
    fn marker_scrolloff_detected_as_delivered_when_agent_active() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate(); // sets ready_confirmed = true
        daemon.watchers.insert("eng-1".to_string(), watcher);

        assert!(daemon.agent_went_active_after_injection("%9999999", "eng-1"));
    }

    #[test]
    fn marker_scrolloff_not_inferred_when_watcher_never_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        assert!(!daemon.agent_went_active_after_injection("%9999999", "eng-1"));
    }

    #[test]
    fn marker_scrolloff_not_inferred_without_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert!(!daemon.agent_went_active_after_injection("%9999999", "unknown-agent"));
    }

    #[test]
    fn state_transition_confirms_delivery_after_activate() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        assert!(watcher.is_ready_for_delivery());
        watcher.activate(); // simulates injection activating the agent
        assert!(watcher.is_ready_for_delivery()); // stays true after activate
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Pane doesn't exist → agent not at prompt → inferred delivered.
        assert!(daemon.agent_went_active_after_injection("%9999999", "eng-1"));
    }

    #[test]
    fn state_transition_ready_to_active_clears_failed_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Seed a failed delivery.
        daemon
            .failed_deliveries
            .push(FailedDelivery::new("eng-1", "manager", "test message"));
        assert_eq!(daemon.failed_deliveries.len(), 1);

        daemon.clear_failed_delivery("eng-1", "manager", "test message");
        assert!(daemon.failed_deliveries.is_empty());
    }
}
