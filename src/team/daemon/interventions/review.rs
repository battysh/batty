//! Review backlog intervention: nudges idle managers who have completed
//! direct-report work parked in review status waiting for merge/disposition.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::OwnedTaskInterventionState;
use crate::team::config::PlanningDirectiveFile;
use crate::team::review::{
    ReviewMergeRemediationKind, ReviewNormalizationStep, ReviewQueueState,
    assess_review_merge_remediation,
};

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
            // Use epoch 1 as fallback so review interventions fire even before
            // the triage system has seen the manager go idle (e.g. after restart).
            let effective_epoch = if idle_epoch == 0 { 1 } else { idle_epoch };

            let signature = review_task_intervention_signature(&review_tasks);
            let review_key = review_intervention_key(&name);
            if self
                .owned_task_interventions
                .get(&review_key)
                .is_some_and(|state| {
                    state.signature == signature && state.idle_epoch == effective_epoch
                })
            {
                continue;
            }
            if self.intervention_on_cooldown(&review_key) {
                continue;
            }

            let text = self.build_review_intervention_message(member, &review_tasks, &tasks);
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
                    idle_epoch: effective_epoch,
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

    pub(super) fn build_review_intervention_message(
        &self,
        member: &MemberInstance,
        review_tasks: &[&crate::task::Task],
        board_tasks: &[crate::task::Task],
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
        let classified_tasks = review_tasks
            .iter()
            .map(|task| {
                (
                    *task,
                    crate::team::review::classify_review_task(
                        &self.config.project_root,
                        task,
                        board_tasks,
                    ),
                )
            })
            .collect::<Vec<_>>();
        let current_review_tasks = classified_tasks
            .iter()
            .filter_map(|(task, state)| match state {
                ReviewQueueState::Current => Some(*task),
                ReviewQueueState::Stale(_) => None,
            })
            .collect::<Vec<_>>();
        let stale_review_tasks = classified_tasks
            .iter()
            .filter_map(|(task, state)| match state {
                ReviewQueueState::Current => None,
                ReviewQueueState::Stale(stale) => Some((*task, stale)),
            })
            .collect::<Vec<_>>();
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

        let mut merge_cmds = Vec::new();
        let mut normalize_cmds = Vec::new();
        let mut suppressed_cmds = Vec::new();
        for task in current_review_tasks {
            let engineer = task.claimed_by.as_deref().unwrap_or("engineer");
            let is_eng = self
                .config
                .members
                .iter()
                .find(|m| m.name == engineer)
                .is_some_and(|m| m.role_type == RoleType::Engineer);
            let remediation = assess_review_merge_remediation(&self.config.project_root, task);
            let detail = remediation_branch_detail(task.id, &remediation);
            if is_eng {
                match remediation.kind {
                    ReviewMergeRemediationKind::Merge => {
                        merge_cmds.push(format!(
                            "   #{} by {} -> merge\n   {}\n   `batty merge {engineer}` && `kanban-md move --dir {board_dir_str} {} done`",
                            task.id, engineer, detail, task.id
                        ));
                    }
                    ReviewMergeRemediationKind::Normalize => {
                        info!(
                            task_id = task.id,
                            engineer,
                            expected_branch = ?remediation.expected_branch,
                            actual_branch = ?remediation.actual_branch,
                            commits_ahead = remediation.commits_ahead,
                            "review merge remediation switched to normalization"
                        );
                        normalize_cmds.push(format!(
                            "   #{} by {} -> normalize review\n   {}\n   Reason: {}.\n   {}",
                            task.id,
                            engineer,
                            detail,
                            remediation.reason,
                            normalize_review_command(&board_dir, task.id)
                        ));
                    }
                    ReviewMergeRemediationKind::Suppress => {
                        warn!(
                            task_id = task.id,
                            engineer,
                            expected_branch = ?remediation.expected_branch,
                            actual_branch = ?remediation.actual_branch,
                            commits_ahead = remediation.commits_ahead,
                            "review merge remediation suppressed"
                        );
                        suppressed_cmds.push(format!(
                            "   #{} by {} -> merge suppressed\n   {}\n   Reason: {}.\n   Inspect `git log --oneline main..{}` before taking action.",
                            task.id,
                            engineer,
                            detail,
                            remediation.reason,
                            remediation
                                .expected_branch
                                .as_deref()
                                .unwrap_or("HEAD")
                        ));
                    }
                }
            } else {
                normalize_cmds.push(format!(
                    "   #{} by {} -> normalize board state\n   `kanban-md move --dir {board_dir_str} {} done`",
                    task.id, engineer, task.id
                ));
            }
        }
        let mut step = 4;
        if !merge_cmds.is_empty() {
            message.push_str(&format!(
                "\n{step}. ACTION REQUIRED — verified live review lanes:\n{}",
                merge_cmds.join("\n"),
            ));
            step += 1;
        }

        if !normalize_cmds.is_empty() {
            message.push_str(&format!(
                "\n{step}. REVIEW NORMALIZATION — already merged or zero-ahead review lanes:\n{}",
                normalize_cmds.join("\n"),
            ));
            step += 1;
        }

        if !suppressed_cmds.is_empty() {
            message.push_str(&format!(
                "\n{step}. MERGE SUPPRESSED — branch verification failed for these lanes:\n{}",
                suppressed_cmds.join("\n"),
            ));
            step += 1;
        }

        if !stale_review_tasks.is_empty() {
            let stale_cmds = stale_review_tasks
                .iter()
                .map(|(task, stale)| {
                    stale_review_command(
                        &self.config.project_root,
                        stale.next_step,
                        &board_dir,
                        task.id,
                        task,
                        &self.config.members,
                        &stale.reason,
                    )
                })
                .collect::<Vec<_>>();
            message.push_str(&format!(
                "\n{step}. STALE REVIEW NORMALIZATION — these lanes already diverged from live engineer state:\n{}",
                stale_cmds.join("\n"),
            ));
            step += 1;
        }

        message.push_str(&format!(
            "\n{step}. To discard it, run `kanban-md move --dir {board_dir_str} {task_id} archived` and `batty send {first_report} \"Task #{task_id} discarded: <reason>\"`.",
            task_id = first_task.id,
        ));
        step += 1;
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
            "\n{step}. To request rework, run `kanban-md move --dir {board_dir_str} {task_id} in-progress` and {rework_command}.",
            task_id = first_task.id,
        ));
        step += 1;

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n{step}. After each review decision, report upward with `batty send {parent} \"Reviewed Task #{task_id}: merged / archived / rework sent to {first_report}\"`.",
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

