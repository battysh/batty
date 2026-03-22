//! Manager dispatch-gap intervention: nudges idle managers when all their
//! reports are idle, there is no triage/review backlog, but there is
//! executable work available on the board or idle active tasks.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::{OwnedTaskInterventionState, task_needs_owned_intervention};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportDispatchSnapshot {
    name: String,
    is_working: bool,
    active_task_ids: Vec<u32>,
}

impl TeamDaemon {
    pub(in super::super) fn maybe_intervene_manager_dispatch_gap(&mut self) -> Result<()> {
        if !self
            .config
            .team_config
            .automation
            .manager_dispatch_interventions
        {
            return Ok(());
        }
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        if super::super::super::nudge_disabled_marker_path(&self.config.project_root, "dispatch")
            .exists()
        {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let direct_reports =
            super::super::super::status::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            if member.role_type != RoleType::Manager {
                continue;
            }
            if !self.is_member_idle(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let Some(reports) = direct_reports.get(&name) else {
                continue;
            };
            if reports.is_empty() {
                continue;
            }

            let triage_state = super::super::super::status::delivered_direct_report_triage_state(
                &inbox_root,
                &name,
                reports,
            )?;
            if triage_state.count > 0 {
                continue;
            }

            let review_count = tasks
                .iter()
                .filter(|task| {
                    super::review::review_backlog_owner_for_task(task, &self.config.members)
                        .as_deref()
                        == Some(name.as_str())
                })
                .count();
            if review_count > 0 {
                continue;
            }

            let report_snapshots: Vec<ReportDispatchSnapshot> = reports
                .iter()
                .map(|report| ReportDispatchSnapshot {
                    name: report.clone(),
                    is_working: !self.is_member_idle(report),
                    active_task_ids: tasks
                        .iter()
                        .filter(|task| task.claimed_by.as_deref() == Some(report.as_str()))
                        .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                        .map(|task| task.id)
                        .collect(),
                })
                .collect();

            if report_snapshots.iter().any(|snapshot| snapshot.is_working) {
                continue;
            }

            let idle_active_reports: Vec<&ReportDispatchSnapshot> = report_snapshots
                .iter()
                .filter(|snapshot| !snapshot.active_task_ids.is_empty())
                .collect();
            let idle_unassigned_reports: Vec<&ReportDispatchSnapshot> = report_snapshots
                .iter()
                .filter(|snapshot| snapshot.active_task_ids.is_empty())
                .collect();

            let unassigned_open_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| task.claimed_by.is_none())
                .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
                .collect();

            if idle_active_reports.is_empty() && unassigned_open_tasks.is_empty() {
                continue;
            }

            let dispatch_key = manager_dispatch_intervention_key(&name);
            let signature = manager_dispatch_intervention_signature(
                &idle_active_reports,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            if self
                .owned_task_interventions
                .get(&dispatch_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }
            if self.intervention_on_cooldown(&dispatch_key) {
                continue;
            }

            let text = self.build_manager_dispatch_gap_message(
                member,
                &idle_active_reports,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            info!(
                member = %name,
                idle_active_reports = idle_active_reports.len(),
                idle_unassigned_reports = idle_unassigned_reports.len(),
                unassigned_open_tasks = unassigned_open_tasks.len(),
                "firing manager dispatch-gap intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver manager dispatch-gap intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: dispatch-gap intervention for {} (idle reports with active work: {}, unassigned reports: {}, open tasks: {})",
                name,
                idle_active_reports.len(),
                idle_unassigned_reports.len(),
                unassigned_open_tasks.len()
            ));
            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            self.owned_task_interventions.insert(
                dispatch_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(dispatch_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn build_manager_dispatch_gap_message(
        &self,
        member: &MemberInstance,
        idle_active_reports: &[&ReportDispatchSnapshot],
        idle_unassigned_reports: &[&ReportDispatchSnapshot],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let active_report_summary = if idle_active_reports.is_empty() {
            "none".to_string()
        } else {
            idle_active_reports
                .iter()
                .map(|snapshot| {
                    let ids = snapshot
                        .active_task_ids
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{} on {}", snapshot.name, ids)
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let unassigned_report_summary = if idle_unassigned_reports.is_empty() {
            "none".to_string()
        } else {
            idle_unassigned_reports
                .iter()
                .map(|snapshot| snapshot.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let open_task_summary = if unassigned_open_tasks.is_empty() {
            "none".to_string()
        } else {
            unassigned_open_tasks
                .iter()
                .take(3)
                .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let mut message = format!(
            "Dispatch recovery needed: you are idle, your reports are idle, and the lane has no triage/review backlog. Idle reports still holding active work: {active_report_summary}. Idle reports with no active task: {unassigned_report_summary}. Unassigned open board work: {open_task_summary}.\n\
            Recover the lane now:\n\
            1. `batty status`\n\
            2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
            3. `kanban-md list --dir {board_dir_str} --status todo`\n\
            4. `kanban-md list --dir {board_dir_str} --status backlog`"
        );

        if let Some(first_active) = idle_active_reports.first() {
            let first_task_id = first_active.active_task_ids[0];
            message.push_str(&format!(
                "\n5. For an idle active lane, intervene directly with `batty send {report} \"Task #{task_id} is idle under your ownership. Either move it forward now, report the exact blocker, or request board normalization.\"`.",
                report = first_active.name,
                task_id = first_task_id,
            ));
        }

        if let (Some(first_unassigned_report), Some(first_open_task)) = (
            idle_unassigned_reports.first(),
            unassigned_open_tasks.first(),
        ) {
            message.push_str(&format!(
                "\n6. If executable work exists, start it now with `batty assign {report} \"Task #{task_id}: {title}\"`.",
                report = first_unassigned_report.name,
                task_id = first_open_task.id,
                title = first_open_task.title,
            ));
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n7. If the lane has no executable next step, escalate explicitly with `batty send {parent} \"lane blocked: all reports idle; need new dispatch or decision\"`."
            ));
        }

        message.push_str(
            "\nDo not let the entire lane sit idle. Either wake an active task, assign new executable work, or escalate the exact blockage now.",
        );
        self.prepend_member_nudge(member, message)
    }
}

pub(super) fn manager_dispatch_intervention_key(member_name: &str) -> String {
    format!("dispatch::{member_name}")
}

pub(super) fn manager_dispatch_intervention_signature(
    idle_active_reports: &[&ReportDispatchSnapshot],
    idle_unassigned_reports: &[&ReportDispatchSnapshot],
    unassigned_open_tasks: &[&crate::task::Task],
) -> String {
    let mut parts = Vec::new();
    for snapshot in idle_active_reports {
        let task_ids = snapshot
            .active_task_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("active:{}:{task_ids}", snapshot.name));
    }
    for snapshot in idle_unassigned_reports {
        parts.push(format!("idle:{}", snapshot.name));
    }
    for task in unassigned_open_tasks {
        parts.push(format!("open:{}:{}", task.id, task.status));
    }
    parts.sort();
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_key_uses_dispatch_prefix() {
        assert_eq!(manager_dispatch_intervention_key("lead"), "dispatch::lead");
        assert_eq!(
            manager_dispatch_intervention_key("mgr-2"),
            "dispatch::mgr-2"
        );
    }

    #[test]
    fn dispatch_signature_includes_active_reports_with_task_ids() {
        let active = ReportDispatchSnapshot {
            name: "eng-1".to_string(),
            is_working: false,
            active_task_ids: vec![10, 20],
        };
        let sig = manager_dispatch_intervention_signature(&[&active], &[], &[]);
        assert_eq!(sig, "active:eng-1:10,20");
    }

    #[test]
    fn dispatch_signature_includes_idle_and_open_components() {
        let idle = ReportDispatchSnapshot {
            name: "eng-2".to_string(),
            is_working: false,
            active_task_ids: vec![],
        };
        let task = crate::task::Task {
            id: 50,
            title: "open-task".to_string(),
            status: "todo".to_string(),
            priority: "high".to_string(),
            claimed_by: None,
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: String::new(),
            batty_config: None,
            source_path: std::path::PathBuf::from("task-50.md"),
        };
        let sig = manager_dispatch_intervention_signature(&[], &[&idle], &[&task]);
        assert_eq!(sig, "idle:eng-2|open:50:todo");
    }

    #[test]
    fn dispatch_signature_empty_inputs_returns_empty() {
        let sig = manager_dispatch_intervention_signature(&[], &[], &[]);
        assert_eq!(sig, "");
    }

    #[test]
    fn dispatch_signature_sorts_all_components() {
        let active = ReportDispatchSnapshot {
            name: "eng-z".to_string(),
            is_working: false,
            active_task_ids: vec![5],
        };
        let idle = ReportDispatchSnapshot {
            name: "eng-a".to_string(),
            is_working: false,
            active_task_ids: vec![],
        };
        let task = crate::task::Task {
            id: 1,
            title: "task".to_string(),
            status: "backlog".to_string(),
            priority: "high".to_string(),
            claimed_by: None,
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: String::new(),
            batty_config: None,
            source_path: std::path::PathBuf::from("task-1.md"),
        };
        let sig = manager_dispatch_intervention_signature(&[&active], &[&idle], &[&task]);
        assert_eq!(sig, "active:eng-z:5|idle:eng-a|open:1:backlog");
    }

    #[test]
    fn dispatch_signature_multiple_active_reports() {
        let a1 = ReportDispatchSnapshot {
            name: "eng-2".to_string(),
            is_working: false,
            active_task_ids: vec![30],
        };
        let a2 = ReportDispatchSnapshot {
            name: "eng-1".to_string(),
            is_working: false,
            active_task_ids: vec![10, 20],
        };
        let sig = manager_dispatch_intervention_signature(&[&a1, &a2], &[], &[]);
        // Should sort: active:eng-1:10,20 before active:eng-2:30
        assert_eq!(sig, "active:eng-1:10,20|active:eng-2:30");
    }
}
