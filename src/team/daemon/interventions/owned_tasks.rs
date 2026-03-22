//! Owned-task intervention: nudges idle members who still own active board
//! tasks, and escalates to their manager if the task remains stuck past
//! the configured threshold.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::{OwnedTaskInterventionState, task_needs_owned_intervention};
use crate::team::config::PlanningDirectiveFile;

impl TeamDaemon {
    pub(in super::super) fn maybe_intervene_owned_tasks(&mut self) -> Result<()> {
        if !self.config.team_config.automation.owned_task_interventions {
            return Ok(());
        }
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        if super::super::super::nudge_disabled_marker_path(&self.config.project_root, "owned-task")
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
            if !self.is_member_idle(&name) {
                continue;
            }
            let owned_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| task.claimed_by.as_deref() == Some(name.as_str()))
                .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                .collect();
            if owned_tasks.is_empty() {
                self.owned_task_interventions.remove(&name);
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            let signature = owned_task_intervention_signature(&owned_tasks);
            if let Some(existing) = self.owned_task_interventions.get(&name) {
                if existing.signature == signature {
                    let stuck_age_secs = existing.detected_at.elapsed().as_secs();
                    let should_escalate = !existing.escalation_sent
                        && super::super::super::policy::should_escalate(
                            &self.config.team_config.workflow_policy,
                            stuck_age_secs,
                        );
                    if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                        state.idle_epoch = idle_epoch;
                        if !should_escalate {
                            continue;
                        }
                    }

                    let Some(parent) = member.reports_to.clone() else {
                        if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                            state.escalation_sent = true;
                        }
                        continue;
                    };
                    let text = self.build_stuck_task_escalation_message(
                        &member,
                        &owned_tasks,
                        stuck_age_secs,
                    );
                    info!(
                        member = %name,
                        parent = %parent,
                        owned_task_count = owned_tasks.len(),
                        stuck_age_secs,
                        "escalating stuck owned task"
                    );
                    match self.queue_message("daemon", &parent, &text) {
                        Ok(()) => {
                            self.record_orchestrator_action(format!(
                                "recovery: stuck-task escalation for {} to {} after {}s on {} active task(s)",
                                name,
                                parent,
                                stuck_age_secs,
                                owned_tasks.len()
                            ));
                            for task in &owned_tasks {
                                self.record_task_escalated(
                                    &name,
                                    task.id.to_string(),
                                    Some("stuck_task"),
                                );
                            }
                            if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                                state.escalation_sent = true;
                            }
                        }
                        Err(error) => {
                            warn!(member = %name, parent = %parent, error = %error, "failed to escalate stuck task");
                        }
                    }
                    continue;
                }
            }

            if self.intervention_on_cooldown(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let reports = direct_reports.get(&name).cloned().unwrap_or_default();
            let text = self.build_owned_task_intervention_message(&member, &owned_tasks, &reports);
            info!(
                member = %name,
                owned_task_count = owned_tasks.len(),
                "firing owned-task intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver owned-task intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: owned-task intervention for {} covering {} active task(s)",
                name,
                owned_tasks.len()
            ));
            self.owned_task_interventions.insert(
                name.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(name.clone(), Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    pub(super) fn build_owned_task_intervention_message(
        &self,
        member: &MemberInstance,
        owned_tasks: &[&crate::task::Task],
        direct_reports: &[String],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = owned_tasks
            .iter()
            .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = owned_tasks
            .iter()
            .map(|task| {
                format!(
                    "- `kanban-md show --dir {board_dir_str} {task_id}`\n- `sed -n '1,220p' {task_path}`",
                    task_id = task.id,
                    task_path = task.source_path.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = owned_tasks[0];

        let mut message = format!(
            "Owned active task backlog detected: you are idle but still own active board task(s): {task_summary}.\n\
            Retrieve task context now:\n\
            1. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
            2. Review each owned task:\n{task_context_cmds}",
        );

        if let Some(first_report) = direct_reports.first() {
            let report_is_engineer = self
                .config
                .members
                .iter()
                .find(|candidate| candidate.name == *first_report)
                .is_some_and(|candidate| candidate.role_type == RoleType::Engineer);
            if report_is_engineer {
                message.push_str(&format!(
                    "\n3. If the task can move, assign the next concrete slice now with `batty assign {first_report} \"Task #{task_id}: <scoped subtask>\"`.",
                    task_id = first_task.id,
                ));
            } else {
                message.push_str(&format!(
                    "\n3. If the task can move, delegate the next concrete step now with `batty send {first_report} \"Task #{task_id}: <next step>\"`.",
                    task_id = first_task.id,
                ));
            }
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n4. If the lane is blocked, escalate explicitly with `batty send {parent} \"Task #{task_id} blocker: <exact blocker and next decision>\"`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(&format!(
            "\n5. If the work is complete or ready for review, update board state now with `kanban-md move --dir {board_dir_str} {task_id} review` or `kanban-md move --dir {board_dir_str} {task_id} done` as appropriate.",
            task_id = first_task.id,
        ));
        message.push_str(
            "\nDo not stay idle while owning active work. Either move the task forward, split it, or escalate the blocker now. Batty will remind you again the next time you become idle while you still own unfinished tasks.",
        );
        self.prepend_member_nudge(member, message)
    }

    pub(super) fn build_stuck_task_escalation_message(
        &self,
        member: &MemberInstance,
        owned_tasks: &[&crate::task::Task],
        stuck_age_secs: u64,
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = owned_tasks
            .iter()
            .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = owned_tasks
            .iter()
            .map(|task| {
                format!(
                    "- `kanban-md show --dir {board_dir_str} {task_id}`\n- `sed -n '1,220p' {task_path}`",
                    task_id = task.id,
                    task_path = task.source_path.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = owned_tasks[0];
        let redirect_command = if member.role_type == RoleType::Engineer {
            format!(
                "`batty assign {member_name} \"Task #{task_id}: <next concrete step or unblock plan>\"`",
                member_name = member.name,
                task_id = first_task.id,
            )
        } else {
            format!(
                "`batty send {member_name} \"Task #{task_id}: <next concrete step or unblock plan>\"`",
                member_name = member.name,
                task_id = first_task.id,
            )
        };

        let mut message = format!(
            "Stuck task escalation: {member_name} has remained idle while still owning active board task(s) for at least {stuck_duration}: {task_summary}.\n\
            Intervene now:\n\
            1. `batty status`\n\
            2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
            3. Review the stuck task context:\n{task_context_cmds}\n\
            4. If the lane is executable, push the next action now with {redirect_command}.",
            member_name = member.name,
            stuck_duration = format_stuck_duration(stuck_age_secs),
        );

        message.push_str(&format!(
            "\n5. If the lane is blocked, record it now with `kanban-md edit --dir {board_dir_str} {task_id} --block \"<exact blocker>\" --claim {member_name}` and send the decision back to `{member_name}`.",
            task_id = first_task.id,
            member_name = member.name,
        ));

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n6. If you need a higher-level decision, escalate again with `batty send {parent} \"Task #{task_id} stuck under {member_name}: <decision needed>\"`.",
                task_id = first_task.id,
                member_name = member.name,
            ));
        }

        message.push_str(
            "\nDo not leave the task parked. Re-dispatch it, block it with a specific reason, or escalate the exact decision needed now.",
        );
        self.prepend_planning_directive(
            PlanningDirectiveFile::EscalationPolicy,
            "Escalation policy context:",
            message,
        )
    }
}

pub(super) fn owned_task_intervention_signature(tasks: &[&crate::task::Task]) -> String {
    let mut parts = tasks
        .iter()
        .map(|task| format!("{}:{}", task.id, task.status))
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: u32, status: &str) -> crate::task::Task {
        crate::task::Task {
            id,
            title: format!("task-{id}"),
            status: status.to_string(),
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
            source_path: std::path::PathBuf::from(format!("task-{id}.md")),
        }
    }

    #[test]
    fn signature_single_task() {
        let task = make_task(42, "in-progress");
        assert_eq!(
            owned_task_intervention_signature(&[&task]),
            "42:in-progress"
        );
    }

    #[test]
    fn signature_sorts_by_id() {
        let t1 = make_task(20, "todo");
        let t2 = make_task(10, "in-progress");
        assert_eq!(
            owned_task_intervention_signature(&[&t1, &t2]),
            "10:in-progress|20:todo"
        );
    }

    #[test]
    fn signature_empty() {
        assert_eq!(owned_task_intervention_signature(&[]), "");
    }
}
