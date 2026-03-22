//! Review backlog intervention: nudges idle managers who have completed
//! direct-report work parked in review status waiting for merge/disposition.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::OwnedTaskInterventionState;
use crate::team::config::PlanningDirectiveFile;

impl TeamDaemon {
    pub(in super::super) fn maybe_intervene_review_backlog(&mut self) -> Result<()> {
        if !self.config.team_config.automation.review_interventions {
            return Ok(());
        }
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        if super::super::super::nudge_disabled_marker_path(&self.config.project_root, "review")
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
            if !self.is_member_idle(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let review_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| {
                    review_backlog_owner_for_task(task, &self.config.members).as_deref()
                        == Some(name.as_str())
                })
                .collect();
            if review_tasks.is_empty() {
                self.owned_task_interventions
                    .remove(&review_intervention_key(&name));
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            if idle_epoch == 0 {
                continue;
            }

            let signature = review_task_intervention_signature(&review_tasks);
            let review_key = review_intervention_key(&name);
            if self
                .owned_task_interventions
                .get(&review_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }
            if self.intervention_on_cooldown(&review_key) {
                continue;
            }

            let text = self.build_review_intervention_message(member, &review_tasks);
            info!(
                member = %name,
                review_task_count = review_tasks.len(),
                "firing review intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver review intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: review intervention for {} covering {} queued review task(s)",
                name,
                review_tasks.len()
            ));
            self.owned_task_interventions.insert(
                review_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(review_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn build_review_intervention_message(
        &self,
        member: &MemberInstance,
        review_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = review_tasks
            .iter()
            .map(|task| {
                let claimed_by = task.claimed_by.as_deref().unwrap_or("unknown");
                if let Some(context) = self.member_worktree_context(claimed_by) {
                    match context.branch {
                        Some(branch) => format!(
                            "#{} by {} [branch: {} | worktree: {}]",
                            task.id,
                            claimed_by,
                            branch,
                            context.path.display()
                        ),
                        None => format!(
                            "#{} by {} [worktree: {}]",
                            task.id,
                            claimed_by,
                            context.path.display()
                        ),
                    }
                } else {
                    format!("#{} by {}", task.id, claimed_by)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = review_tasks
            .iter()
            .map(|task| {
                let claimed_by = task.claimed_by.as_deref().unwrap_or("unknown");
                let mut lines = vec![
                    format!("- `kanban-md show --dir {board_dir_str} {}`", task.id),
                    format!("- `sed -n '1,220p' {}`", task.source_path.display()),
                ];
                if let Some(context) = self.member_worktree_context(claimed_by) {
                    lines.push(format!(
                        "- worktree: `{}`{}",
                        context.path.display(),
                        context
                            .branch
                            .as_deref()
                            .map(|branch| format!(" (branch `{branch}`)"))
                            .unwrap_or_default()
                    ));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = review_tasks[0];
        let first_report = first_task.claimed_by.as_deref().unwrap_or("engineer");
        let first_report_is_engineer = self
            .config
            .members
            .iter()
            .find(|candidate| candidate.name == first_report)
            .is_some_and(|candidate| candidate.role_type == RoleType::Engineer);

        let mut message = format!(
            "Review backlog detected: direct-report work has completed and is waiting for your review: {task_summary}.\n\
            Review and disposition it now:\n\
            1. `kanban-md list --dir {board_dir_str} --status review`\n\
            2. `batty inbox {member_name}` then `batty read {member_name} <ref>` to inspect the completion packet(s).\n\
            3. Review each task and its lane context:\n{task_context_cmds}",
            member_name = member.name,
        );

        if first_report_is_engineer {
            message.push_str(&format!(
                "\n4. To accept engineer work, run `batty merge {first_report}` then `kanban-md move --dir {board_dir_str} {task_id} done`.",
                task_id = first_task.id,
            ));
        } else {
            message.push_str(&format!(
                "\n4. To accept the review packet, move it forward with `kanban-md move --dir {board_dir_str} {task_id} done` and send the disposition to `{first_report}`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(&format!(
            "\n5. To discard it, run `kanban-md move --dir {board_dir_str} {task_id} archived` and `batty send {first_report} \"Task #{task_id} discarded: <reason>\"`.",
            task_id = first_task.id,
        ));
        let rework_command = if first_report_is_engineer {
            format!(
                "`batty assign {first_report} \"Task #{task_id}: <required changes>\"`",
                task_id = first_task.id
            )
        } else {
            format!(
                "`batty send {first_report} \"Task #{task_id}: <required changes>\"`",
                task_id = first_task.id
            )
        };
        message.push_str(&format!(
            "\n6. To request rework, run `kanban-md move --dir {board_dir_str} {task_id} in-progress` and {rework_command}.",
            task_id = first_task.id,
        ));

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n7. After each review decision, report upward with `batty send {parent} \"Reviewed Task #{task_id}: merged / archived / rework sent to {first_report}\"`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(
            "\nDo not leave completed direct-report work parked in review. Merge it, discard it, or send exact rework now. Batty will remind you again if review backlog remains unchanged.",
        );
        self.prepend_planning_directive(
            PlanningDirectiveFile::ReviewPolicy,
            "Review policy context:",
            message,
        )
    }
}

pub(super) fn review_backlog_owner_for_task(
    task: &crate::task::Task,
    members: &[MemberInstance],
) -> Option<String> {
    if task.status != "review" {
        return None;
    }
    let claimed_by = task.claimed_by.as_deref()?;
    Some(
        members
            .iter()
            .find(|member| member.name == claimed_by)
            .and_then(|member| member.reports_to.clone())
            .unwrap_or_else(|| claimed_by.to_string()),
    )
}

pub(super) fn review_intervention_key(member_name: &str) -> String {
    format!("review::{member_name}")
}

pub(super) fn review_task_intervention_signature(tasks: &[&crate::task::Task]) -> String {
    let mut parts = tasks
        .iter()
        .map(|task| {
            format!(
                "{}:{}:{}",
                task.id,
                task.status,
                task.claimed_by.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}
