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

        let body = format!(
            "Dispatch queue entry failed validation too many times.\nEngineer: {}\nTask #{}: {}\nFailures: {}\nLast failure: {}",
            entry.engineer, entry.task_id, entry.task_title, entry.validation_failures, detail
        );
        self.queue_daemon_message(&manager, &body)?;
        Ok(())
    }
}
