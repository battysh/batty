//! Dispatch guard: escalation routing and failure handling for dispatch
//! queue entries that exhaust their validation retry budget.

use anyhow::Result;
use tracing::warn;

use super::super::*;
use super::DispatchQueueEntry;

impl TeamDaemon {
    pub(in super::super) fn dispatch_failure_recipient(&self, engineer: &str) -> Option<String> {
        self.manager_name(engineer).or_else(|| {
            self.config
                .members
                .iter()
                .find(|member| member.role_type == RoleType::Manager)
                .map(|member| member.name.clone())
        })
    }

    pub(in super::super) fn escalate_dispatch_queue_entry(
        &mut self,
        entry: &DispatchQueueEntry,
        detail: &str,
    ) -> Result<()> {
        let Some(manager) = self.dispatch_failure_recipient(&entry.engineer) else {
            warn!(
                engineer = %entry.engineer,
                task_id = entry.task_id,
                detail,
                "dispatch queue entry exhausted retries without escalation target"
            );
            return Ok(());
        };

        // Suppress repeated escalations for the same task+engineer pair.
        // Without this, auto-dispatch re-queues the entry with fresh
        // validation_failures on every poll cycle, creating an infinite
        // escalation→drop→re-queue→escalation message flood.
        let escalation_key = format!("dispatch-fail:{}:{}", entry.task_id, entry.engineer);
        let cooldown = std::time::Duration::from_secs(900); // 15 minutes
        if self.suppress_recent_escalation(escalation_key, cooldown) {
            // Still insert into recent_dispatches to prevent re-enqueueing
            self.recent_dispatches
                .insert((entry.task_id, entry.engineer.clone()), std::time::Instant::now());
            return Ok(());
        }

        let body = format!(
            "Dispatch queue entry failed validation too many times.\nEngineer: {}\nTask #{}: {}\nFailures: {}\nLast failure: {}",
            entry.engineer, entry.task_id, entry.task_title, entry.validation_failures, detail
        );
        self.queue_daemon_message(&manager, &body)?;

        // Record in recent_dispatches so enqueue_dispatch_candidates won't
        // immediately re-queue this same task+engineer pair on the next poll
        // cycle. The dedup window (default 60s) is short, but the escalation
        // suppression cooldown (900s) prevents message floods even if the
        // entry does get re-enqueued after the dedup window expires.
        self.recent_dispatches
            .insert((entry.task_id, entry.engineer.clone()), std::time::Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::team::inbox;
    use crate::team::test_support::{TestDaemonBuilder, engineer_member, manager_member};

    use super::DispatchQueueEntry;

    #[test]
    fn failure_recipient_returns_reports_to_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr-a", None),
                manager_member("mgr-b", None),
                engineer_member("eng-1", Some("mgr-b"), false),
            ])
            .build();

        assert_eq!(
            daemon.dispatch_failure_recipient("eng-1"),
            Some("mgr-b".to_string()),
            "should return the engineer's reports_to manager"
        );
    }

    #[test]
    fn failure_recipient_falls_back_to_first_manager() {
        let tmp = tempfile::tempdir().unwrap();
        // Engineer with no reports_to — should fall back to any manager
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr-a", None),
                engineer_member("eng-1", None, false),
            ])
            .build();

        assert_eq!(
            daemon.dispatch_failure_recipient("eng-1"),
            Some("mgr-a".to_string()),
            "should fall back to first manager when no reports_to"
        );
    }

    #[test]
    fn failure_recipient_none_when_no_managers() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", None, false)])
            .build();

        assert_eq!(
            daemon.dispatch_failure_recipient("eng-1"),
            None,
            "should return None when no managers exist"
        );
    }

    #[test]
    fn failure_recipient_none_for_unknown_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("mgr", None)])
            .build();

        // Unknown engineer has no reports_to, falls back to first manager
        assert_eq!(
            daemon.dispatch_failure_recipient("ghost"),
            Some("mgr".to_string())
        );
    }

    #[test]
    fn escalate_sends_message_to_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .build();

        let entry = DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 42,
            task_title: "Test task".to_string(),
            queued_at: 0,
            validation_failures: 3,
            last_failure: Some("worktree dirty".to_string()),
        };

        daemon
            .escalate_dispatch_queue_entry(&entry, "worktree dirty")
            .unwrap();

        let inbox_root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&inbox_root, "mgr").unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].body.contains("Dispatch queue entry failed"));
        assert!(messages[0].body.contains("eng-1"));
        assert!(messages[0].body.contains("worktree dirty"));
    }

    #[test]
    fn escalate_suppresses_repeated_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .build();

        let entry = DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 42,
            task_title: "Test task".to_string(),
            queued_at: 0,
            validation_failures: 3,
            last_failure: Some("worktree dirty".to_string()),
        };

        // First call sends a message
        daemon
            .escalate_dispatch_queue_entry(&entry, "worktree dirty")
            .unwrap();
        // Second call is suppressed (same task+engineer within cooldown)
        daemon
            .escalate_dispatch_queue_entry(&entry, "worktree dirty")
            .unwrap();

        let inbox_root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&inbox_root, "mgr").unwrap();
        assert_eq!(
            messages.len(),
            1,
            "repeated escalation for same task+engineer should be suppressed"
        );
    }

    #[test]
    fn escalate_without_manager_does_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", None, false)])
            .build();

        let entry = DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 42,
            task_title: "orphan".to_string(),
            queued_at: 0,
            validation_failures: 3,
            last_failure: None,
        };

        // Should succeed (no-op) without panicking
        daemon
            .escalate_dispatch_queue_entry(&entry, "no manager")
            .unwrap();
    }
}