fn current_review_recipient(claimed_by: &str, task_id: u32, members: &[MemberInstance]) -> String {
    let is_engineer = members
        .iter()
        .find(|member| member.name == claimed_by)
        .is_some_and(|member| member.role_type == RoleType::Engineer);
    if is_engineer {
        format!("`batty assign {claimed_by} \"Task #{task_id}: <required changes>\"`")
    } else {
        format!("`batty send {claimed_by} \"Task #{task_id}: <required changes>\"`")
    }
}

fn stale_review_command(
    project_root: &std::path::Path,
    next_step: ReviewNormalizationStep,
    board_dir: &std::path::Path,
    task_id: u32,
    task: &crate::task::Task,
    members: &[MemberInstance],
    stale_reason: &str,
) -> String {
    let claimed_by = task.claimed_by.as_deref().unwrap_or("engineer");
    let rework_command = current_review_recipient(claimed_by, task_id, members);
    let board_dir_str = board_dir.display();
    match next_step {
        ReviewNormalizationStep::Merge => {
            let remediation = assess_review_merge_remediation(project_root, task);
            let detail = remediation_branch_detail(task_id, &remediation);
            match remediation.kind {
                ReviewMergeRemediationKind::Merge => format!(
                    "   #{} by {} -> merge ({})\n   {}\n   `batty merge {claimed_by}` && `kanban-md move --dir {board_dir_str} {task_id} done`",
                    task_id, claimed_by, stale_reason, detail
                ),
                ReviewMergeRemediationKind::Normalize => format!(
                    "   #{} by {} -> normalize review ({})\n   {}\n   Reason: {}.\n   {}",
                    task_id,
                    claimed_by,
                    stale_reason,
                    detail,
                    remediation.reason,
                    normalize_review_command(board_dir, task_id)
                ),
                ReviewMergeRemediationKind::Suppress => format!(
                    "   #{} by {} -> merge suppressed ({})\n   {}\n   Reason: {}.\n   Inspect `git log --oneline main..{}` before taking action.",
                    task_id,
                    claimed_by,
                    stale_reason,
                    detail,
                    remediation.reason,
                    remediation.expected_branch.as_deref().unwrap_or("HEAD")
                ),
            }
        }
        ReviewNormalizationStep::Archive => {
            format!(
                "   #{} by {} -> archive ({})\n   `kanban-md move --dir {board_dir_str} {task_id} archived`",
                task_id, claimed_by, stale_reason
            )
        }
        ReviewNormalizationStep::Rework => format!(
            "   #{} by {} -> rework ({})\n   `kanban-md move --dir {board_dir_str} {task_id} in-progress` and {rework_command}",
            task_id, claimed_by, stale_reason
        ),
    }
}

