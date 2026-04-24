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
        if self
            .config
            .team_config
            .workflow_mode
            .suppresses_manager_relay()
        {
            return Ok(());
        }
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
                .cloned()
            else {
                continue;
            };
            if member.role_type != RoleType::Manager {
                continue;
            }
            let stall_threshold = self.config.team_config.workflow_policy.stall_threshold_secs;
            let supervisory_stalled = self.is_supervisory_lane_stalled(&name, stall_threshold);

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
                    super::review::actionable_review_backlog_owner_for_task(
                        task,
                        &self.config.members,
                    )
                    .as_deref()
                        == Some(name.as_str())
                })
                .count();
            if review_count > 0 {
                continue;
            }
            let manager_idle = self.member_idle_for_dispatch_gap(&name);

            let report_snapshots: Vec<ReportDispatchSnapshot> = reports
                .iter()
                .map(|report| ReportDispatchSnapshot {
                    name: report.clone(),
                    is_working: !self.member_idle_for_dispatch_gap(report),
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

            let dispatchable_task_ids: std::collections::HashSet<u32> =
                crate::team::resolver::engineer_dispatchable_tasks(
                    &board_dir,
                    &self.config.members,
                )?
                .into_iter()
                .map(|task| task.id)
                .collect();
            let mut unassigned_open_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| dispatchable_task_ids.contains(&task.id))
                .collect();
            unassigned_open_tasks
                .sort_by_key(|task| (manager_dispatch_priority_rank(&task.priority), task.id));

            if idle_active_reports.is_empty() && unassigned_open_tasks.is_empty() {
                continue;
            }

            let dispatch_gap_stall_reason = self.manager_dispatch_gap_stall_reason(
                &name,
                stall_threshold,
                manager_idle,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            let dispatch_gap_stalled = supervisory_stalled || dispatch_gap_stall_reason.is_some();
            if supervisory_stalled {
                let reason = self.supervisory_progress_signal(&name, stall_threshold);
                self.record_supervisory_stall_reason(&name, stall_threshold, reason);
            } else if let Some((reason_suffix, short_label)) = dispatch_gap_stall_reason {
                self.record_manager_dispatch_gap_stall(
                    &name,
                    stall_threshold,
                    reason_suffix,
                    short_label,
                );
            }
            if !dispatch_gap_stalled && !manager_idle {
                continue;
            }
            if !dispatch_gap_stalled && !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            if dispatch_gap_stalled
                && !idle_unassigned_reports.is_empty()
                && !unassigned_open_tasks.is_empty()
            {
                let reason = if supervisory_stalled {
                    format!(
                        "manager_{}",
                        self.supervisory_progress_signal(&name, stall_threshold)
                            .stall_reason()
                    )
                } else {
                    let (reason_suffix, _) = dispatch_gap_stall_reason.unwrap();
                    format!("manager_supervisory_{reason_suffix}")
                };
                let fallback_count = self.fallback_direct_dispatch(
                    &name,
                    &reason,
                    &board_dir,
                    &idle_unassigned_reports,
                    &unassigned_open_tasks,
                )?;
                if fallback_count > 0 {
                    self.record_orchestrator_action(format!(
                        "recovery: dispatch fallback for {} assigned {} task(s) directly ({})",
                        name, fallback_count, reason
                    ));
                    continue;
                }
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
                &member,
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

    fn manager_dispatch_gap_stall_reason(
        &self,
        member_name: &str,
        threshold_secs: u64,
        member_idle: bool,
        idle_unassigned_reports: &[&ReportDispatchSnapshot],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> Option<(&'static str, &'static str)> {
        if threshold_secs == 0
            || member_idle
            || idle_unassigned_reports.is_empty()
            || unassigned_open_tasks.is_empty()
        {
            return None;
        }

        let handle = self.shim_handles.get(member_name)?;
        if handle.state != crate::shim::protocol::ShimState::Working
            || handle.secs_since_state_change() < threshold_secs
        {
            return None;
        }

        if handle
            .secs_since_last_activity()
            .is_some_and(|secs| secs < threshold_secs)
        {
            return Some(("shim_activity_only", "shim activity only"));
        }

        if self
            .watchers
            .get(member_name)
            .is_some_and(|watcher| watcher.secs_since_last_output_change() < threshold_secs)
        {
            return Some(("status_only_output", "status-only output"));
        }

        Some(("no_actionable_progress", "no actionable progress"))
    }

    fn member_idle_for_dispatch_gap(&self, member_name: &str) -> bool {
        self.shim_handles
            .get(member_name)
            .map(|handle| handle.state != crate::shim::protocol::ShimState::Working)
            .unwrap_or_else(|| self.is_member_idle(member_name))
    }

    fn record_manager_dispatch_gap_stall(
        &mut self,
        member_name: &str,
        stall_secs: u64,
        reason_suffix: &str,
        short_label: &str,
    ) {
        let cooldown_key = format!("supervisory-stall::{member_name}");
        let cooldown = std::time::Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        if self
            .intervention_cooldowns
            .get(&cooldown_key)
            .is_some_and(|last| last.elapsed() < cooldown)
        {
            return;
        }

        let observed_stall_secs = self
            .shim_handles
            .get(member_name)
            .map(|handle| handle.secs_since_state_change())
            .unwrap_or(stall_secs);
        let mut event = TeamEvent::stall_detected_with_reason(
            member_name,
            None,
            observed_stall_secs,
            Some(&format!("supervisory_stalled_manager_{reason_suffix}")),
        );
        event.task = Some(format!("supervisory::{member_name}"));
        event.details = Some(format!(
            "{member_name} (manager) stalled after {}: {short_label}",
            crate::team::status::format_health_duration(observed_stall_secs),
        ));
        self.emit_event(event);
        self.record_orchestrator_action(format!(
            "stall: detected {member_name} manager dispatch gap ({short_label})"
        ));
        self.intervention_cooldowns
            .insert(cooldown_key, Instant::now());
    }

    fn fallback_direct_dispatch(
        &mut self,
        manager_name: &str,
        reason: &str,
        board_dir: &std::path::Path,
        idle_unassigned_reports: &[&ReportDispatchSnapshot],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> Result<usize> {
        let mut dispatched = 0usize;
        for (report, task) in idle_unassigned_reports
            .iter()
            .zip(unassigned_open_tasks.iter())
        {
            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            crate::team::task_cmd::assign_task_owners(
                board_dir,
                task.id,
                Some(&report.name),
                None,
            )?;
            crate::team::task_cmd::transition_task_with_attribution(
                board_dir,
                task.id,
                "in-progress",
                crate::team::task_cmd::StatusTransitionAttribution::daemon(
                    "daemon.interventions.dispatch.fallback",
                ),
            )?;

            match self.assign_task_with_task_id_as(
                "daemon",
                &report.name,
                &assignment_message,
                Some(task.id),
            ) {
                Ok(_) => {
                    self.record_dispatch_fallback_used(manager_name, &report.name, task.id, reason);
                    dispatched += 1;
                }
                Err(error) => {
                    let _ = crate::team::task_cmd::transition_task_with_attribution(
                        board_dir,
                        task.id,
                        "todo",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.interventions.dispatch.rollback",
                        ),
                    );
                    let _ = crate::team::task_cmd::unclaim_task(board_dir, task.id);
                    warn!(
                        manager = %manager_name,
                        engineer = %report.name,
                        task_id = task.id,
                        error = %error,
                        "fallback direct-dispatch failed"
                    );
                }
            }
        }
        Ok(dispatched)
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
        let active_ids = idle_active_reports
            .iter()
            .flat_map(|snapshot| snapshot.active_task_ids.iter().copied())
            .collect::<std::collections::HashSet<_>>();
        let github_blockers = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))
            .map(|tasks| {
                let active_tasks = tasks
                    .iter()
                    .filter(|task| active_ids.contains(&task.id))
                    .collect::<Vec<_>>();
                crate::team::github_feedback::active_github_blockers_for_tasks(
                    &self.config.project_root,
                    &active_tasks,
                )
            })
            .unwrap_or_default();

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

        if !github_blockers.is_empty() {
            let blocker_lines = github_blockers
                .iter()
                .map(|feedback| format!("- {}", feedback.intervention_line()))
                .collect::<Vec<_>>()
                .join("\n");
            message.push_str(&format!(
                "\nGitHub/CI verification blockers on idle active work:\n{blocker_lines}"
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

fn manager_dispatch_priority_rank(priority: &str) -> u32 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
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
            assignee: None,
            claimed_by: None,
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
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
            assignee: None,
            claimed_by: None,
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
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

    #[test]
    fn dispatch_priority_rank_orders_named_priorities() {
        assert_eq!(manager_dispatch_priority_rank("critical"), 0);
        assert_eq!(manager_dispatch_priority_rank("high"), 1);
        assert_eq!(manager_dispatch_priority_rank("medium"), 2);
        assert_eq!(manager_dispatch_priority_rank("low"), 3);
        assert_eq!(manager_dispatch_priority_rank("unknown"), 4);
    }
}