fn normalize_review_command(board_dir: &std::path::Path, task_id: u32) -> String {
    let board_dir_str = board_dir.display();
    format!(
        "`batty review {task_id} approve \"Already merged upstream; normalize review backlog\"` && `kanban-md move --dir {board_dir_str} {task_id} done`"
    )
}

fn remediation_branch_detail(
    task_id: u32,
    remediation: &crate::team::review::ReviewMergeRemediation,
) -> String {
    let expected_branch = remediation.expected_branch.as_deref().unwrap_or("missing");
    let actual_branch = remediation
        .actual_branch
        .as_deref()
        .unwrap_or("unavailable");
    let commits_ahead = remediation
        .commits_ahead
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "Task #{task_id}: expected branch `{expected_branch}`, actual branch `{actual_branch}`, commits ahead of main: {commits_ahead}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::RoleType;
    use crate::team::hierarchy::MemberInstance;

    fn make_member(name: &str, role: RoleType, reports_to: Option<&str>) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: "test".to_string(),
            role_type: role,
            agent: None,
            prompt: None,
            reports_to: reports_to.map(str::to_string),
            use_worktrees: false,
            ..Default::default()
        }
    }

    fn make_task(id: u32, status: &str, claimed_by: Option<&str>) -> crate::task::Task {
        crate::task::Task {
            id,
            title: format!("task-{id}"),
            status: status.to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: claimed_by.map(str::to_string),
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
            source_path: std::path::PathBuf::from(format!("task-{id}.md")),
        }
    }

    #[test]
    fn review_key_uses_review_prefix() {
        assert_eq!(review_intervention_key("lead"), "review::lead");
    }

    #[test]
    fn review_signature_empty_returns_empty() {
        assert_eq!(review_task_intervention_signature(&[]), "");
    }

    #[test]
    fn review_signature_single_task() {
        let task = make_task(42, "review", Some("eng-1"));
        assert_eq!(
            review_task_intervention_signature(&[&task]),
            "42:review:eng-1"
        );
    }

    #[test]
    fn review_signature_unknown_when_no_claimed_by() {
        let task = make_task(42, "review", None);
        assert_eq!(
            review_task_intervention_signature(&[&task]),
            "42:review:unknown"
        );
    }

    #[test]
    fn review_backlog_owner_returns_none_for_unclaimed() {
        let task = make_task(42, "review", None);
        let members = vec![make_member("lead", RoleType::Manager, None)];
        assert_eq!(review_backlog_owner_for_task(&task, &members), None);
    }

    #[test]
    fn review_backlog_owner_returns_none_for_non_review() {
        let task = make_task(42, "in-progress", Some("eng-1"));
        let members = vec![
            make_member("lead", RoleType::Manager, None),
            make_member("eng-1", RoleType::Engineer, Some("lead")),
        ];
        assert_eq!(review_backlog_owner_for_task(&task, &members), None);
    }

    #[test]
    fn review_backlog_owner_uses_reports_to_when_member_found() {
        let task = make_task(42, "review", Some("eng-1"));
        let members = vec![
            make_member("lead", RoleType::Manager, Some("architect")),
            make_member("eng-1", RoleType::Engineer, Some("lead")),
        ];
        assert_eq!(
            review_backlog_owner_for_task(&task, &members),
            Some("lead".to_string())
        );
    }
}
