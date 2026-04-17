//! Periodic automation subsystems extracted from daemon.rs.
//!
//! Review timeout, dependency unblocking, pipeline starvation,
//! worktree reconciliation, board rotation, cron, retrospectives.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use super::helpers::MemberWorktreeContext;
use super::*;

const STATE_RECONCILIATION_AUDIT_KEY: &str = "state-reconciliation::audit";
const CLAIM_PROGRESS_AUDIT_KEY_PREFIX: &str = "claim-progress::";
const CLAIMED_BRANCH_MISMATCH_ALERT_KEY_PREFIX: &str = "claimed-branch-mismatch::";
const ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX: &str = "orphan-branch-mismatch::";
const DEFAULT_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaimedTaskBranchMismatch {
    pub(super) task_id: u32,
    pub(super) expected_branch: String,
    pub(super) current_branch: String,
}

fn normalized_context_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }

    let trimmed = trimmed.trim_start_matches('#').trim();
    let trimmed = trimmed.trim_start_matches('-').trim();
    let trimmed = trimmed.trim_start_matches('*').trim();
    let trimmed = trimmed
        .strip_prefix(|ch: char| ch.is_ascii_digit())
        .and_then(|rest| rest.strip_prefix('.'))
        .map(str::trim)
        .unwrap_or(trimmed);

    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn collect_context_lines(path: &std::path::Path, limit: usize) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|content| {
            content
                .lines()
                .filter_map(normalized_context_line)
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}

fn planning_prompt_context(project_root: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let planning_dir = project_root.join("planning");
    let roadmap = collect_context_lines(&planning_dir.join("roadmap.md"), 6);

    let goals = [
        planning_dir.join("architecture.md"),
        planning_dir.join("dev-philosophy.md"),
        project_root.join("README.md"),
    ]
    .into_iter()
    .find_map(|path| {
        let lines = collect_context_lines(&path, 6);
        if lines.is_empty() { None } else { Some(lines) }
    })
    .unwrap_or_default();

    (roadmap, goals)
}

fn orphan_branch_mismatch_max_attempts() -> u32 {
    std::env::var("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS)
}

fn claim_time_held_secs(task: &crate::task::Task, now: DateTime<Utc>) -> Option<u64> {
    task.claimed_at
        .as_deref()
        .and_then(parse_rfc3339_utc)
        .and_then(|claimed_at| {
            let held_secs = now.signed_duration_since(claimed_at).num_seconds();
            (held_secs >= 0).then_some(held_secs as u64)
        })
}

fn current_review_task<'a>(
    tasks_by_id: &HashMap<u32, &'a crate::task::Task>,
    task_id: u32,
) -> Option<&'a crate::task::Task> {
    tasks_by_id
        .get(&task_id)
        .copied()
        .filter(|task| task.status == "review")
}

fn active_stale_review_entries(
    report: &crate::team::board::TaskAgingReport,
    tasks_by_id: &HashMap<u32, &crate::task::Task>,
) -> (Vec<crate::team::board::AgedTask>, Vec<u32>) {
    let mut active = Vec::new();
    let mut suppressed = Vec::new();

    for task in &report.stale_review {
        if current_review_task(tasks_by_id, task.task_id).is_some() {
            active.push(task.clone());
        } else {
            suppressed.push(task.task_id);
        }
    }

    (active, suppressed)
}

fn reset_claimed_worktree_to_base(
    work_dir: &std::path::Path,
    base_branch: &str,
) -> Result<crate::worktree::WorktreeResetReason> {
    let branch = current_worktree_branch(work_dir).unwrap_or_else(|_| base_branch.to_string());
    let commit_message = format!("wip: auto-save before worktree reset [{branch}]");
    crate::worktree::reset_worktree_to_base_with_options_for(
        work_dir,
        base_branch,
        &commit_message,
        Duration::from_secs(5),
        crate::worktree::PreserveFailureMode::SkipReset,
        "state-reconciliation/claimed-lane-reset",
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}

fn owned_task_status_rank(status: &str) -> u8 {
    match status {
        "in-progress" => 0,
        "todo" => 1,
        "backlog" => 2,
        _ => 3,
    }
}

fn owned_task_priority_rank(priority: &str) -> u32 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn owned_task_collision_rank(task: &crate::task::Task) -> (u32, bool, Option<DateTime<Utc>>, u32) {
    (
        owned_task_priority_rank(&task.priority),
        task.claimed_at.is_none(),
        task.claimed_at.as_deref().and_then(parse_rfc3339_utc),
        task.id,
    )
}

fn select_authoritative_multi_claim_task<'a>(
    claimed_tasks: &[&'a crate::task::Task],
) -> Option<&'a crate::task::Task> {
    claimed_tasks
        .iter()
        .copied()
        .min_by_key(|task| owned_task_collision_rank(task))
}

fn select_authoritative_owned_task<'a>(
    current_task_id: Option<u32>,
    current_branch: Option<&str>,
    claimed_tasks: &[&'a crate::task::Task],
) -> Option<&'a crate::task::Task> {
    if let Some(task_id) = current_task_id
        && let Some(task) = claimed_tasks
            .iter()
            .copied()
            .find(|task| task.id == task_id)
    {
        return Some(task);
    }

    if let Some(branch) = current_branch {
        let mut matches = claimed_tasks
            .iter()
            .copied()
            .filter(|task| task.branch.as_deref() == Some(branch));
        if let Some(task) = matches.next()
            && matches.next().is_none()
        {
            return Some(task);
        }
    }

    claimed_tasks.iter().copied().min_by_key(|task| {
        (
            owned_task_status_rank(task.status.as_str()),
            owned_task_priority_rank(&task.priority),
            task.claimed_at.is_none(),
            task.claimed_at.as_deref().and_then(parse_rfc3339_utc),
            task.id,
        )
    })
}

fn authoritative_task_branch(engineer: &str, task: &crate::task::Task) -> String {
    task.branch
        .clone()
        .unwrap_or_else(|| format!("{engineer}/{}", task.id))
}

fn is_managed_task_branch(engineer: &str, branch: &str) -> bool {
    branch.starts_with(&format!("{engineer}/"))
}

fn format_branch_recovery_blocker(reason: &str) -> String {
    format!("automatic branch recovery blocked: {reason}")
}

fn sync_stale_review_next_action(
    task: &crate::task::Task,
    stale: &crate::team::review::StaleReviewState,
) -> Result<bool> {
    let next_action = stale.status_next_action();
    if task.next_action.as_deref() == Some(next_action.as_str()) {
        return Ok(false);
    }

    crate::team::task_cmd::update_task_frontmatter(&task.source_path, |mapping| {
        crate::team::task_cmd::set_optional_string(mapping, "next_action", Some(&next_action));
    })?;
    Ok(true)
}

impl TeamDaemon {
    pub(super) fn claimed_task_branch_mismatch(
        &self,
        engineer: &str,
    ) -> Result<Option<ClaimedTaskBranchMismatch>> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
        else {
            return Ok(None);
        };
        if !member.use_worktrees {
            return Ok(None);
        }

        let tasks_dir = self.board_dir().join("tasks");
        if !tasks_dir.exists() {
            return Ok(None);
        }

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.exists() {
            return Ok(None);
        }

        let current_branch = match current_worktree_branch(&worktree_dir) {
            Ok(branch) => branch,
            Err(_) => return Ok(None),
        };

        let board_tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let claimed: Vec<&crate::task::Task> = board_tasks
            .iter()
            .filter(|task| {
                task.claimed_by.as_deref() == Some(engineer)
                    && super::interventions::task_needs_owned_intervention(task.status.as_str())
            })
            .collect();
        let Some(task) = select_authoritative_owned_task(
            self.active_task_id(engineer),
            (current_branch != "HEAD").then_some(current_branch.as_str()),
            &claimed,
        ) else {
            return Ok(None);
        };

        let expected_branch = authoritative_task_branch(engineer, task);
        if current_branch == expected_branch {
            return Ok(None);
        }

        Ok(Some(ClaimedTaskBranchMismatch {
            task_id: task.id,
            expected_branch,
            current_branch,
        }))
    }

    pub(super) fn alert_claimed_task_branch_mismatch(
        &mut self,
        engineer: &str,
        mismatch: &ClaimedTaskBranchMismatch,
        actionable_reason: Option<&str>,
    ) -> Result<()> {
        let cooldown_key = format!("{CLAIMED_BRANCH_MISMATCH_ALERT_KEY_PREFIX}{engineer}");
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs
                .max(60),
        );
        if self
            .intervention_cooldowns
            .get(&cooldown_key)
            .is_some_and(|last| last.elapsed() < cooldown)
        {
            return Ok(());
        }
        self.intervention_cooldowns
            .insert(cooldown_key, Instant::now());

        let reason_suffix = actionable_reason
            .map(|reason| format!(" Reason: {reason}."))
            .unwrap_or_default();
        let alert = format!(
            "Reconciliation alert: {engineer} is claimed on task #{} but the worktree is on '{}' instead of '{}'. Batty will refuse new assignment and suppress automation nudges until the branch is corrected manually.{reason_suffix}",
            mismatch.task_id, mismatch.current_branch, mismatch.expected_branch
        );
        let manager = self.assignment_sender(engineer);
        let _ = self.queue_daemon_message(&manager, &alert);
        let _ = self.queue_daemon_message(
            engineer,
            &format!(
                "Branch/task mismatch detected for task #{}. Current branch: '{}'. Expected branch: '{}'. {} Resolve it manually before continuing.",
                mismatch.task_id,
                mismatch.current_branch,
                mismatch.expected_branch,
                actionable_reason.unwrap_or("Automatic recovery is paused.")
            ),
        );

        self.emit_event(TeamEvent::state_reconciliation(
            Some(engineer),
            Some(&mismatch.task_id.to_string()),
            "branch_mismatch",
        ));
        self.record_orchestrator_action(format!(
            "state reconciliation: detected branch/task mismatch for {} (task #{}, current '{}', expected '{}')",
            engineer, mismatch.task_id, mismatch.current_branch, mismatch.expected_branch
        ));
        Ok(())
    }

    pub(in crate::team) fn deliver_automation_nudge(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        if self
            .config
            .members
            .iter()
            .any(|member| member.name == recipient && member.role_type == RoleType::Engineer)
            && let Some(mismatch) = self.claimed_task_branch_mismatch(recipient)?
        {
            self.alert_claimed_task_branch_mismatch(recipient, &mismatch, None)?;
            self.record_orchestrator_action(format!(
                "suppressed automation nudge for {} while branch/task mismatch is unresolved",
                recipient
            ));
            return Ok(MessageDelivery::OrchestratorLogged);
        }

        if self.config.team_config.orchestrator_enabled() {
            let summary = body
                .lines()
                .find_map(normalized_context_line)
                .unwrap_or_else(|| "automation nudge".to_string());
            self.record_orchestrator_action(format!("diverted nudge for {recipient}: {summary}"));
            return Ok(MessageDelivery::OrchestratorLogged);
        }

        self.queue_daemon_message(recipient, body)
    }

    pub(super) fn maybe_manage_task_claim_ttls(&mut self) -> Result<()> {
        let tasks_dir = self.board_dir().join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }

        let board_tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let now = Utc::now();
        let progress_interval = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .claim_ttl
                .progress_check_interval_secs,
        );

        for task in board_tasks
            .iter()
            .filter(|task| task.status == "in-progress")
            .filter(|task| task.claimed_by.is_some())
        {
            let Some(engineer) = task.claimed_by.as_deref() else {
                continue;
            };
            // #674 defect 3: skip TTL reclaim when the claimed engineer is
            // backend-parked (quota_exhausted with future retry_at). Parked
            // engineers cannot produce progress by definition — reclaiming
            // their tasks only rotates them to another quota-blocked
            // engineer and inflates board churn.
            if self.member_backend_parked(engineer) {
                continue;
            }
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == engineer)
                .cloned()
            else {
                continue;
            };
            let use_worktrees = member.use_worktrees;
            let work_dir = if use_worktrees {
                self.config
                    .project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(engineer)
            } else {
                self.member_work_dir(&member)
            };
            let ttl_secs = self.claim_ttl_secs_for_priority(&task.priority);
            let current_output_bytes = self
                .shim_handles
                .get(engineer)
                .map(|handle| handle.output_bytes)
                .unwrap_or(0);

            if task.claim_expires_at.is_none()
                || task.last_progress_at.is_none()
                || task.claim_ttl_secs.is_none()
            {
                crate::team::task_cmd::initialize_task_claim(
                    &self.board_dir(),
                    task.id,
                    ttl_secs,
                    now,
                    current_output_bytes,
                )?;
                self.emit_event(TeamEvent::task_claim_created(
                    engineer,
                    &task.id.to_string(),
                    ttl_secs,
                    &(now + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339(),
                ));
                continue;
            }

            let progress_key = format!("{CLAIM_PROGRESS_AUDIT_KEY_PREFIX}{}", task.id);
            let should_check_progress = self
                .intervention_cooldowns
                .get(&progress_key)
                .is_none_or(|last| last.elapsed() >= progress_interval);
            let Some(expires_at) = task.claim_expires_at.as_deref().and_then(parse_rfc3339_utc)
            else {
                continue;
            };

            if should_check_progress
                && let Some(progress_type) =
                    task_claim_progress_type(task, &work_dir, current_output_bytes)
            {
                if expires_at <= now && progress_type == "dirty_files" {
                    self.intervention_cooldowns
                        .insert(progress_key.clone(), Instant::now());
                } else {
                    let extensions = task.claim_extensions.unwrap_or(0).saturating_add(1).min(
                        self.config
                            .team_config
                            .workflow_policy
                            .claim_ttl
                            .max_extensions,
                    );
                    crate::team::task_cmd::refresh_task_claim_progress(
                        &self.board_dir(),
                        task.id,
                        ttl_secs,
                        now,
                        current_output_bytes,
                        extensions,
                    )?;
                    self.emit_event(TeamEvent::task_claim_progress(
                        engineer,
                        &task.id.to_string(),
                        progress_type,
                    ));
                    self.emit_event(TeamEvent::task_claim_extended(
                        engineer,
                        &task.id.to_string(),
                        &(now + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339(),
                    ));
                    self.intervention_cooldowns
                        .insert(progress_key.clone(), Instant::now());
                    continue;
                }
            }

            self.intervention_cooldowns
                .insert(progress_key, Instant::now());

            let expires_in_secs = expires_at.signed_duration_since(now).num_seconds().max(0) as u64;
            if expires_in_secs
                <= self
                    .config
                    .team_config
                    .workflow_policy
                    .claim_ttl
                    .warning_secs
                && task.claim_warning_sent_at.is_none()
            {
                crate::team::task_cmd::mark_task_claim_warning(&self.board_dir(), task.id, now)?;
                self.emit_event(TeamEvent::task_claim_warning(
                    engineer,
                    &task.id.to_string(),
                    expires_in_secs,
                ));
                let _ = self.queue_message(
                    "daemon",
                    engineer,
                    &format!(
                        "Your claim on task #{} expires in {} minutes. Commit your work or report a blocker to extend it.",
                        task.id,
                        (expires_in_secs / 60).max(1)
                    ),
                );
            }

            if expires_at > now {
                continue;
            }

            let time_held_secs = claim_time_held_secs(task, now);
            if use_worktrees {
                let base_branch = engineer_base_branch_name(engineer);
                match reset_claimed_worktree_to_base(&work_dir, &base_branch) {
                    Ok(reason) if reason.reset_performed() => {
                        self.record_orchestrator_action(format!(
                            "claim ttl: reset {} worktree to {} before reclaiming task #{} ({})",
                            engineer,
                            base_branch,
                            task.id,
                            reason.as_str()
                        ))
                    }
                    Ok(reason) => {
                        warn!(
                            engineer = %engineer,
                            task_id = task.id,
                            worktree = %work_dir.display(),
                            reset_reason = reason.as_str(),
                            "claim ttl: skipped engineer worktree reset before reclaim"
                        );
                        self.report_preserve_failure(
                            engineer,
                            Some(task.id),
                            "TTL reclaim/reset",
                            reason.as_str(),
                        );
                        self.record_orchestrator_action(format!(
                            "claim ttl: skipped reclaim reset for {} task #{} ({})",
                            engineer,
                            task.id,
                            reason.as_str()
                        ));
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            engineer = %engineer,
                            task_id = task.id,
                            worktree = %work_dir.display(),
                            error = %error,
                            "claim ttl: failed to reset engineer worktree before reclaim"
                        );
                        self.report_preserve_failure(
                            engineer,
                            Some(task.id),
                            "TTL reclaim/reset",
                            &error.to_string(),
                        );
                        continue;
                    }
                }
            }
            let branch = task
                .branch
                .clone()
                .or_else(|| current_worktree_branch(&work_dir).ok());
            let next_action = match branch.as_deref() {
                Some(branch) => {
                    format!(
                        "Reclaimed after TTL expiry. Previous work preserved on branch {branch}."
                    )
                }
                None => {
                    "Reclaimed after TTL expiry. Previous work preserved in the engineer worktree."
                        .to_string()
                }
            };
            crate::team::task_cmd::reclaim_task_claim(&self.board_dir(), task.id, &next_action)?;
            self.clear_active_task(engineer);
            self.emit_event(TeamEvent::task_claim_expired(
                engineer,
                &task.id.to_string(),
                true,
                time_held_secs,
            ));
            self.recent_dispatches
                .insert((task.id, engineer.to_string()), Instant::now());
            let manager = self.assignment_sender(engineer);
            let _ = self.queue_message(
                "daemon",
                &manager,
                &format!(
                    "Task #{} reclaimed from {} after {} minutes with no progress.",
                    task.id,
                    engineer,
                    ttl_secs / 60
                ),
            );
            self.record_orchestrator_action(format!(
                "claim ttl: reclaimed task #{} from {} after {} minutes without progress",
                task.id,
                engineer,
                ttl_secs / 60
            ));
        }

        Ok(())
    }

    fn maybe_reset_engineer_to_safe_branch(
        &mut self,
        engineer: &str,
        current_branch: &str,
        authorized_tasks: &[&crate::task::Task],
        completed_task: Option<&crate::task::Task>,
        reason: &str,
    ) {
        let base_branch = engineer_base_branch_name(engineer);
        if current_branch == "HEAD" || current_branch == base_branch {
            return;
        }
        if authorized_tasks
            .iter()
            .any(|task| task.branch.as_deref() == Some(current_branch))
        {
            return;
        }

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.exists() {
            return;
        }

        if let Some(task) = completed_task {
            let team_config_dir = self.config.project_root.join(".batty").join("team_config");
            match crate::team::task_loop::quarantine_completed_lane_for_recovery(
                &self.config.project_root,
                &worktree_dir,
                engineer,
                task,
                current_branch,
                reason,
                &team_config_dir,
                Duration::from_secs(10),
            ) {
                Ok(Some(record)) => {
                    self.emit_event(TeamEvent::worktree_reconciled(engineer, current_branch));
                    self.record_state_reconciliation(
                        Some(engineer),
                        Some(task.id),
                        "done_lane_reset",
                    );
                    self.record_orchestrator_action(format!(
                        "state reconciliation: quarantined completed task #{} for {} before reset ({})",
                        task.id,
                        engineer,
                        record.doctor_check_line()
                    ));
                }
                Ok(None) => {
                    self.emit_event(TeamEvent::worktree_reconciled(engineer, current_branch));
                    self.record_state_reconciliation(
                        Some(engineer),
                        Some(task.id),
                        "done_lane_reset",
                    );
                    self.record_orchestrator_action(format!(
                        "state reconciliation: reset completed task lane #{} for {} to '{}'",
                        task.id, engineer, base_branch
                    ));
                }
                Err(error) => {
                    self.report_preserve_failure(
                        engineer,
                        Some(task.id),
                        "completed-task cleanup/reset",
                        &error.to_string(),
                    );
                    self.record_orchestrator_action(format!(
                        "state reconciliation: blocked completed task lane #{} for {} ({error})",
                        task.id, engineer
                    ));
                }
            }
            return;
        }

        match reset_claimed_worktree_to_base(&worktree_dir, &base_branch) {
            Ok(reset_reason) if reset_reason.reset_performed() => {
                info!(
                    engineer = %engineer,
                    stale_branch = %current_branch,
                    reset_to = %base_branch,
                    reset_reason = reset_reason.as_str(),
                    reason,
                    "state reconciliation reset engineer worktree to safe branch"
                );
                self.emit_event(TeamEvent::worktree_reconciled(engineer, current_branch));
                self.record_state_reconciliation(Some(engineer), None, "branch_reset");
                self.record_orchestrator_action(format!(
                    "state reconciliation: reset {engineer} from stale branch '{current_branch}' to '{base_branch}' ({reason}, {})",
                    reset_reason.as_str()
                ));
            }
            Ok(reset_reason) => {
                self.report_preserve_failure(
                    engineer,
                    None,
                    "safe-branch recovery",
                    reset_reason.as_str(),
                );
                self.record_orchestrator_action(format!(
                    "state reconciliation: skipped resetting {engineer} from stale branch '{current_branch}' to '{base_branch}' ({reason}, {})",
                    reset_reason.as_str()
                ))
            }
            Err(error) => {
                warn!(
                    engineer = %engineer,
                    branch = %current_branch,
                    reset_to = %base_branch,
                    error = %error,
                    reason,
                    "state reconciliation failed to reset engineer worktree to safe branch"
                );
                self.report_preserve_failure(
                    engineer,
                    None,
                    "safe-branch recovery",
                    &error.to_string(),
                );
            }
        }
    }

    pub(super) fn claim_ttl_secs_for_priority(&self, priority: &str) -> u64 {
        let policy = &self.config.team_config.workflow_policy.claim_ttl;
        if priority.eq_ignore_ascii_case("critical") {
            policy.critical_secs
        } else {
            policy.default_secs
        }
    }

    pub(super) fn reconcile_active_tasks(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        let mut board_tasks = if tasks_dir.exists() {
            crate::task::load_tasks_from_dir(&tasks_dir)?
        } else {
            Vec::new()
        };
        let audit_due = self.state_reconciliation_audit_due();
        if audit_due {
            self.mark_state_reconciliation_audit();
        }
        let stale: Vec<(String, u32, &'static str, bool)> = self
            .active_tasks
            .iter()
            .filter_map(|(engineer, task_id)| {
                let task_id = *task_id;
                match board_tasks.iter().find(|t| t.id == task_id) {
                    Some(task) if task.status == "done" => {
                        Some((engineer.clone(), task_id, "task is done", false))
                    }
                    Some(task) if task.status == "archived" => {
                        Some((engineer.clone(), task_id, "task is archived", false))
                    }
                    Some(task) if task.status == "review" => {
                        Some((engineer.clone(), task_id, "task entered review", true))
                    }
                    Some(task) if task.status == "blocked" => {
                        Some((engineer.clone(), task_id, "task entered blocked", true))
                    }
                    None => Some((engineer.clone(), task_id, "task no longer exists", false)),
                    Some(task) if task.claimed_by.as_deref() != Some(engineer.as_str()) => Some((
                        engineer.clone(),
                        task_id,
                        "task no longer claimed by this engineer",
                        false,
                    )),
                    // Clear if the task has been in todo/backlog for more than 60 seconds.
                    // This gives dispatch time to transition it to in-progress before
                    // reconciliation clears it. Without the delay, dispatch assigns a task,
                    // reconciliation immediately clears the active entry, and dispatch
                    // re-assigns in an infinite loop.
                    _ => None,
                }
            })
            .collect();
        let mut released_claims = false;
        for (engineer, task_id, reason, release_claim) in stale {
            if reason == "task is done" {
                if let Some(task) = board_tasks.iter().find(|task| task.id == task_id) {
                    let worktree_dir = self.worktree_dir(&engineer);
                    if worktree_dir.exists() {
                        let source_branch = authoritative_task_branch(&engineer, task);
                        let team_config_dir =
                            self.config.project_root.join(".batty").join("team_config");
                        match crate::team::task_loop::quarantine_completed_lane_for_recovery(
                            &self.config.project_root,
                            &worktree_dir,
                            &engineer,
                            task,
                            &source_branch,
                            "completed task no longer needs engineer lane",
                            &team_config_dir,
                            Duration::from_secs(10),
                        ) {
                            Ok(Some(record)) => {
                                self.record_orchestrator_action(format!(
                                    "state reconciliation: preserved completed task #{} for {} before cleanup ({})",
                                    task_id,
                                    engineer,
                                    record.doctor_check_line()
                                ));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                self.report_preserve_failure(
                                    &engineer,
                                    Some(task_id),
                                    "completed-task cleanup/reset",
                                    &error.to_string(),
                                );
                                self.record_orchestrator_action(format!(
                                    "state reconciliation: blocked completed task #{} cleanup for {} ({error})",
                                    task_id, engineer
                                ));
                                continue;
                            }
                        }
                    }
                }
            }
            info!(
                engineer = %engineer,
                task_id,
                reason,
                "Reconciled stale active_task: {engineer} was tracking task #{task_id} ({reason})"
            );
            let correction = match reason {
                "task entered review" => "release_review",
                "task entered blocked" => "release_blocked",
                _ => "clear",
            };
            if release_claim {
                crate::team::task_cmd::release_engineer_claim(&board_dir, task_id)?;
                released_claims = true;
            }
            self.record_state_reconciliation(Some(&engineer), Some(task_id), correction);
            self.record_orchestrator_action(format!(
                "state reconciliation: {} stale active task #{} for {} ({})",
                if release_claim {
                    "released engineer from"
                } else {
                    "cleared"
                },
                task_id,
                engineer,
                reason
            ));
            // #630: if the task is done/archived but the engineer's
            // worktree is still dirty on the old task branch,
            // snapshot the dirty state into a preservation commit
            // so the worktree can be freed for the next task. Without
            // this call the engineer sits on a completed branch
            // indefinitely until a human intervenes.
            if reason == "task is done/archived" {
                let worktree_dir = self.worktree_dir(&engineer);
                if worktree_dir.exists() {
                    self.preserve_worktree_before_restart(
                        &engineer,
                        &worktree_dir,
                        "post_approval_dirty_lane_recovery",
                    );
                }
            }
            self.clear_active_task(&engineer);
        }
        if released_claims {
            board_tasks = if tasks_dir.exists() {
                crate::task::load_tasks_from_dir(&tasks_dir)?
            } else {
                Vec::new()
            };
        }

        let engineer_names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .map(|m| m.name.clone())
            .collect();
        for eng in &engineer_names {
            let released_excess_claims = self.normalize_engineer_active_task_ownership(
                &board_tasks,
                &board_dir,
                eng,
                audit_due,
            )?;
            if released_excess_claims {
                board_tasks = if tasks_dir.exists() {
                    crate::task::load_tasks_from_dir(&tasks_dir)?
                } else {
                    Vec::new()
                };
            }
        }

        // Skip tasks that are currently tracked in active_tasks — those were just
        // dispatched this cycle and the board file may not reflect the claim yet.
        let actively_tracked: std::collections::HashSet<u32> =
            self.active_tasks.values().copied().collect();

        // Orphaned review rescue: tasks in "review" that are stuck because
        // they have no review_owner (nobody assigned to review). Review tasks
        // no longer require claimed_by once the engineer is auto-released.
        for task in &board_tasks {
            if task.status == "review"
                && task.review_owner.is_none()
                && !actively_tracked.contains(&task.id)
            {
                warn!(
                    task_id = task.id,
                    "orphaned review task #{} has no owner — moving back to todo", task.id
                );
                let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "in-progress");
                let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "todo");
                let _ = crate::team::task_cmd::unclaim_task(&board_dir, task.id);
                // #684 / #686: same exponential-backoff dispatch-cooldown
                // pattern as in-progress rescue.
                self.record_task_rescue(task.id);
            }
        }

        // Orphaned in-progress rescue: tasks in "in-progress" with no claimed_by.
        for task in &board_tasks {
            if task.status == "in-progress"
                && task.claimed_by.is_none()
                && !actively_tracked.contains(&task.id)
            {
                warn!(
                    task_id = task.id,
                    "orphaned in-progress task #{} has no owner — moving back to todo", task.id
                );
                let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "todo");
                // #684 / #686: hold the task off the dispatch queue with an
                // exponentially-growing cooldown on repeated rescues so the
                // releasing engineer or manager has a widening window to
                // reclaim/re-route before it auto-dispatches to a peer.
                self.record_task_rescue(task.id);
            }
        }

        Ok(())
    }

    fn state_reconciliation_audit_due(&self) -> bool {
        let interval = Duration::from_secs(
            self.config
                .team_config
                .board
                .state_reconciliation_interval_secs,
        );
        self.intervention_cooldowns
            .get(STATE_RECONCILIATION_AUDIT_KEY)
            .is_none_or(|last| last.elapsed() >= interval)
    }

    fn mark_state_reconciliation_audit(&mut self) {
        self.intervention_cooldowns
            .insert(STATE_RECONCILIATION_AUDIT_KEY.to_string(), Instant::now());
    }

    fn normalize_engineer_active_task_ownership(
        &mut self,
        board_tasks: &[crate::task::Task],
        board_dir: &std::path::Path,
        engineer: &str,
        audit_due: bool,
    ) -> Result<bool> {
        let claimed: Vec<&crate::task::Task> = board_tasks
            .iter()
            .filter(|task| {
                task.claimed_by.as_deref() == Some(engineer)
                    && super::interventions::task_needs_owned_intervention(task.status.as_str())
            })
            .collect();

        if claimed.is_empty() {
            return Ok(false);
        }

        let worktree_dir = self.worktree_dir(engineer);
        let current_branch =
            if self.states.get(engineer) == Some(&MemberState::Idle) && worktree_dir.exists() {
                current_worktree_branch(&worktree_dir).ok()
            } else {
                None
            };

        let branch_matches: Vec<u32> = current_branch
            .as_deref()
            .map(|branch| {
                claimed
                    .iter()
                    .copied()
                    .filter(|task| authoritative_task_branch(engineer, task) == branch)
                    .map(|task| task.id)
                    .collect()
            })
            .unwrap_or_default();
        let worktree_matches: Vec<u32> = claimed
            .iter()
            .copied()
            .filter(|task| {
                task.worktree_path.as_deref().is_some_and(|path| {
                    let candidate = std::path::PathBuf::from(path);
                    let resolved = if candidate.is_absolute() {
                        candidate
                    } else {
                        self.config.project_root.join(candidate)
                    };
                    resolved == worktree_dir
                })
            })
            .map(|task| task.id)
            .collect();

        let authoritative_id = if claimed.len() > 1 {
            select_authoritative_multi_claim_task(&claimed)
                .map(|task| task.id)
                .expect("claimed tasks is not empty")
        } else if let [task_id] = branch_matches.as_slice() {
            *task_id
        } else if let [task_id] = worktree_matches.as_slice() {
            *task_id
        } else {
            select_authoritative_owned_task(
                self.active_task_id(engineer),
                current_branch.as_deref(),
                &claimed,
            )
            .map(|task| task.id)
            .expect("claimed tasks is not empty")
        };

        if self.active_task_id(engineer) != Some(authoritative_id) {
            let reason = if claimed.len() == 1 {
                "adopt"
            } else {
                "repair"
            };
            self.active_tasks
                .insert(engineer.to_string(), authoritative_id);
            self.record_state_reconciliation(Some(engineer), Some(authoritative_id), reason);
            self.record_orchestrator_action(format!(
                "state reconciliation: set authoritative active task #{} for {}",
                authoritative_id, engineer
            ));
        }

        let mut released_excess_claims = false;
        if claimed.len() > 1 {
            for task in claimed
                .iter()
                .copied()
                .filter(|task| task.id != authoritative_id)
            {
                warn!(
                    engineer,
                    task_id = task.id,
                    authoritative_task_id = authoritative_id,
                    "state reconciliation: releasing excess claimed task"
                );
                crate::team::task_cmd::reclaim_task_claim(
                    board_dir,
                    task.id,
                    &format!(
                        "Reclaimed during reconciliation while {engineer} retained authoritative task #{authoritative_id}."
                    ),
                )?;
                released_excess_claims = true;
                self.record_state_reconciliation(Some(engineer), Some(task.id), "repair");
                self.record_orchestrator_action(format!(
                    "state reconciliation: released excess claimed task #{} from {} (keeping #{})",
                    task.id, engineer, authoritative_id
                ));
            }
        }

        if audit_due
            && let Some(task) = claimed
                .iter()
                .copied()
                .find(|task| task.id == authoritative_id)
        {
            self.maybe_align_engineer_worktree_with_task(engineer, task)?;
        }

        Ok(released_excess_claims)
    }

    fn maybe_align_engineer_worktree_with_task(
        &mut self,
        engineer: &str,
        task: &crate::task::Task,
    ) -> Result<()> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
        else {
            return Ok(());
        };
        if !member.use_worktrees {
            return Ok(());
        }
        // Self-heal runs for both Idle and Working members (#666). Previously
        // we skipped when the member was Working, to avoid interrupting a live
        // session — but that left engineers permanently wedged when a branch
        // mismatch appeared on a non-idle lane: the "branch recovery blocked"
        // signal would re-emit every monitor tick with no healing action,
        // because the engineer's state was still Working despite the lane
        // being unable to make progress. Auto-saving dirty work as a wip
        // commit before switching branches is non-destructive — the stale
        // branch retains every byte of user work, and the session either
        // picks up on the right branch or restarts naturally. The
        // `audit_due` gate upstream already rate-limits how often this path
        // runs, so the cost of letting it run while Working is bounded.

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.exists() {
            return Ok(());
        }

        let current_branch = match current_worktree_branch(&worktree_dir) {
            Ok(branch) => branch,
            Err(_) => return Ok(()),
        };
        let mismatch = ClaimedTaskBranchMismatch {
            task_id: task.id,
            expected_branch: authoritative_task_branch(engineer, task),
            current_branch,
        };
        if mismatch.current_branch == mismatch.expected_branch {
            self.retry_counts.remove(&format!(
                "{ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX}{engineer}::{}",
                task.id
            ));
            return Ok(());
        }

        if mismatch.current_branch == "HEAD" {
            self.handle_orphan_branch_mismatch_failure(engineer, task, &mismatch)?;
            return self.alert_claimed_task_branch_mismatch(
                engineer,
                &mismatch,
                Some(&format_branch_recovery_blocker(
                    "worktree is in detached HEAD state",
                )),
            );
        }

        // Preserve any dirty changes as an auto-save commit on the current
        // branch BEFORE switching. This lets the recovery path work on lanes
        // that have in-flight engineer work, instead of refusing to act.
        // The existing work is preserved on the wrong branch — the engineer
        // can cherry-pick it to the correct branch after recovery lands them
        // on the expected lane.
        if crate::team::task_loop::worktree_has_user_changes(&worktree_dir)? {
            let auto_save_message = format!(
                "wip: auto-save before branch recovery from {} to {}",
                mismatch.current_branch, mismatch.expected_branch
            );
            match crate::team::task_loop::preserve_worktree_with_commit_for(
                &worktree_dir,
                &auto_save_message,
                Duration::from_secs(10),
                "state-reconciliation/branch-recovery",
            ) {
                Ok(true) => {
                    self.record_orchestrator_action(format!(
                        "state reconciliation: auto-saved dirty worktree for {} on branch '{}' before recovery to '{}'",
                        engineer, mismatch.current_branch, mismatch.expected_branch
                    ));
                }
                Ok(false) => {
                    // worktree_has_user_changes said dirty but preserve saw
                    // nothing to save; fall through to checkout.
                }
                Err(error) => {
                    self.handle_orphan_branch_mismatch_failure(engineer, task, &mismatch)?;
                    return self.alert_claimed_task_branch_mismatch(
                        engineer,
                        &mismatch,
                        Some(&format_branch_recovery_blocker(&format!(
                            "failed to auto-save dirty worktree before recovery: {error}"
                        ))),
                    );
                }
            }
        }

        let switch_result = if crate::team::git_cmd::show_ref_exists(
            &self.config.project_root,
            &mismatch.expected_branch,
        )
        .map_err(|error| anyhow::anyhow!("failed to inspect expected branch: {error}"))?
        {
            crate::team::git_cmd::run_git(&worktree_dir, &["checkout", &mismatch.expected_branch])
                .map(|_| ())
                .map_err(|error| anyhow::anyhow!("failed to checkout expected branch: {error}"))
        } else {
            crate::team::git_cmd::checkout_new_branch(
                &worktree_dir,
                &mismatch.expected_branch,
                "main",
            )
            .map_err(|error| anyhow::anyhow!("failed to create expected branch from main: {error}"))
        };

        match switch_result {
            Ok(()) => {
                self.retry_counts.remove(&format!(
                    "{ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX}{engineer}::{}",
                    task.id
                ));
                self.record_state_reconciliation(Some(engineer), Some(task.id), "branch_repair");
                self.record_orchestrator_action(format!(
                    "state reconciliation: repaired {} from branch '{}' to '{}' for task #{}",
                    engineer, mismatch.current_branch, mismatch.expected_branch, task.id
                ));
                Ok(())
            }
            Err(error) => {
                self.handle_orphan_branch_mismatch_failure(engineer, task, &mismatch)?;
                self.alert_claimed_task_branch_mismatch(
                    engineer,
                    &mismatch,
                    Some(&format_branch_recovery_blocker(&error.to_string())),
                )
            }
        }
    }

    fn handle_orphan_branch_mismatch_failure(
        &mut self,
        engineer: &str,
        task: &crate::task::Task,
        mismatch: &ClaimedTaskBranchMismatch,
    ) -> Result<()> {
        if task.status != "in-progress" {
            return Ok(());
        }

        let retry_key = format!(
            "{ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX}{engineer}::{}",
            task.id
        );
        let attempts = self.retry_counts.entry(retry_key.clone()).or_insert(0);
        *attempts += 1;
        let max_attempts = orphan_branch_mismatch_max_attempts();
        if *attempts < max_attempts {
            return Ok(());
        }

        warn!(
            task_id = task.id,
            engineer,
            current_branch = %mismatch.current_branch,
            expected_branch = %mismatch.expected_branch,
            "persistent orphaned in-progress branch mismatch exceeded retry limit; moving task back to todo"
        );
        let board_dir = self.board_dir();
        let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "todo");
        let _ = crate::team::task_cmd::unclaim_task(&board_dir, task.id);
        self.clear_active_task(engineer);
        self.retry_counts.remove(&retry_key);
        self.record_state_reconciliation(
            Some(engineer),
            Some(task.id),
            "orphan_branch_mismatch_requeued",
        );
        self.record_orchestrator_action(format!(
            "state reconciliation: requeued orphaned task #{} for {} after {} branch-mismatch recovery attempt(s)",
            task.id, engineer, max_attempts
        ));
        Ok(())
    }

    pub(super) fn maybe_escalate_stale_reviews(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }
        let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Clone policy to avoid borrow conflict with &mut self methods below
        let policy = self.config.team_config.workflow_policy.clone();

        // Collect IDs of tasks currently in review
        let review_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|t| t.status == "review")
            .map(|t| t.id)
            .collect();

        // Prune tracking maps for tasks no longer in review and log the repair.
        let stale_review_first_seen: Vec<u32> = self
            .review_first_seen
            .keys()
            .copied()
            .filter(|id| !review_task_ids.contains(id))
            .collect();
        for task_id in stale_review_first_seen {
            self.review_first_seen.remove(&task_id);
            self.record_state_reconciliation(None, Some(task_id), "review_fix");
            self.record_orchestrator_action(format!(
                "state reconciliation: cleared stale review tracking for task #{}",
                task_id
            ));
        }
        let stale_review_nudges: Vec<u32> = self
            .review_nudge_sent
            .iter()
            .copied()
            .filter(|id| !review_task_ids.contains(id))
            .collect();
        for task_id in stale_review_nudges {
            self.review_nudge_sent.remove(&task_id);
            self.record_state_reconciliation(None, Some(task_id), "review_fix");
            self.record_orchestrator_action(format!(
                "state reconciliation: cleared stale review nudge state for task #{}",
                task_id
            ));
        }

        for task in &tasks {
            if task.status != "review" {
                continue;
            }

            let review_state =
                crate::team::review::classify_review_task(&self.config.project_root, task, &tasks);
            if matches!(
                task.next_action.as_deref(),
                Some(next_action) if next_action.starts_with("stale review ->")
            ) && matches!(review_state, crate::team::review::ReviewQueueState::Current)
            {
                crate::team::task_cmd::update_task_frontmatter(&task.source_path, |mapping| {
                    crate::team::task_cmd::set_optional_string(mapping, "next_action", None);
                })?;
                self.record_state_reconciliation(None, Some(task.id), "review_fix");
                self.record_orchestrator_action(format!(
                    "state reconciliation: cleared stale review normalization hint for task #{}",
                    task.id
                ));
            }

            if !self.review_first_seen.contains_key(&task.id) {
                self.record_state_reconciliation(None, Some(task.id), "review_fix");
                self.record_orchestrator_action(format!(
                    "state reconciliation: seeded missing review tracking for task #{}",
                    task.id
                ));
                self.review_first_seen.insert(task.id, now);
            }
            let first_seen = *self.review_first_seen.get(&task.id).unwrap_or(&now);
            let age = now.saturating_sub(first_seen);

            // Resolve per-priority thresholds (falls back to global defaults)
            let nudge_threshold =
                super::super::policy::effective_nudge_threshold(&policy, &task.priority);
            let timeout_threshold =
                super::super::policy::effective_escalation_threshold(&policy, &task.priority);

            if let crate::team::review::ReviewQueueState::Stale(stale) = &review_state {
                if sync_stale_review_next_action(task, stale)? {
                    self.record_state_reconciliation(None, Some(task.id), "review_fix");
                    self.record_orchestrator_action(format!(
                        "state reconciliation: normalized stale review task #{} -> {} ({})",
                        task.id,
                        stale.next_step.as_str(),
                        stale.reason
                    ));
                }
                if !self.review_nudge_sent.contains(&task.id) {
                    let review_owner = task
                        .review_owner
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| {
                            task.claimed_by
                                .as_deref()
                                .and_then(|owner| self.manager_for_member_name(owner))
                                .map(str::to_string)
                        })
                        .or_else(|| first_manager_name(&self.config.members));
                    if let Some(review_owner) = review_owner.as_deref() {
                        let body = format!(
                            "Stale review normalization: task #{} no longer matches the live engineer lane.\nTask: {}\nReason: {}\nNext step: {}.",
                            task.id,
                            task.title,
                            stale.reason,
                            stale.status_next_action()
                        );
                        let _ = self.deliver_automation_nudge(review_owner, &body);
                    }
                    self.review_nudge_sent.insert(task.id);
                }
                continue;
            }

            // Check escalation first (higher threshold)
            if age >= timeout_threshold {
                // Escalate to architect
                let architect = self
                    .config
                    .members
                    .iter()
                    .find(|m| m.role_type == RoleType::Architect)
                    .map(|m| m.name.clone());

                if let Some(architect_name) = architect {
                    let msg = format!(
                        "Review timeout: task #{} has been in review for {}s (threshold: {}s). \
                         Escalating for resolution.",
                        task.id, age, timeout_threshold,
                    );
                    let _ = self.queue_daemon_message(&architect_name, &msg);
                    self.record_orchestrator_action(format!(
                        "review_escalated: task #{} -> {architect_name}",
                        task.id,
                    ));
                }

                if let Err(error) = self.event_sink.emit(TeamEvent::review_escalated(
                    &task.id.to_string(),
                    &format!("review timeout after {age}s"),
                )) {
                    warn!(error = %error, "failed to emit review_escalated event");
                }

                // Transition to blocked
                let _ = super::super::task_cmd::transition_task(&board_dir, task.id, "blocked");
                let _ = super::super::task_cmd::cmd_update(
                    &board_dir,
                    task.id,
                    std::collections::HashMap::from([(
                        "blocked_on".to_string(),
                        "review timeout escalated to architect".to_string(),
                    )]),
                );

                // Remove from tracking since it's no longer in review
                self.review_first_seen.remove(&task.id);
                self.review_nudge_sent.remove(&task.id);
                continue;
            }

            // Check nudge threshold
            if age >= nudge_threshold && !self.review_nudge_sent.contains(&task.id) {
                let reviewer = task.review_owner.as_deref().unwrap_or("manager");
                let msg = format!(
                    "Review nudge: task #{} has been in review for {}s (nudge threshold: {}s). \
                     Please review or escalate.",
                    task.id, age, nudge_threshold,
                );
                let _ = self.deliver_automation_nudge(reviewer, &msg);
                self.record_orchestrator_action(format!(
                    "review_nudge_sent: task #{} -> {reviewer}",
                    task.id,
                ));

                if let Err(error) = self
                    .event_sink
                    .emit(TeamEvent::review_nudge_sent(reviewer, &task.id.to_string()))
                {
                    warn!(error = %error, "failed to emit review_nudge_sent event");
                }

                self.review_nudge_sent.insert(task.id);
            }
        }

        Ok(())
    }

    pub(super) fn maybe_emit_task_aging_alerts(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }
        // #679: messaging below assumes a single git repo at `project_root`
        // (branch, commits ahead of main, etc.). When the project isn't a
        // git repo (or is a multi-repo parent), reframe the aging alert to
        // avoid referencing branches that don't exist.
        let project_is_git = self.is_git_repo && !self.is_multi_repo;

        let thresholds = crate::team::board::AgingThresholds {
            stale_in_progress_hours: self
                .config
                .team_config
                .workflow_policy
                .stale_in_progress_hours,
            aged_todo_hours: self.config.team_config.workflow_policy.aged_todo_hours,
            stale_review_hours: self.config.team_config.workflow_policy.stale_review_hours,
        };
        let report = crate::team::board::compute_task_aging(
            &board_dir,
            &self.config.project_root,
            thresholds,
        )?;
        let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let tasks_by_id = tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<HashMap<_, _>>();
        let (active_stale_review, suppressed_stale_review) =
            active_stale_review_entries(&report, &tasks_by_id);
        for task_id in suppressed_stale_review {
            self.record_state_reconciliation(None, Some(task_id), "review_fix");
            self.record_orchestrator_action(format!(
                "state reconciliation: suppressed stale review aging alert for task #{}",
                task_id
            ));
        }
        let progress_window = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        let now = Utc::now();

        let active_keys: HashSet<String> = report
            .stale_in_progress
            .iter()
            .map(|task| aging_cooldown_key("task_stale", task.task_id))
            .chain(
                report
                    .stale_in_progress
                    .iter()
                    .map(|task| aging_cooldown_key("task_checkpoint", task.task_id)),
            )
            .chain(
                report
                    .aged_todo
                    .iter()
                    .map(|task| aging_cooldown_key("task_aged", task.task_id)),
            )
            .chain(
                active_stale_review
                    .iter()
                    .map(|task| aging_cooldown_key("review_stale", task.task_id)),
            )
            .collect();
        self.intervention_cooldowns
            .retain(|key, _| !key.starts_with("aging::") || active_keys.contains(key));

        for task in &report.stale_in_progress {
            let stale_key = aging_cooldown_key("task_stale", task.task_id);
            let checkpoint_key = aging_cooldown_key("task_checkpoint", task.task_id);

            let owner = task.claimed_by.as_deref().unwrap_or("unassigned");
            // #674 defect 4: do not escalate stuck-task or emit task_stale
            // alerts when the owner is backend-parked. Parked engineers are
            // waiting, not stalled — escalating produces board churn and
            // noisy telemetry during already-degraded windows. Also clear
            // the cooldown markers so that when the engineer recovers we
            // start aging bookkeeping from a clean slate.
            if task
                .claimed_by
                .as_deref()
                .is_some_and(|name| self.member_backend_parked(name))
            {
                self.intervention_cooldowns.remove(&stale_key);
                self.intervention_cooldowns.remove(&checkpoint_key);
                continue;
            }
            let liveness = tasks_by_id.get(&task.task_id).and_then(|board_task| {
                self.in_progress_task_liveness(board_task, now, progress_window)
            });

            if let Some(liveness) = liveness {
                if liveness.has_live_progress() {
                    self.intervention_cooldowns.remove(&stale_key);
                    self.intervention_cooldowns.remove(&checkpoint_key);
                    continue;
                }

                if liveness.dirty_worktree {
                    let checkpoint_sent = self.intervention_cooldowns.contains_key(&checkpoint_key);
                    if !checkpoint_sent {
                        let body = if project_is_git {
                            format!(
                                "Task #{} is still in progress and has dirty worktree changes, but the branch has no commits ahead of `main`.\nTask: {}\nNext step: create a checkpoint commit now so Batty does not escalate this lane as stale.\n```\ngit add -A\ngit commit -m \"wip: checkpoint task #{}\"\n```",
                                task.task_id, task.title, task.task_id
                            )
                        } else {
                            format!(
                                "Task #{} is still in progress with uncommitted changes, but no progress signals have been observed during the cooldown window.\nTask: {}\nNext step: save a checkpoint of your work so Batty does not escalate this lane as stale.",
                                task.task_id, task.title
                            )
                        };
                        let _ = self.queue_daemon_message(owner, &body);
                        self.record_orchestrator_action(format!(
                            "aging: requested checkpoint commit for task #{} ({owner})",
                            task.task_id
                        ));
                        self.intervention_cooldowns
                            .insert(checkpoint_key, Instant::now());
                        self.intervention_cooldowns.remove(&stale_key);
                        continue;
                    }
                    if self.aging_alert_on_cooldown(&checkpoint_key) {
                        continue;
                    }
                } else {
                    self.intervention_cooldowns.remove(&checkpoint_key);
                }
            }

            if self.aging_alert_on_cooldown(&stale_key) {
                continue;
            }

            let (reason_code, reason, manager_reason) = if liveness
                .is_some_and(|state| state.dirty_worktree)
            {
                (
                    "commit_stale_with_dirty_work",
                    format!(
                        "commit stale after {}s with dirty worktree changes and no claim progress during the cooldown window",
                        task.age_secs
                    ),
                    "commit_stale_with_dirty_work",
                )
            } else if project_is_git {
                (
                    "progress_stale",
                    format!(
                        "progress stale after {}s with no commits ahead of main and no claim progress during the cooldown window",
                        task.age_secs
                    ),
                    "progress_stale",
                )
            } else {
                (
                    "progress_stale",
                    format!(
                        "progress stale after {}s with no progress signals during the cooldown window",
                        task.age_secs
                    ),
                    "progress_stale",
                )
            };
            self.emit_event(TeamEvent::task_stale(
                owner,
                &task.task_id.to_string(),
                &reason,
            ));
            self.record_task_escalated(owner, task.task_id.to_string(), Some(reason_code));

            if let Some(recipient) = task
                .claimed_by
                .as_deref()
                .and_then(|owner| self.manager_for_member_name(owner))
                .map(str::to_string)
                .or_else(|| first_manager_name(&self.config.members))
            {
                let body = if project_is_git {
                    format!(
                        "Task #{} has been in progress for {}s with no commits ahead of `main`.\nTask: {}\nOwner: {}\nReason: {}\nNext step: intervene, split the task, or confirm the engineer is still making progress.",
                        task.task_id, task.age_secs, task.title, owner, manager_reason
                    )
                } else {
                    format!(
                        "Task #{} has been in progress for {}s with no progress signals.\nTask: {}\nOwner: {}\nReason: {}\nNext step: intervene, split the task, or confirm the engineer is still making progress.",
                        task.task_id, task.age_secs, task.title, owner, manager_reason
                    )
                };
                let _ = self.queue_daemon_message(&recipient, &body);
            }

            self.intervention_cooldowns
                .insert(stale_key, Instant::now());
        }

        for task in &report.aged_todo {
            let key = aging_cooldown_key("task_aged", task.task_id);
            if self.aging_alert_on_cooldown(&key) {
                continue;
            }

            let reason = format!("todo task aged {}s without movement", task.age_secs);
            self.emit_event(TeamEvent::task_aged(&task.task_id.to_string(), &reason));
            self.intervention_cooldowns.insert(key, Instant::now());
        }

        for task in &active_stale_review {
            let key = aging_cooldown_key("review_stale", task.task_id);
            if self.aging_alert_on_cooldown(&key) {
                continue;
            }

            let current_task = current_review_task(&tasks_by_id, task.task_id)
                .expect("filtered stale review task should still be in review");
            let review_owner = Some(current_task)
                .and_then(|task| task.review_owner.as_deref())
                .map(str::to_string)
                .or_else(|| {
                    current_task
                        .claimed_by
                        .as_deref()
                        .and_then(|owner| self.manager_for_member_name(owner))
                        .map(str::to_string)
                })
                .or_else(|| first_manager_name(&self.config.members));
            let review_state = crate::team::review::classify_review_task(
                &self.config.project_root,
                current_task,
                &tasks,
            );
            let reason = match &review_state {
                crate::team::review::ReviewQueueState::Current => {
                    format!("review queue stale after {}s", task.age_secs)
                }
                crate::team::review::ReviewQueueState::Stale(stale) => format!(
                    "stale review normalization after {}s: {}",
                    task.age_secs, stale.reason
                ),
            };
            self.emit_event(TeamEvent::review_stale(&task.task_id.to_string(), &reason));

            if let Some(recipient) = review_owner {
                let body = match review_state {
                    crate::team::review::ReviewQueueState::Current => format!(
                        "Review urgency: task #{} has been in review for {}s.\nTask: {}\nNext step: merge it, request rework, or escalate immediately.",
                        task.task_id, task.age_secs, current_task.title
                    ),
                    crate::team::review::ReviewQueueState::Stale(stale) => format!(
                        "Stale review normalization: task #{} has been stuck in review for {}s.\nTask: {}\nReason: {}\nNext step: {}.",
                        task.task_id,
                        task.age_secs,
                        current_task.title,
                        stale.reason,
                        stale.status_next_action()
                    ),
                };
                let _ = self.queue_daemon_message(&recipient, &body);
            }

            self.intervention_cooldowns.insert(key, Instant::now());
        }

        Ok(())
    }

    pub(super) fn maybe_auto_unblock_blocked_tasks(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let done_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|task| task.status == "done")
            .map(|task| task.id)
            .collect();
        let unblocked_tasks = tasks
            .iter()
            .filter(|task| task.status == "blocked")
            .filter(|task| !task.depends_on.is_empty())
            .filter(|task| {
                task.depends_on
                    .iter()
                    .all(|dependency| done_task_ids.contains(dependency))
            })
            .map(|task| {
                (
                    task.id,
                    task.title.clone(),
                    task.depends_on.clone(),
                    self.auto_unblock_notification_recipient(task),
                )
            })
            .collect::<Vec<_>>();

        for (task_id, title, dependencies, recipient) in unblocked_tasks {
            task_cmd::cmd_transition(&board_dir, task_id, "todo")
                .with_context(|| format!("failed to auto-unblock task #{task_id}"))?;

            let dependency_list = dependencies
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let event_role = recipient.as_deref().unwrap_or("daemon");
            self.record_task_unblocked(event_role, task_id.to_string());
            self.record_orchestrator_action(format!(
                "dependency resolution: auto-unblocked task #{} ({}) after dependencies [{}] completed",
                task_id, title, dependency_list
            ));
            info!(
                task_id,
                task_title = %title,
                dependencies = %dependency_list,
                recipient = recipient.as_deref().unwrap_or("none"),
                "auto-unblocked blocked task"
            );

            let Some(recipient) = recipient else {
                continue;
            };
            let body = format!(
                "Task #{task_id} ({title}) was automatically moved from `blocked` to `todo` because dependencies [{dependency_list}] are done."
            );
            if let Err(error) = self.queue_daemon_message(&recipient, &body) {
                warn!(
                    task_id,
                    to = %recipient,
                    error = %error,
                    "failed to notify auto-unblocked task recipient"
                );
            }
        }

        Ok(())
    }

    pub(super) fn manager_for_member_name(&self, member_name: &str) -> Option<&str> {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| member.reports_to.as_deref())
    }

    pub(super) fn auto_unblock_notification_recipient(
        &self,
        task: &crate::task::Task,
    ) -> Option<String> {
        task.claimed_by
            .as_deref()
            .filter(|owner| {
                self.config
                    .members
                    .iter()
                    .any(|member| member.name == *owner)
            })
            .map(str::to_string)
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| member.role_type == RoleType::Manager)
                    .map(|member| member.name.clone())
            })
    }

    fn aging_alert_on_cooldown(&self, key: &str) -> bool {
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        self.intervention_cooldowns
            .get(key)
            .is_some_and(|fired_at| fired_at.elapsed() < cooldown)
    }

    pub(super) fn maybe_detect_pipeline_starvation(&mut self) -> Result<()> {
        let Some(threshold) = self
            .config
            .team_config
            .workflow_policy
            .pipeline_starvation_threshold
        else {
            self.pipeline_starvation_fired = false;
            return Ok(());
        };

        // Already fired — stay suppressed until condition fully clears
        if self.pipeline_starvation_fired {
            // Only reset when enough unclaimed work exists for all idle engineers
            let board_dir = self
                .config
                .project_root
                .join(".batty")
                .join("team_config")
                .join("board");
            let all_tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
            let unclaimed_todo = all_tasks
                .iter()
                .filter(|t| matches!(t.status.as_str(), "todo" | "backlog"))
                .filter(|t| t.claimed_by.is_none())
                .count();
            let truly_idle = self.truly_idle_engineer_count(&all_tasks);
            if truly_idle == 0 || unclaimed_todo > truly_idle {
                self.pipeline_starvation_fired = false;
                self.pipeline_starvation_last_fired = None;
            } else {
                return Ok(());
            }
        }

        // Hard cooldown: never fire more than once per 5 minutes
        const STARVATION_COOLDOWN: Duration = Duration::from_secs(300);
        if let Some(last) = self.pipeline_starvation_last_fired {
            if last.elapsed() < STARVATION_COOLDOWN {
                return Ok(());
            }
        }

        // Suppress if manager has been actively working for less than 10 minutes.
        // Previously this was an unconditional suppression, causing permanent
        // deadlock when the shim state classifier got stuck on "working".
        const MANAGER_WORKING_GRACE: Duration = Duration::from_secs(600);
        let manager_recently_working = self.config.members.iter().any(|m| {
            m.role_type == RoleType::Manager
                && self.states.get(&m.name) == Some(&MemberState::Working)
                && self
                    .shim_handles
                    .get(&m.name)
                    .map(|handle| {
                        handle.secs_since_state_change() < MANAGER_WORKING_GRACE.as_secs()
                    })
                    // For non-shim agents, check if idle_started_at is absent
                    // (meaning they transitioned to working recently)
                    .unwrap_or_else(|| !self.idle_started_at.contains_key(&m.name))
        });
        if manager_recently_working {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let all_tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let idle_count = self.truly_idle_engineer_count(&all_tasks);
        if idle_count == 0 {
            return Ok(());
        }

        let todo_count = all_tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "backlog"))
            .filter(|task| task.claimed_by.is_none())
            .count();

        let deficit = idle_count.saturating_sub(todo_count);
        if todo_count >= idle_count || deficit < threshold {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let architects: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .collect();
        if architects.is_empty() {
            return Ok(());
        }

        let message =
            format!("Pipeline running dry: {idle_count} idle engineers, {todo_count} todo tasks.");
        for architect in &architects {
            let visible_sender = self.automation_sender_for(architect);
            let inbox_msg = inbox::InboxMessage::new_send(&visible_sender, architect, &message);
            inbox::deliver_to_inbox(&inbox_root, &inbox_msg)?;
        }
        self.emit_event(TeamEvent::pipeline_starvation_detected(
            idle_count, todo_count,
        ));
        self.pipeline_starvation_fired = true;
        self.pipeline_starvation_last_fired = Some(Instant::now());
        Ok(())
    }

    pub(super) fn tact_check(&mut self) -> Result<()> {
        let base_cooldown = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .planning_cycle_cooldown_secs,
        );
        // #681: when recent planning cycles have produced zero new tasks,
        // extend the cooldown linearly (1x → 6x) so a stuck board doesn't
        // burn orchestrator tokens with empty cycles every few minutes.
        // Resets to 1x as soon as a cycle creates any tasks.
        let backoff_multiplier = 1 + self.planning_cycle_consecutive_empty.min(5);
        let cooldown = base_cooldown * backoff_multiplier;

        let Some(architect) = self
            .config
            .members
            .iter()
            .find(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
        else {
            return Ok(());
        };

        let board_dir = self.board_dir();
        let board_tasks =
            crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap_or_default();
        let idle_engineer_count = self.truly_idle_engineer_count(&board_tasks);
        let dispatchable_task_count =
            crate::team::tact::dispatchable_task_count(&board_dir, &self.config.members)?;
        if !crate::team::tact::should_trigger(idle_engineer_count, dispatchable_task_count) {
            return Ok(());
        }
        if !crate::team::tact::planning_cycle_ready(
            self.planning_cycle_active,
            self.planning_cycle_last_fired,
            cooldown,
            idle_engineer_count,
            dispatchable_task_count,
        ) {
            return Ok(());
        }
        let board_summary = format!(
            "todo={}, backlog={}, in-progress={}, review={}, done={}, idle_engineers={}, dispatchable_tasks={}",
            board_tasks
                .iter()
                .filter(|task| task.status == "todo")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "backlog")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "in-progress")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "review")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "done")
                .count(),
            idle_engineer_count,
            dispatchable_task_count
        );
        let recent_completions = crate::team::events::read_events(&crate::team::team_events_path(
            &self.config.project_root,
        ))?
        .into_iter()
        .rev()
        .filter(|event| event.event == "task_completed")
        .take(5)
        .map(|event| match (event.role, event.task) {
            (Some(role), Some(task)) => format!("{role} completed task #{task}"),
            (Some(role), None) => format!("{role} reported a completion"),
            _ => "recent completion recorded".to_string(),
        })
        .collect::<Vec<_>>();
        let (roadmap_context, project_goals) = planning_prompt_context(&self.config.project_root);
        let tact_prompt = crate::team::tact::TactPrompt {
            board_summary: board_summary.clone(),
            recent_completions: recent_completions.clone(),
            roadmap_priorities: roadmap_context.clone(),
            idle_count: idle_engineer_count,
            dispatchable_count: dispatchable_task_count,
        };
        let prompt = crate::team::tact::compose_planning_prompt(
            idle_engineer_count,
            &board_summary,
            &recent_completions,
            &roadmap_context,
            &project_goals,
            &self.config.team_config.name,
        );
        let body = format!(
            "HIGH PRIORITY: planning cycle triggered because idle engineers outnumber dispatchable tasks.\n\n{prompt}"
        );

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let sender = self.automation_sender_for(&architect);
        let message = inbox::InboxMessage::new_send(&sender, &architect, &body);
        inbox::deliver_to_inbox(&inbox_root, &message)?;
        self.record_message_routed(&sender, &architect);
        self.record_tact_cycle_triggered(&architect, idle_engineer_count as u32, &board_summary);

        self.planning_cycle_last_fired = Some(Instant::now());
        self.planning_cycle_active = true;
        // #687 followup: persist immediately so a daemon restart within the
        // heartbeat window (5 min) doesn't fire a duplicate planning cycle
        // at the architect. Without this, hot-reload or manual restart
        // seconds after a cycle fires sends the architect two prompts for
        // the same "pipeline is dry" state, burning orchestrator tokens.
        if let Err(error) = self.persist_runtime_state(false) {
            warn!(error = %error, "failed to persist daemon state after planning cycle fire");
        }
        info!(
            architect,
            idle_engineers = idle_engineer_count,
            dispatchable_tasks = dispatchable_task_count,
            "triggered planning cycle"
        );
        self.record_orchestrator_action(format!(
            "planning: triggered planning cycle for {} (idle={}, dispatchable={})\n{}",
            architect,
            idle_engineer_count,
            dispatchable_task_count,
            crate::team::tact::compose_prompt(&tact_prompt)
        ));
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn maybe_trigger_planning_cycle(&mut self) -> Result<()> {
        self.tact_check()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn handle_planning_response(&mut self, response: &str) -> Result<usize> {
        let architect = self
            .config
            .members
            .iter()
            .find(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .unwrap_or_else(|| "architect".to_string());
        let latency_secs = self
            .planning_cycle_last_fired
            .map(|instant| instant.elapsed().as_secs())
            .unwrap_or(0);
        let specs = crate::team::tact::parser::parse_planning_response(response);
        let board_dir = self.board_dir();
        let result = crate::team::tact::create_board_tasks(&specs, &board_dir)
            .map(|created: Vec<u32>| created.len());

        self.planning_cycle_active = false;

        match result {
            Ok(created) => {
                // #681: track empty vs productive cycles so `tact_check` can
                // back off the cadence when the architect keeps returning
                // zero new tasks (blocked board, no dispatchable work).
                if created == 0 {
                    self.planning_cycle_consecutive_empty =
                        self.planning_cycle_consecutive_empty.saturating_add(1);
                    // #688: when a cycle is empty, reset the cooldown anchor to
                    // "now" so the next cycle is gated on time-since-completion,
                    // not time-since-fire. Without this a slow (10-min) empty
                    // cycle exactly hits the 2× cooldown boundary the moment
                    // it completes and a fresh cycle fires within the same tick.
                    // Productive cycles (created > 0) leave last_fired alone so
                    // the architect can keep planning as fast as the pipeline needs.
                    self.planning_cycle_last_fired = Some(Instant::now());
                } else {
                    self.planning_cycle_consecutive_empty = 0;
                }
                self.record_tact_tasks_created(
                    &architect,
                    created as u32,
                    latency_secs,
                    true,
                    None,
                );
                info!(
                    created,
                    consecutive_empty = self.planning_cycle_consecutive_empty,
                    "applied planning cycle response"
                );
                self.record_orchestrator_action(format!(
                    "planning: applied planning response and created {} tasks",
                    created
                ));
                Ok(created)
            }
            Err(error) => {
                self.record_tact_tasks_created(
                    &architect,
                    0,
                    latency_secs,
                    false,
                    Some(&error.to_string()),
                );
                Err(error)
            }
        }
    }

    /// Count engineers that are tmux-idle AND have no active board items.
    pub(super) fn truly_idle_engineer_count(&self, all_tasks: &[crate::task::Task]) -> usize {
        let engineers_with_active_items: std::collections::HashSet<String> = all_tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "in-progress" | "review"))
            .filter_map(|task| task.claimed_by.as_ref())
            .map(|name| name.trim_start_matches('@').to_string())
            .collect();

        self.idle_engineer_names()
            .into_iter()
            .filter(|name| !engineers_with_active_items.contains(name))
            .count()
    }

    pub(super) fn member_worktree_context(
        &self,
        member_name: &str,
    ) -> Option<MemberWorktreeContext> {
        if !self.member_uses_worktrees(member_name) {
            return None;
        }
        let worktree_path = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        if !worktree_path.exists() {
            return None;
        }

        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&worktree_path)
            .output()
            .ok()
            .and_then(|output| {
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            })
            .filter(|branch| !branch.is_empty());

        Some(MemberWorktreeContext {
            path: worktree_path,
            branch,
        })
    }

    /// Detect engineer worktrees still on branches that have been merged to main.
    /// For idle engineers with no active task, auto-reset to their base branch.
    pub(super) fn maybe_reconcile_stale_worktrees(&mut self) -> Result<()> {
        if !self.is_git_repo && !self.is_multi_repo {
            return Ok(());
        }
        let tasks_dir = self.board_dir().join("tasks");
        let board_tasks = if tasks_dir.exists() {
            crate::task::load_tasks_from_dir(&tasks_dir)?
        } else {
            Vec::new()
        };

        let engineers: Vec<(String, bool)> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| {
                let is_idle = self.states.get(&m.name) == Some(&MemberState::Idle);
                (m.name.clone(), is_idle)
            })
            .collect();

        for (engineer, is_idle) in engineers {
            if !is_idle {
                continue;
            }
            if self.active_tasks.contains_key(&engineer) {
                continue;
            }

            let worktree_dir = self.worktree_dir(&engineer);
            if !worktree_dir.exists() {
                continue;
            }

            let branch = match current_worktree_branch(&worktree_dir) {
                Ok(b) => b,
                Err(_) => continue,
            };

            let base_branch = engineer_base_branch_name(&engineer);
            if branch == "HEAD" {
                continue;
            }

            let authorized_tasks: Vec<&crate::task::Task> = board_tasks
                .iter()
                .filter(|task| {
                    task.claimed_by.as_deref() == Some(engineer.as_str())
                        && super::interventions::task_needs_owned_intervention(task.status.as_str())
                })
                .collect();

            if branch == base_branch {
                match crate::team::task_loop::refresh_engineer_worktree(
                    &self.config.project_root,
                    &worktree_dir,
                    &base_branch,
                    &self.config.project_root.join(".batty").join("team_config"),
                ) {
                    Ok(crate::team::task_loop::WorktreeRefreshAction::Rebased)
                    | Ok(crate::team::task_loop::WorktreeRefreshAction::Reset) => {
                        info!(
                            engineer = %engineer,
                            branch = %base_branch,
                            "auto-refreshed engineer worktree after main advanced"
                        );
                        self.emit_event(TeamEvent::worktree_refreshed(
                            &engineer,
                            "rebased clean base worktree onto main",
                        ));
                        self.record_orchestrator_action(format!(
                            "worktree: auto-rebased clean base worktree for {engineer} onto main"
                        ));
                    }
                    Ok(crate::team::task_loop::WorktreeRefreshAction::Unchanged)
                    | Ok(crate::team::task_loop::WorktreeRefreshAction::SkippedDirty) => {}
                    Err(error) => warn!(
                        engineer = %engineer,
                        branch = %base_branch,
                        error = %error,
                        "worktree refresh after main advance failed"
                    ),
                }
                continue;
            }

            let completed_task_for_branch = board_tasks.iter().find(|task| {
                task.claimed_by.as_deref() == Some(engineer.as_str())
                    && task.status == "done"
                    && authoritative_task_branch(&engineer, task) == branch
            });

            if is_managed_task_branch(&engineer, &branch)
                && authorized_tasks
                    .iter()
                    .all(|task| authoritative_task_branch(&engineer, task) != branch)
            {
                self.maybe_reset_engineer_to_safe_branch(
                    &engineer,
                    &branch,
                    &authorized_tasks,
                    completed_task_for_branch,
                    "board no longer authorizes current branch",
                );
                continue;
            }

            let merged = match branch_is_merged_into(&self.config.project_root, &branch, "main") {
                Ok(m) => m,
                Err(_) => continue,
            };

            if !merged {
                continue;
            }

            // SAFETY: never reset a worktree that has work ahead of main.
            // During stop/start cycles, active_tasks can be empty momentarily
            // while the engineer still has unmerged commits.
            if let Ok(ahead) = crate::worktree::commits_ahead(&worktree_dir, "main") {
                if ahead > 0 {
                    debug!(
                        engineer = %engineer,
                        branch = %branch,
                        ahead,
                        "worktree has commits ahead of main; skipping reconciliation"
                    );
                    continue;
                }
            }

            match reset_claimed_worktree_to_base(&worktree_dir, &base_branch) {
                Ok(reset_reason) if reset_reason.reset_performed() => {
                    info!(
                        engineer = %engineer,
                        stale_branch = %branch,
                        reset_to = %base_branch,
                        reset_reason = reset_reason.as_str(),
                        "auto-reconciled stale worktree"
                    );
                    self.emit_event(TeamEvent::worktree_reconciled(&engineer, &branch));
                    self.record_orchestrator_action(format!(
                        "worktree: auto-reconciled {engineer} from stale branch '{branch}' to '{base_branch}' ({})",
                        reset_reason.as_str()
                    ));
                }
                Ok(reset_reason) => {
                    debug!(
                        engineer = %engineer,
                        branch = %branch,
                        reset_reason = reset_reason.as_str(),
                        "skipping worktree reconciliation"
                    );
                }
                Err(error) => {
                    warn!(
                        engineer = %engineer,
                        branch = %branch,
                        error = %error,
                        "worktree reconciliation failed"
                    );
                    continue;
                }
            }
        }

        Ok(())
    }

    /// Rotate the board if enough time has passed.
    ///
    /// When using kanban-md (board/ directory), rotation is not needed — each
    /// task is an individual file. Only rotates the legacy plain kanban.md.
    pub(super) fn maybe_rotate_board(&mut self) -> Result<()> {
        // Check every 10 minutes
        if self.last_board_rotation.elapsed() < Duration::from_secs(600) {
            return Ok(());
        }

        self.last_board_rotation = Instant::now();

        let config_dir = self.config.project_root.join(".batty").join("team_config");

        // kanban-md uses a board/ directory — no rotation needed
        let board_dir = config_dir.join("board");
        if board_dir.is_dir() {
            return Ok(());
        }

        // Legacy plain kanban.md — rotate done items
        let kanban_path = config_dir.join("kanban.md");
        let archive_path = config_dir.join("kanban-archive.md");

        if kanban_path.exists() {
            match board::rotate_done_items(
                &kanban_path,
                &archive_path,
                self.config.team_config.board.rotation_threshold,
            ) {
                Ok(rotated) if rotated > 0 => {
                    info!(rotated, "board rotation completed");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "board rotation failed");
                }
            }
        }

        Ok(())
    }

    /// Periodically archive done tasks that exceed the configured age threshold.
    ///
    /// Rate-limited to run at most once per 60 seconds. Disabled when
    /// `auto_archive_done_after_secs` is `None` or `0`.
    pub(super) fn maybe_auto_archive(&mut self) -> Result<()> {
        // Rate-limit to once per minute
        if self.last_auto_archive.elapsed() < Duration::from_secs(60) {
            return Ok(());
        }
        self.last_auto_archive = Instant::now();

        let threshold_secs = match self
            .config
            .team_config
            .workflow_policy
            .auto_archive_done_after_secs
        {
            Some(0) | None => return Ok(()),
            Some(secs) => secs,
        };

        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.is_dir() {
            return Ok(());
        }

        let max_age = Duration::from_secs(threshold_secs);
        let old_done = board::done_tasks_older_than(&board_dir, max_age)?;
        if old_done.is_empty() {
            return Ok(());
        }

        let summary = board::archive_tasks(&board_dir, &old_done, false)?;
        if summary.archived_count > 0 {
            for task in &old_done {
                self.record_board_task_archived(task.id, task.claimed_by.as_deref());
            }
            info!(
                archived = summary.archived_count,
                threshold_secs, "auto-archived done tasks"
            );
            self.record_orchestrator_action(format!(
                "auto-archive: archived {} done tasks older than {}s",
                summary.archived_count, threshold_secs
            ));
        }

        Ok(())
    }

    pub(super) fn maybe_recycle_cron_tasks(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let recycled = super::super::task_loop::recycle_cron_tasks(&board_dir)?;
        for (task_id, cron_expr) in recycled {
            self.emit_event(TeamEvent::task_recycled(task_id, &cron_expr));
            self.record_orchestrator_action(format!(
                "cron: recycled task #{task_id} (schedule: {cron_expr}) back to todo"
            ));
        }
        Ok(())
    }

    pub(super) fn maybe_generate_retrospective(&mut self) -> Result<()> {
        let Some(stats) = super::super::retrospective::should_generate_retro(
            &self.config.project_root,
            self.retro_generated,
            self.config.team_config.retro_min_duration_secs,
        )?
        else {
            return Ok(());
        };

        let report_path =
            super::super::retrospective::generate_retrospective(&self.config.project_root, &stats)?;
        self.retro_generated = true;
        self.record_retro_generated();
        info!(path = %report_path.display(), "retrospective generated");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InProgressTaskLiveness {
    dirty_worktree: bool,
    output_growth: bool,
    recent_claim_progress: bool,
    fresh_claim_grace: bool,
    live_progress_type: Option<&'static str>,
}

impl InProgressTaskLiveness {
    fn has_live_progress(self) -> bool {
        self.fresh_claim_grace
            || self.recent_claim_progress
            || self.output_growth
            || matches!(self.live_progress_type, Some("commit"))
    }
}

fn aging_cooldown_key(kind: &str, task_id: u32) -> String {
    format!("aging::{kind}::{task_id}")
}

fn first_manager_name(members: &[MemberInstance]) -> Option<String> {
    members
        .iter()
        .find(|member| member.role_type == RoleType::Manager)
        .map(|member| member.name.clone())
}

fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn latest_commit_timestamp(work_dir: &std::path::Path) -> Option<DateTime<Utc>> {
    if crate::team::git_cmd::rev_list_count(work_dir, "main..HEAD")
        .ok()
        .is_none_or(|count| count == 0)
    {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["log", "-1", "--format=%cI"])
        .current_dir(work_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_rfc3339_utc(stdout.trim())
}

fn has_claim_progress_worktree_changes(work_dir: &std::path::Path) -> bool {
    crate::team::git_cmd::status_porcelain(work_dir)
        .ok()
        .map(|status| {
            status.lines().any(|line| {
                let path = line.get(3..).unwrap_or("").trim();
                if path.starts_with(".batty/") || path.starts_with(".cargo/") {
                    return false;
                }
                !path.starts_with(".batty/team_config/")
            })
        })
        .unwrap_or(false)
}

fn task_claim_progress_type(
    task: &crate::task::Task,
    work_dir: &std::path::Path,
    current_output_bytes: u64,
) -> Option<&'static str> {
    let last_progress_at = task
        .last_progress_at
        .as_deref()
        .and_then(parse_rfc3339_utc)?;
    if latest_commit_timestamp(work_dir).is_some_and(|ts| ts > last_progress_at) {
        return Some("commit");
    }
    if has_claim_progress_worktree_changes(work_dir) {
        return Some("dirty_files");
    }
    if current_output_bytes > task.last_output_bytes.unwrap_or(0) {
        return Some("output");
    }
    None
}

fn task_recent_claim_progress(
    task: &crate::task::Task,
    now: DateTime<Utc>,
    progress_window: Duration,
) -> bool {
    let Some(last_progress_at) = task.last_progress_at.as_deref().and_then(parse_rfc3339_utc)
    else {
        return false;
    };
    let age_secs = now.signed_duration_since(last_progress_at).num_seconds();
    age_secs >= 0 && age_secs < progress_window.as_secs() as i64
}

fn fresh_claim_immunity_secs(progress_window: Duration) -> u64 {
    std::cmp::max(60, progress_window.as_secs() / 10)
}

fn task_in_fresh_claim_grace(
    task: &crate::task::Task,
    now: DateTime<Utc>,
    progress_window: Duration,
) -> bool {
    let Some(claimed_at) = task.claimed_at.as_deref().and_then(parse_rfc3339_utc) else {
        return false;
    };
    let age_secs = now.signed_duration_since(claimed_at).num_seconds();
    age_secs >= 0 && age_secs < fresh_claim_immunity_secs(progress_window) as i64
}

impl TeamDaemon {
    fn in_progress_task_liveness(
        &self,
        task: &crate::task::Task,
        now: DateTime<Utc>,
        progress_window: Duration,
    ) -> Option<InProgressTaskLiveness> {
        let owner = task.claimed_by.as_deref()?;
        let work_dir = self.task_progress_work_dir(task)?;
        let current_output_bytes = self
            .shim_handles
            .get(owner)
            .map(|handle| handle.output_bytes)
            .unwrap_or(0);

        Some(InProgressTaskLiveness {
            dirty_worktree: has_claim_progress_worktree_changes(&work_dir),
            output_growth: current_output_bytes > task.last_output_bytes.unwrap_or(0),
            recent_claim_progress: task_recent_claim_progress(task, now, progress_window),
            fresh_claim_grace: task_in_fresh_claim_grace(task, now, progress_window),
            live_progress_type: task_claim_progress_type(task, &work_dir, current_output_bytes),
        })
    }

    fn task_progress_work_dir(&self, task: &crate::task::Task) -> Option<std::path::PathBuf> {
        if let Some(worktree_path) = task.worktree_path.as_deref() {
            let path = std::path::PathBuf::from(worktree_path);
            return Some(if path.is_absolute() {
                path
            } else {
                self.config.project_root.join(path)
            });
        }

        let owner = task.claimed_by.as_deref()?;
        let uses_worktrees = self
            .config
            .members
            .iter()
            .find(|member| member.name == owner)
            .is_none_or(|member| member.use_worktrees);
        Some(if uses_worktrees {
            self.worktree_dir(owner)
        } else {
            self.config.project_root.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{
        ClaimedTaskBranchMismatch, active_stale_review_entries,
        select_authoritative_multi_claim_task,
    };
    use crate::team::config::RoleType;
    use crate::team::config::{BoardConfig, WorkflowMode, WorkflowPolicy};
    use crate::team::events::TeamEvent;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::task_cmd::{
        set_optional_string, set_optional_u64, update_task_frontmatter, yaml_key,
    };
    use crate::team::task_loop::{
        current_worktree_branch, engineer_base_branch_name, setup_engineer_worktree,
    };
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{
        EnvVarGuard, PATH_LOCK, TestDaemonBuilder, architect_member, engineer_member, git_ok,
        git_stdout, init_git_repo, manager_member, write_board_task_file, write_open_task_file,
        write_owned_task_file, write_owned_task_file_with_context,
    };
    use std::collections::HashMap;

    fn test_task(
        id: u32,
        status: &str,
        claimed_by: Option<&str>,
        review_owner: Option<&str>,
    ) -> crate::task::Task {
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
            review_owner: review_owner.map(str::to_string),
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
            source_path: std::path::PathBuf::new(),
        }
    }

    fn setup_fake_kanban_for_planning(
        tmp: &tempfile::TempDir,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let fake_bin = tmp.path().join("fake-bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        let log_path = tmp.path().join("kanban.log");
        let script = fake_bin.join("kanban-md");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash
set -euo pipefail
printf '%s\\n' \"$*\" >> '{}'
if [ \"$1\" != \"create\" ]; then
  exit 1
fi
title=\"$2\"
body=\"\"
priority=\"high\"
tags=\"\"
depends_on=\"\"
shift 2
while [ $# -gt 0 ]; do
  case \"$1\" in
    --body) body=\"$2\"; shift 2 ;;
    --priority) priority=\"$2\"; shift 2 ;;
    --tags) tags=\"$2\"; shift 2 ;;
    --depends-on) depends_on=\"$2\"; shift 2 ;;
    --dir) board_dir=\"$2\"; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p \"$board_dir/tasks\"
count=$(find \"$board_dir/tasks\" -maxdepth 1 -name '*.md' | wc -l | tr -d ' ')
id=$((count + 1))
printf -- '---\nid: %s\ntitle: %s\nstatus: todo\npriority: %s\n' \"$id\" \"$title\" \"$priority\" > \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
if [ -n \"$tags\" ]; then
  printf 'tags: [%s]\n' \"$tags\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
fi
if [ -n \"$depends_on\" ]; then
  printf 'depends_on: [%s]\n' \"$depends_on\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
fi
printf -- '---\n\n%s\n' \"$body\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
printf 'Created task #%s\n' \"$id\"
",
                log_path.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (fake_bin, log_path)
    }

    fn planning_test_daemon(tmp: &tempfile::TempDir, cooldown_secs: u64) -> TeamDaemon {
        let board_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&board_dir).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                engineer_member("eng-1", Some("architect"), false),
                engineer_member("eng-2", Some("architect"), false),
                engineer_member("eng-3", Some("architect"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                planning_cycle_cooldown_secs: cooldown_secs,
                ..WorkflowPolicy::default()
            })
            .states(HashMap::from([
                ("architect".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
                ("eng-3".to_string(), MemberState::Idle),
            ]))
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon
    }

    fn planning_inbox_messages(tmp: &tempfile::TempDir) -> Vec<inbox::InboxMessage> {
        inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "architect").unwrap()
    }

    fn write_planning_docs(tmp: &tempfile::TempDir) {
        let planning_dir = tmp.path().join("planning");
        std::fs::create_dir_all(&planning_dir).unwrap();
        std::fs::write(
            planning_dir.join("roadmap.md"),
            "# Roadmap\n\nPhase 2: Productionize tact planning.\n- Auto-dispatch new tact tasks.\n",
        )
        .unwrap();
        std::fs::write(
            planning_dir.join("architecture.md"),
            "# Architecture\n\nGoal: Keep idle engineers fed with executable work.\n- Prefer small end-to-end automation loops.\n",
        )
        .unwrap();
    }

    fn board_task_path(project_root: &Path, task_id: u32) -> std::path::PathBuf {
        crate::task::find_task_path_by_id(
            &project_root
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
            task_id,
        )
        .unwrap()
    }

    const SINGLE_TASK_RESPONSE: &str = r#"---
title: "Add planning telemetry"
priority: high
tags: [tact, telemetry]
---
Record planning cycle events in the orchestrator log and add assertions.
"#;

    const THREE_TASK_RESPONSE: &str = r#"---
title: "Add planning telemetry"
priority: high
tags: [tact, telemetry]
---
Record planning cycle events in the orchestrator log.
---
title: "Persist planning outputs"
priority: medium
depends_on: [1]
tags: [tact, board]
---
Create board tasks from planning responses.
---
title: "Backfill planning tests"
priority: medium
depends_on: [1, 2]
tags: [tact, tests]
---
Add end-to-end planning cycle coverage.
"#;

    const MIXED_RESPONSE: &str = r#"---
title: "Good task"
priority: high
tags: [tact]
---
Good body.
---
title: [broken
---
Bad body.
---
title: "Second good task"
priority: medium
depends_on: [1]
---
Second body.
"#;

    const RAW_LOG_RESPONSE: &str = r#"---
title: "Valid planning task"
priority: high
tags: [tact]
---
Write a real planning task body.
---
title: "Rejected raw log task"
priority: high
tags: [tact]
---
running 3144 tests
test tmux::tests::split_window_horizontal_creates_new_pane ... FAILED
thread 'tmux::tests::split_window_horizontal_creates_new_pane' panicked at src/tmux.rs:42:9
"#;

    #[test]
    fn planning_cycle_trigger_generates_prompt_in_architect_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);

        daemon.maybe_trigger_planning_cycle().unwrap();

        let messages = planning_inbox_messages(&tmp);
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .body
                .contains("HIGH PRIORITY: planning cycle triggered")
        );
        assert!(messages[0].body.contains("Idle engineers available: 3"));
        assert!(messages[0].body.contains("Expected response format:"));
        assert!(daemon.planning_cycle_active);
    }

    #[test]
    fn planning_cycle_prompt_includes_recent_completion_context() {
        let tmp = tempfile::tempdir().unwrap();
        write_event_log(
            tmp.path(),
            &[TeamEvent::task_completed("eng-2", Some("44"))],
        );

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.maybe_trigger_planning_cycle().unwrap();

        let messages = planning_inbox_messages(&tmp);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].body.contains("eng-2 completed task #44"));
    }

    #[test]
    fn planning_cycle_prompt_uses_truly_idle_engineer_count() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);

        write_board_task_file(
            tmp.path(),
            201,
            "eng-1-active-review",
            "review",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.maybe_trigger_planning_cycle().unwrap();

        let messages = planning_inbox_messages(&tmp);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].body.contains("Idle engineers available: 2"));
        assert!(messages[0].body.contains("Propose exactly 2 task(s)."));
    }

    #[test]
    fn planning_cycle_does_not_trigger_when_dispatchable_work_is_sufficient() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);
        write_board_task_file(tmp.path(), 301, "ready-a", "todo", None, &[], None);
        write_board_task_file(tmp.path(), 302, "ready-b", "todo", None, &[], None);
        write_board_task_file(tmp.path(), 303, "ready-c", "todo", None, &[], None);

        daemon.maybe_trigger_planning_cycle().unwrap();

        assert!(planning_inbox_messages(&tmp).is_empty());
        assert!(!daemon.planning_cycle_active);
    }

    #[test]
    fn planning_cycle_triggers_when_todo_exists_but_no_work_is_dispatchable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);
        write_board_task_file(
            tmp.path(),
            401,
            "blocked-by-dependency",
            "todo",
            None,
            &[999],
            None,
        );

        daemon.maybe_trigger_planning_cycle().unwrap();

        let messages = planning_inbox_messages(&tmp);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].body.contains("dispatchable_tasks=0"));
        assert!(messages[0].body.contains("Propose exactly 3 task(s)."));
        assert!(daemon.planning_cycle_active);
    }

    #[test]
    fn planning_cycle_round_trip_creates_board_tasks_and_resets_active_flag() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.maybe_trigger_planning_cycle().unwrap();
        let created = daemon
            .handle_planning_response(THREE_TASK_RESPONSE)
            .unwrap();

        assert_eq!(created, 3);
        assert!(!daemon.planning_cycle_active);

        let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].title, "Add planning telemetry");
        assert_eq!(tasks[1].title, "Persist planning outputs");
        assert_eq!(tasks[2].title, "Backfill planning tests");
        assert_eq!(tasks[0].tags, vec!["tact", "telemetry"]);
        assert_eq!(tasks[1].depends_on, vec![1]);
        assert_eq!(tasks[1].tags, vec!["tact", "board"]);
        assert_eq!(tasks[2].depends_on, vec![1, 2]);
        assert_eq!(tasks[2].tags, vec!["tact", "tests"]);

        let orchestrator_log =
            std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path()))
                .unwrap_or_default();
        assert!(orchestrator_log.contains("planning: triggered planning cycle"));
        assert!(
            orchestrator_log.contains("planning: applied planning response and created 3 tasks")
        );

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "tact_cycle_triggered"
                && event.role.as_deref() == Some("architect")
                && event.working_members == Some(3)
        }));
        assert!(events.iter().any(|event| {
            event.event == "tact_tasks_created"
                && event.role.as_deref() == Some("architect")
                && event.restart_count == Some(3)
                && event.reason.as_deref() == Some("success")
        }));
    }

    #[test]
    fn planning_round_trip_with_malformed_blocks_keeps_good_tasks() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.planning_cycle_active = true;

        let created = daemon.handle_planning_response(MIXED_RESPONSE).unwrap();
        assert_eq!(created, 2);
        assert!(!daemon.planning_cycle_active);
    }

    #[test]
    fn planning_round_trip_rejects_raw_log_generated_tasks() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.planning_cycle_active = true;

        let created = daemon.handle_planning_response(RAW_LOG_RESPONSE).unwrap();
        assert_eq!(created, 1);
        assert!(!daemon.planning_cycle_active);

        let tasks = crate::task::load_tasks_from_dir(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Valid planning task");
    }

    #[test]
    fn planning_response_empty_creates_zero_tasks_and_resets_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.planning_cycle_active = true;

        let created = daemon.handle_planning_response("").unwrap();
        assert_eq!(created, 0);
        assert!(!daemon.planning_cycle_active);

        let orchestrator_log =
            std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path()))
                .unwrap_or_default();
        assert!(
            orchestrator_log.contains("planning: applied planning response and created 0 tasks")
        );
    }

    #[test]
    fn planning_response_missing_board_dir_returns_graceful_error_and_resets_cycle() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let mut daemon = planning_test_daemon(&tmp, 300);
        std::fs::remove_dir_all(daemon.board_dir()).unwrap();
        daemon.planning_cycle_active = true;

        let error = daemon
            .handle_planning_response(SINGLE_TASK_RESPONSE)
            .unwrap_err();
        assert!(error.to_string().contains("board directory does not exist"));
        assert!(!daemon.planning_cycle_active);
    }

    #[test]
    fn planning_response_missing_kanban_binary_returns_clear_error() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _path_guard = EnvVarGuard::set("PATH", tmp.path().display().to_string().as_str());

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.planning_cycle_active = true;

        let error = daemon
            .handle_planning_response(SINGLE_TASK_RESPONSE)
            .unwrap_err();
        let detail = error.to_string();
        assert!(
            detail.contains("failed to create board task") || detail.contains("failed to execute")
        );
        assert!(!daemon.planning_cycle_active);
    }

    #[test]
    fn planning_response_double_apply_is_graceful() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon.planning_cycle_active = true;

        assert_eq!(
            daemon
                .handle_planning_response(SINGLE_TASK_RESPONSE)
                .unwrap(),
            1
        );
        assert_eq!(
            daemon
                .handle_planning_response(SINGLE_TASK_RESPONSE)
                .unwrap(),
            0
        );

        let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(!daemon.planning_cycle_active);
    }

    #[test]
    fn planning_cycle_cooldown_blocks_immediate_retrigger() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);

        daemon.maybe_trigger_planning_cycle().unwrap();
        daemon.planning_cycle_active = false;
        daemon.maybe_trigger_planning_cycle().unwrap();

        assert_eq!(planning_inbox_messages(&tmp).len(), 1);
    }

    #[test]
    fn planning_cycle_retriggers_after_cooldown_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = planning_test_daemon(&tmp, 300);

        daemon.maybe_trigger_planning_cycle().unwrap();
        daemon.planning_cycle_active = false;
        daemon.planning_cycle_last_fired = Some(Instant::now() - Duration::from_secs(301));
        daemon.maybe_trigger_planning_cycle().unwrap();

        assert_eq!(planning_inbox_messages(&tmp).len(), 2);
    }

    #[test]
    fn planning_prompt_and_parser_round_trip_sample_response() {
        let prompt = crate::team::tact::compose_planning_prompt(
            3,
            "todo=0 backlog=2 in-progress=1 review=0 done=3 idle_engineers=3",
            &["eng-1 completed task #44".to_string()],
            &["Improve planning automation".to_string()],
            &["Improve planning automation".to_string()],
            "Batty",
        );

        let specs = crate::team::tact::parse_planning_response(THREE_TASK_RESPONSE);
        assert!(prompt.contains("Improve planning automation"));
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[2].depends_on, vec![1, 2]);
    }

    #[test]
    #[serial_test::serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn planning_cycle_end_to_end_includes_context_and_dispatches_created_work() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        write_planning_docs(&tmp);
        let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let mut daemon = planning_test_daemon(&tmp, 300);
        daemon
            .config
            .team_config
            .workflow_policy
            .pipeline_starvation_threshold = Some(1);
        daemon
            .config
            .team_config
            .board
            .dispatch_stabilization_delay_secs = 0;
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        for engineer in ["eng-1", "eng-2", "eng-3"] {
            daemon.idle_started_at.insert(
                engineer.to_string(),
                Instant::now() - Duration::from_secs(60),
            );
        }

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(daemon.pipeline_starvation_fired);

        daemon.maybe_trigger_planning_cycle().unwrap();

        let messages = planning_inbox_messages(&tmp);
        assert_eq!(messages.len(), 2);
        let planning_prompt = messages
            .iter()
            .find(|message| message.body.contains("Expected response format:"))
            .map(|message| message.body.as_str())
            .expect("expected planning prompt in architect inbox");
        assert!(planning_prompt.contains("Phase 2: Productionize tact planning."));
        assert!(planning_prompt.contains("Auto-dispatch new tact tasks."));
        assert!(planning_prompt.contains("Goal: Keep idle engineers fed with executable work."));

        let created = daemon
            .handle_planning_response(THREE_TASK_RESPONSE)
            .unwrap();
        assert_eq!(created, 3);

        daemon.maybe_auto_dispatch().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
        assert!(
            tasks
                .iter()
                .any(|task| task.status == "in-progress" && task.claimed_by.is_some()),
            "expected at least one created tact task to be dispatched"
        );
    }

    #[test]
    fn maybe_auto_unblock_moves_blocked_task_to_todo_and_notifies_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
        let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
        let board_tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        write_board_task_file(tmp.path(), 11, "dep-a", "done", None, &[], None);
        write_board_task_file(tmp.path(), 12, "dep-b", "done", None, &[], None);
        write_board_task_file(
            tmp.path(),
            13,
            "blocked-task",
            "blocked",
            Some("eng-1"),
            &[11, 12],
            Some("waiting on dependencies"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
        let task = tasks.iter().find(|task| task.id == 13).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.blocked_on.is_none());
        assert!(task.blocked.is_none());

        let pending = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #13 (blocked-task)"));
        assert!(
            pending[0]
                .body
                .contains("automatically moved from `blocked` to `todo`")
        );
        assert!(pending[0].body.contains("[11, 12]"));

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_unblocked"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("13")
        }));
    }

    #[test]
    fn maybe_auto_unblock_notifies_manager_when_task_is_unowned() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
        let inbox_root = inbox::inboxes_root(tmp.path());
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        write_board_task_file(tmp.path(), 21, "dep-a", "done", None, &[], None);
        write_board_task_file(
            tmp.path(),
            22,
            "blocked-task",
            "blocked",
            None,
            &[21],
            Some("waiting on dependencies"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #22 (blocked-task)"));

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_unblocked"
                && event.role.as_deref() == Some("manager")
                && event.task.as_deref() == Some("22")
        }));
    }

    #[test]
    fn maybe_auto_unblock_leaves_unresolved_or_dependency_free_tasks_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
        let board_tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        write_board_task_file(tmp.path(), 31, "dep-a", "done", None, &[], None);
        write_board_task_file(tmp.path(), 32, "dep-b", "review", None, &[], None);
        write_board_task_file(
            tmp.path(),
            33,
            "blocked-partial",
            "blocked",
            None,
            &[31, 32],
            Some("waiting on dependencies"),
        );
        write_board_task_file(
            tmp.path(),
            34,
            "blocked-no-deps",
            "blocked",
            None,
            &[],
            Some("manual hold"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
        let partial = tasks.iter().find(|task| task.id == 33).unwrap();
        assert_eq!(partial.status, "blocked");
        assert_eq!(
            partial.blocked_on.as_deref(),
            Some("waiting on dependencies")
        );

        let no_deps = tasks.iter().find(|task| task.id == 34).unwrap();
        assert_eq!(no_deps.status, "blocked");
        assert_eq!(no_deps.blocked_on.as_deref(), Some("manual hold"));

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(pending.is_empty());

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.task.as_deref(), Some("33" | "34")))
        );
    }

    #[test]
    fn auto_retro_fires_when_all_done() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::daemon_started(),
                TeamEvent::task_assigned("eng-1", "45"),
                TeamEvent::task_completed("eng-1", Some("45")),
                TeamEvent::daemon_stopped(),
            ],
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .orchestrator_pane(false)
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        daemon.maybe_generate_retrospective().unwrap();

        assert!(daemon.retro_generated);
        let retro_dir = tmp.path().join(".batty").join("retrospectives");
        let reports = std::fs::read_dir(&retro_dir).unwrap().count();
        assert_eq!(reports, 1);

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
        );
    }

    // ── reconcile_active_tasks ──────────────────────────────────────

    /// Regression for #630: when a task is already done/approved on
    /// the board but the engineer worktree is still tracked in the
    /// daemon's active_tasks map (the "post-approval dirty lane"
    /// case), reconcile must clear the engineer so a fresh dispatch
    /// can run. Without this, completed lanes stay parked forever
    /// waiting on a human intervention.
    #[test]
    fn reconcile_active_tasks_recovers_post_approval_dirty_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 628);

        // Task #628 is done on the board — the engineer's active_task
        // is pointing at completed work.
        write_board_task_file(
            tmp.path(),
            628,
            "completed-task",
            "done",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.reconcile_active_tasks().unwrap();

        assert!(
            !daemon.active_tasks.contains_key("eng-1"),
            "engineer must be freed from the post-approval dirty lane"
        );
    }

    #[test]
    fn reconcile_active_tasks_clears_done_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        write_board_task_file(
            tmp.path(),
            10,
            "done-task",
            "done",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.reconcile_active_tasks().unwrap();
        assert!(!daemon.active_tasks.contains_key("eng-1"));
    }

    #[test]
    fn reconcile_active_tasks_clears_archived_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        write_board_task_file(
            tmp.path(),
            10,
            "archived-task",
            "archived",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.reconcile_active_tasks().unwrap();
        assert!(!daemon.active_tasks.contains_key("eng-1"));
    }

    #[test]
    fn reconcile_active_tasks_clears_missing_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 999);

        // No task files exist at all — task 999 is missing from board
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        daemon.reconcile_active_tasks().unwrap();
        assert!(!daemon.active_tasks.contains_key("eng-1"));
    }

    #[test]
    fn reconcile_active_tasks_keeps_in_progress_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        write_board_task_file(
            tmp.path(),
            10,
            "active-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.reconcile_active_tasks().unwrap();
        assert_eq!(daemon.active_tasks.get("eng-1"), Some(&10));
    }

    #[test]
    fn reconcile_active_tasks_releases_review_tasks_without_losing_review_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        write_board_task_file(
            tmp.path(),
            10,
            "review-task",
            "review",
            Some("eng-1"),
            &[],
            None,
        );
        let task_path = board_task_path(tmp.path(), 10);
        crate::team::task_cmd::assign_task_owners(
            &daemon.board_dir(),
            10,
            Some("eng-1"),
            Some("manager"),
        )
        .unwrap();

        daemon.reconcile_active_tasks().unwrap();

        assert!(!daemon.active_tasks.contains_key("eng-1"));
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "review");
        assert!(task.claimed_by.is_none());
        assert_eq!(task.review_owner.as_deref(), Some("manager"));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("10")
                && event.reason.as_deref() == Some("release_review")
        }));
    }

    #[test]
    fn reconcile_active_tasks_releases_blocked_tasks_without_losing_block_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        write_board_task_file(
            tmp.path(),
            10,
            "blocked-task",
            "blocked",
            Some("eng-1"),
            &[],
            Some("manual provider-console token rotation"),
        );
        let task_path = board_task_path(tmp.path(), 10);

        daemon.reconcile_active_tasks().unwrap();

        assert!(!daemon.active_tasks.contains_key("eng-1"));
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "blocked");
        assert!(task.claimed_by.is_none());
        assert_eq!(
            task.blocked.as_deref(),
            Some("manual provider-console token rotation")
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("10")
                && event.reason.as_deref() == Some("release_blocked")
        }));
    }

    #[test]
    fn reconcile_active_tasks_adopts_single_board_owned_task() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);

        write_board_task_file(
            tmp.path(),
            20,
            "owned-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(20));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("20")
                && event.reason.as_deref() == Some("adopt")
        }));
    }

    #[test]
    fn reconcile_active_tasks_adopts_owned_in_progress_task_from_worktree_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "replay-owned-task");
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![manager, engineer]);
        daemon.set_member_idle("eng-1");

        write_board_task_file(
            &repo,
            10,
            "active-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-main/eng-1", &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/10")
            .unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        daemon.reconcile_active_tasks().unwrap();
        assert_eq!(daemon.active_task_id("eng-1"), Some(10));
    }

    #[test]
    fn reconcile_active_tasks_noop_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), Vec::new());
        // No active tasks — should return immediately
        daemon.reconcile_active_tasks().unwrap();
        assert!(daemon.active_tasks.is_empty());
    }

    #[test]
    fn select_authoritative_multi_claim_task_prefers_priority_then_claim_age() {
        let mut older_high = test_task(10, "in-progress", Some("eng-1"), None);
        older_high.priority = "high".to_string();
        older_high.claimed_at = Some("2026-04-14T21:06:20Z".to_string());

        let mut newer_critical = test_task(20, "in-progress", Some("eng-1"), None);
        newer_critical.priority = "critical".to_string();
        newer_critical.claimed_at = Some("2026-04-14T21:06:30Z".to_string());

        assert_eq!(
            select_authoritative_multi_claim_task(&[&older_high, &newer_critical])
                .map(|task| task.id),
            Some(20),
            "higher priority must win even when claimed later"
        );

        let mut older_critical = test_task(30, "in-progress", Some("eng-1"), None);
        older_critical.priority = "critical".to_string();
        older_critical.claimed_at = Some("2026-04-14T21:06:10Z".to_string());

        let mut newer_critical_same_priority = test_task(40, "in-progress", Some("eng-1"), None);
        newer_critical_same_priority.priority = "critical".to_string();
        newer_critical_same_priority.claimed_at = Some("2026-04-14T21:06:40Z".to_string());

        assert_eq!(
            select_authoritative_multi_claim_task(&[
                &older_critical,
                &newer_critical_same_priority
            ])
            .map(|task| task.id),
            Some(30),
            "oldest claim must win when priorities tie"
        );
    }

    #[test]
    fn reconcile_active_tasks_repairs_multiple_claimed_tasks_for_one_engineer_in_one_pass() {
        let tmp = tempfile::tempdir().unwrap();
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
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);

        write_board_task_file(
            tmp.path(),
            10,
            "old-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );
        write_board_task_file(
            tmp.path(),
            20,
            "manual-override-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        let older_task_path = board_task_path(tmp.path(), 10);
        update_task_frontmatter(&older_task_path, |mapping| {
            mapping.insert(
                yaml_key("claimed_at"),
                serde_yaml::Value::String("2026-04-14T21:06:10Z".to_string()),
            );
        })
        .unwrap();

        let newer_task_path = board_task_path(tmp.path(), 20);
        update_task_frontmatter(&newer_task_path, |mapping| {
            mapping.insert(
                yaml_key("claimed_at"),
                serde_yaml::Value::String("2026-04-14T21:06:40Z".to_string()),
            );
        })
        .unwrap();

        daemon.reconcile_active_tasks().unwrap();

        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap();
        let eng_tasks: Vec<_> = tasks
            .iter()
            .filter(|task| task.claimed_by.as_deref() == Some("eng-1"))
            .map(|task| (task.id, task.status.as_str()))
            .collect();

        assert!(eng_tasks.contains(&(10, "in-progress")));
        assert!(!eng_tasks.contains(&(20, "in-progress")));
        assert_eq!(daemon.active_task_id("eng-1"), Some(10));

        let normalized = tasks.iter().find(|task| task.id == 20).unwrap();
        assert_eq!(normalized.status, "todo");
        assert!(normalized.claimed_by.is_none());

        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let first_pass_events = crate::team::events::read_events(&events_path).unwrap();
        let first_pass_repairs = first_pass_events
            .iter()
            .filter(|event| {
                event.event == "state_reconciliation"
                    && event.role.as_deref() == Some("eng-1")
                    && event.reason.as_deref() == Some("repair")
            })
            .count();
        assert_eq!(
            first_pass_repairs, 2,
            "first pass should emit one authoritative-task repair and one excess-claim release"
        );

        daemon.reconcile_active_tasks().unwrap();

        let second_pass_events = crate::team::events::read_events(&events_path).unwrap();
        let second_pass_repairs = second_pass_events
            .iter()
            .filter(|event| {
                event.event == "state_reconciliation"
                    && event.role.as_deref() == Some("eng-1")
                    && event.reason.as_deref() == Some("repair")
            })
            .count();
        assert_eq!(
            second_pass_repairs, first_pass_repairs,
            "stable winner selection must converge without extra repair events on the next pass"
        );
        assert_eq!(daemon.active_task_id("eng-1"), Some(10));
    }

    #[test]
    fn reconcile_active_tasks_adopts_board_task_after_clearing_stale_tracked_task() {
        let tmp = tempfile::tempdir().unwrap();
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
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        write_board_task_file(
            tmp.path(),
            20,
            "replacement-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(20));
    }

    #[test]
    fn claimed_task_branch_mismatch_reports_detached_head_claimed_task() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claimed-branch-detached-head");
        let engineer = engineer_member("eng-1", Some("manager"), true);
        let manager = manager_member("manager", None);
        let daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();
        let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        git_ok(&worktree_dir, &["checkout", "--detach", &head]);

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        let mismatch = daemon
            .claimed_task_branch_mismatch("eng-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            mismatch,
            ClaimedTaskBranchMismatch {
                task_id: 42,
                expected_branch: "eng-1/42".to_string(),
                current_branch: "HEAD".to_string(),
            }
        );
    }

    #[test]
    fn claimed_task_branch_mismatch_ignores_matching_claimed_branch() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claimed-branch-match");
        let engineer = engineer_member("eng-1", Some("manager"), true);
        let manager = manager_member("manager", None);
        let daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        assert!(
            daemon
                .claimed_task_branch_mismatch("eng-1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn claimed_task_branch_mismatch_ignores_detached_head_without_claim() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claimed-branch-no-claim");
        let engineer = engineer_member("eng-1", Some("manager"), true);
        let manager = manager_member("manager", None);
        let daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        git_ok(&worktree_dir, &["checkout", "--detach", &head]);

        assert!(
            daemon
                .claimed_task_branch_mismatch("eng-1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconcile_active_tasks_repairs_clean_branch_mismatch_to_expected_branch() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile-clean-branch-repair");
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("42")
                && event.reason.as_deref() == Some("branch_repair")
        }));
        let inbox_root = inbox::inboxes_root(&repo);
        assert!(
            inbox::pending_messages(&inbox_root, "manager")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reconcile_active_tasks_preserves_dirty_work_then_repairs_branch_mismatch() {
        // Regression: previously, a dirty worktree on the wrong branch would
        // cause the reconciliation path to refuse recovery and just alert.
        // This left engineers permanently stuck on stale branches until a
        // human intervened. The fix: auto-save dirty changes as a commit on
        // the current (stale) branch first, then switch to the expected
        // branch. Dirty work is preserved in git history and the engineer
        // lands on the correct lane.
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile-stale-branch");
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
        std::fs::write(worktree_dir.join("scratch.txt"), "stale dirty work\n").unwrap();
        git_ok(&worktree_dir, &["add", "scratch.txt"]);

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        daemon.reconcile_active_tasks().unwrap();

        // Task ownership recorded.
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        // Branch was switched to the expected lane.
        assert_eq!(
            current_worktree_branch(&worktree_dir).unwrap(),
            "eng-1/42",
            "branch should have been recovered to the expected lane"
        );
        // Dirty file is now committed (not dirty, not lost). Use the same
        // semantics as production (ignores .batty/ and .cargo/ which are
        // always excluded from preservation).
        assert!(
            !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
            "worktree should have no user changes after preserve-and-recover",
        );
        // Verify the auto-save commit exists on eng-1/41 (the original branch).
        let log_on_41 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/41", "-1"]);
        assert!(
            log_on_41.contains("auto-save before branch recovery"),
            "eng-1/41 tip should be the auto-save commit, was:\n{log_on_41}",
        );
        // The originally-dirty scratch.txt should exist on eng-1/41 as a
        // committed file, not in the current worktree (eng-1/42 doesn't have it).
        let files_on_41 = git_stdout(&worktree_dir, &["ls-tree", "-r", "eng-1/41", "--name-only"]);
        assert!(
            files_on_41.contains("scratch.txt"),
            "scratch.txt should be committed on eng-1/41, files:\n{files_on_41}",
        );
        // Reconciliation event recorded as a branch_repair, not a mismatch
        // block.
        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(
            events.iter().any(|event| {
                event.event == "state_reconciliation"
                    && event.role.as_deref() == Some("eng-1")
                    && event.task.as_deref() == Some("42")
                    && event.reason.as_deref() == Some("branch_repair")
            }),
            "expected a branch_repair state_reconciliation event, got: {:?}",
            events
                .iter()
                .filter(|e| e.event == "state_reconciliation")
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn reconcile_active_tasks_self_heals_working_engineer_on_stale_branch() {
        // Regression for #666: a Working engineer stuck on the wrong branch
        // with a dirty worktree used to be skipped by the self-heal path
        // (which only ran for Idle members). That left engineers wedged
        // re-emitting "branch recovery blocked" every monitor tick with no
        // healing action. The fix: self-heal runs for Working members too,
        // auto-saving dirty work on the stale branch before switching.
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile-stale-branch-working");
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            // The key difference from the existing regression test: Working, not Idle.
            .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
        std::fs::write(worktree_dir.join("scratch.txt"), "stale working work\n").unwrap();
        git_ok(&worktree_dir, &["add", "scratch.txt"]);

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        daemon.reconcile_active_tasks().unwrap();

        // Branch recovery completed despite Working state.
        assert_eq!(
            current_worktree_branch(&worktree_dir).unwrap(),
            "eng-1/42",
            "branch should have been recovered even while Working"
        );
        // Dirty work preserved as a commit on the stale branch.
        let log_on_41 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/41", "-1"]);
        assert!(
            log_on_41.contains("auto-save before branch recovery"),
            "eng-1/41 tip should be the auto-save commit while Working, was:\n{log_on_41}",
        );
    }

    #[test]
    fn reconcile_active_tasks_ignores_gitignored_runtime_noise() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile-runtime-noise");
        std::fs::write(repo.join(".gitignore"), ".batty-target/\n").unwrap();
        git_ok(&repo, &["add", ".gitignore"]);
        git_ok(&repo, &["commit", "-m", "ignore runtime target"]);
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
        std::fs::create_dir_all(worktree_dir.join(".batty-target").join("debug")).unwrap();
        std::fs::write(
            worktree_dir
                .join(".batty-target")
                .join("debug")
                .join("build.log"),
            "transient\n",
        )
        .unwrap();

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
        assert!(
            !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
            "runtime-only .batty-target noise should not survive reconciliation as user dirt"
        );
        let inbox_root = inbox::inboxes_root(&repo);
        assert!(
            inbox::pending_messages(&inbox_root, "manager")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reconcile_active_tasks_requeues_orphaned_branch_mismatch_after_retry_limit() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile-orphan-branch-mismatch");
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/108"]);
        std::fs::write(worktree_dir.join("scratch.txt"), "stale dirty work\n").unwrap();
        git_ok(&worktree_dir, &["add", "scratch.txt"]);
        let git_dir = PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            worktree_dir.join(git_dir)
        };
        std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

        write_owned_task_file_with_context(
            &repo,
            124,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/124",
            ".batty/worktrees/eng-1",
        );

        let old = std::env::var("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS").ok();
        unsafe {
            std::env::set_var("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS", "1");
        }
        daemon.reconcile_active_tasks().unwrap();
        match old {
            Some(value) => unsafe {
                std::env::set_var("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS", value)
            },
            None => unsafe { std::env::remove_var("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS") },
        }

        let task = crate::task::load_task_by_id(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
            124,
        )
        .unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.claimed_by.is_none());
        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("124")
                && event.reason.as_deref() == Some("orphan_branch_mismatch_requeued")
        }));
    }

    #[test]
    fn reconcile_active_tasks_blocks_detached_head_branch_mismatch() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile-detached-branch");
        let manager = manager_member("manager", None);
        let engineer = engineer_member("eng-1", Some("manager"), true);
        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![manager, engineer])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();
        let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        git_ok(&worktree_dir, &["checkout", "--detach", &head]);

        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "HEAD");
        let inbox_root = inbox::inboxes_root(&repo);
        let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(manager_messages.iter().any(|message| {
            message.body.contains("Reconciliation alert")
                && message.body.contains("detached HEAD state")
        }));
    }

    #[test]
    fn deliver_automation_nudge_suppresses_engineer_nudge_when_branch_mismatch_exists() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "nudge-branch-mismatch");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");

        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
        write_owned_task_file_with_context(
            &repo,
            42,
            "authoritative-task",
            "in-progress",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let delivery = daemon
            .deliver_automation_nudge("eng-1", "Idle nudge: move the task forward.")
            .unwrap();

        assert_eq!(delivery, MessageDelivery::OrchestratorLogged);
        let inbox_root = inbox::inboxes_root(&repo);
        let engineer_messages = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert!(
            engineer_messages
                .iter()
                .all(|message| { !message.body.contains("Idle nudge: move the task forward.") })
        );
        let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(manager_messages.iter().any(|message| {
            message.body.contains("Reconciliation alert")
                && message.body.contains("eng-1/41")
                && message.body.contains("eng-1/42")
        }));
    }

    // ── manager_for_member_name ──────────────────────────────────

    #[test]
    fn manager_for_member_name_returns_reports_to() {
        let tmp = tempfile::tempdir().unwrap();
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
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
        assert_eq!(daemon.manager_for_member_name("eng-1"), Some("manager"));
    }

    #[test]
    fn manager_for_member_name_returns_none_for_top_level() {
        let tmp = tempfile::tempdir().unwrap();
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
        let daemon = make_test_daemon(tmp.path(), vec![architect]);
        assert_eq!(daemon.manager_for_member_name("architect"), None);
    }

    #[test]
    fn manager_for_member_name_returns_none_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = make_test_daemon(tmp.path(), Vec::new());
        assert_eq!(daemon.manager_for_member_name("nobody"), None);
    }

    // ── auto_unblock_notification_recipient ──────────────────────

    #[test]
    fn auto_unblock_recipient_is_task_owner_when_known() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let daemon = make_test_daemon(tmp.path(), vec![engineer]);

        let task = crate::task::Task {
            id: 10,
            title: "test".to_string(),
            status: "blocked".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("eng-1".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: Vec::new(),
            depends_on: vec![1],
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
            source_path: std::path::PathBuf::new(),
        };
        assert_eq!(
            daemon.auto_unblock_notification_recipient(&task),
            Some("eng-1".to_string())
        );
    }

    #[test]
    fn auto_unblock_recipient_falls_back_to_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let daemon = make_test_daemon(tmp.path(), vec![manager]);

        let task = crate::task::Task {
            id: 10,
            title: "test".to_string(),
            status: "blocked".to_string(),
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
            depends_on: vec![1],
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
            source_path: std::path::PathBuf::new(),
        };
        assert_eq!(
            daemon.auto_unblock_notification_recipient(&task),
            Some("manager".to_string())
        );
    }

    #[test]
    fn auto_unblock_recipient_ignores_unknown_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let daemon = make_test_daemon(tmp.path(), vec![manager]);

        let task = crate::task::Task {
            id: 10,
            title: "test".to_string(),
            status: "blocked".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("unknown-eng".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: Vec::new(),
            depends_on: vec![1],
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
            source_path: std::path::PathBuf::new(),
        };
        // Owner not in members → falls back to manager
        assert_eq!(
            daemon.auto_unblock_notification_recipient(&task),
            Some("manager".to_string())
        );
    }

    // ── truly_idle_engineer_count ────────────────────────────────

    #[test]
    fn truly_idle_counts_only_idle_engineers_without_board_items() {
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Idle);
        states.insert("eng-2".to_string(), MemberState::Idle);
        states.insert("eng-3".to_string(), MemberState::Working);

        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let eng2 = MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let eng3 = MemberInstance {
            name: "eng-3".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![eng1, eng2, eng3])
            .states(states)
            .build();

        // eng-3 is Working with an active task → not idle per idle_engineer_names
        daemon.active_tasks.insert("eng-3".to_string(), 99);

        // eng-2 has an in-progress task on the board
        let tasks = vec![crate::task::Task {
            id: 1,
            title: "active-task".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("eng-2".to_string()),
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
            source_path: std::path::PathBuf::new(),
        }];

        // eng-1 is idle with no board items → truly idle
        // eng-2 is idle but has in-progress task → not truly idle
        // eng-3 is working → not idle at all
        assert_eq!(daemon.truly_idle_engineer_count(&tasks), 1);
    }

    #[test]
    fn truly_idle_count_is_zero_when_all_busy() {
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Working);

        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![eng1])
            .states(states)
            .build();

        // Working engineer needs an active task to not be treated as idle
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        assert_eq!(daemon.truly_idle_engineer_count(&[]), 0);
    }

    #[test]
    fn truly_idle_strips_at_prefix_from_claimed_by() {
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Idle);

        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![eng1])
            .states(states)
            .build();

        let tasks = vec![crate::task::Task {
            id: 1,
            title: "task".to_string(),
            status: "todo".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("@eng-1".to_string()),
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
            source_path: std::path::PathBuf::new(),
        }];

        // eng-1 has a todo task (with @ prefix) — not truly idle
        assert_eq!(daemon.truly_idle_engineer_count(&tasks), 0);
    }

    // ── maybe_escalate_stale_reviews ─────────────────────────────

    #[test]
    fn escalate_stale_reviews_sends_nudge_then_escalation() {
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();
        inbox::init_inbox(&inbox_root, "architect").unwrap();

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

        // Use tiny thresholds for testing
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 5,
            review_timeout_secs: 10,
            ..WorkflowPolicy::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect, manager])
            .workflow_policy(policy)
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        // Write a task in review
        write_board_task_file(
            tmp.path(),
            50,
            "review-task",
            "review",
            Some("eng-1"),
            &[],
            None,
        );

        // First call: task just entered review, no nudge yet (age = 0)
        daemon.maybe_escalate_stale_reviews().unwrap();
        let pending_manager = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(pending_manager.is_empty(), "no nudge should fire at age 0");

        // Simulate the task having been first seen long enough ago for nudge
        daemon.review_first_seen.insert(50, 0); // epoch = 0, so age will be huge
        daemon.review_nudge_sent.clear();

        daemon.maybe_escalate_stale_reviews().unwrap();

        // At this point the age is >> both nudge (5s) and timeout (10s),
        // so escalation fires (escalation > nudge, and escalation check comes first)
        let pending_architect = inbox::pending_messages(&inbox_root, "architect").unwrap();
        assert!(
            pending_architect
                .iter()
                .any(|msg| msg.body.contains("Review timeout")),
            "architect should receive escalation message"
        );

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(
            events.iter().any(|e| e.event == "review_escalated"),
            "review_escalated event should be emitted"
        );
    }

    #[test]
    fn escalate_stale_reviews_sends_nudge_below_timeout() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 5,
            review_timeout_secs: 999_999, // very high so escalation won't fire
            ..WorkflowPolicy::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager])
            .workflow_policy(policy)
            .build();
        daemon.config.team_config.workflow_mode = crate::team::config::WorkflowMode::Hybrid;
        daemon.config.team_config.orchestrator_pane = true;
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        write_board_task_file(tmp.path(), 60, "nudge-task", "review", None, &[], None);

        // Simulate first_seen long enough ago to trigger nudge but not timeout
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        daemon.review_first_seen.insert(60, now - 100);

        daemon.maybe_escalate_stale_reviews().unwrap();

        let pending_manager = inbox::pending_messages(&inbox_root, "manager").unwrap();
        let orchestrator_log =
            std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(
            pending_manager.is_empty(),
            "review nudge should stay out of inbox"
        );
        assert!(
            orchestrator_log.contains("Review nudge"),
            "review nudge should be recorded in orchestrator log"
        );
        assert!(daemon.review_nudge_sent.contains(&60));

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|e| e.event == "review_nudge_sent"));
    }

    #[test]
    fn escalate_stale_reviews_skips_non_review_tasks() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1,
            review_timeout_secs: 2,
            ..WorkflowPolicy::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager])
            .workflow_policy(policy)
            .build();

        // Only in-progress and todo tasks — no review tasks
        write_board_task_file(tmp.path(), 70, "ip-task", "in-progress", None, &[], None);
        write_board_task_file(tmp.path(), 71, "todo-task", "todo", None, &[], None);

        daemon.maybe_escalate_stale_reviews().unwrap();

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn escalate_stale_reviews_prunes_tracking_for_non_review_tasks() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager])
            .workflow_policy(WorkflowPolicy::default())
            .build();

        // Pre-populate tracking with task IDs that are no longer in review
        daemon.review_first_seen.insert(80, 1000);
        daemon.review_first_seen.insert(81, 2000);
        daemon.review_nudge_sent.insert(80);

        // Only task 80 exists and it's done, 81 doesn't exist at all
        write_board_task_file(tmp.path(), 80, "done-task", "done", None, &[], None);

        daemon.maybe_escalate_stale_reviews().unwrap();

        assert!(!daemon.review_first_seen.contains_key(&80));
        assert!(!daemon.review_first_seen.contains_key(&81));
        assert!(!daemon.review_nudge_sent.contains(&80));
    }

    #[test]
    fn stale_review_tracking_emits_state_reconciliation_event() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let mut daemon = make_test_daemon(tmp.path(), Vec::new());
        std::fs::create_dir_all(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();
        daemon.event_sink = EventSink::new(&events_path).unwrap();
        daemon.review_first_seen.insert(80, 1000);
        daemon.review_nudge_sent.insert(80);

        daemon.maybe_escalate_stale_reviews().unwrap();

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.task.as_deref() == Some("80")
                && event.reason.as_deref() == Some("review_fix")
        }));
    }

    #[test]
    fn missing_review_tracking_emits_state_reconciliation_event() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let mut daemon = make_test_daemon(tmp.path(), Vec::new());
        daemon.event_sink = EventSink::new(&events_path).unwrap();
        write_board_task_file(
            tmp.path(),
            60,
            "review-task",
            "review",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.maybe_escalate_stale_reviews().unwrap();

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.task.as_deref() == Some("60")
                && event.reason.as_deref() == Some("review_fix")
        }));
    }

    #[test]
    fn active_stale_review_entries_skip_tasks_that_left_review() {
        let task = test_task(90, "done", Some("eng-1"), Some("manager"));
        let tasks_by_id = HashMap::from([(task.id, &task)]);
        let report = crate::team::board::TaskAgingReport {
            stale_in_progress: Vec::new(),
            aged_todo: Vec::new(),
            stale_review: vec![crate::team::board::AgedTask {
                task_id: 90,
                title: "task-90".to_string(),
                status: "review".to_string(),
                claimed_by: Some("eng-1".to_string()),
                age_secs: 7_200,
            }],
        };

        let (active, suppressed) = active_stale_review_entries(&report, &tasks_by_id);

        assert!(active.is_empty());
        assert_eq!(suppressed, vec![90]);
    }

    // ── maybe_rotate_board ───────────────────────────────────────

    #[test]
    fn maybe_rotate_board_skips_when_board_dir_exists() {
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(&board_dir).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        // Force last rotation far in the past to trigger the check
        daemon.last_board_rotation =
            std::time::Instant::now() - std::time::Duration::from_secs(700);

        daemon.maybe_rotate_board().unwrap();
        // No crash, no rotation needed for kanban-md directory board
    }

    #[test]
    fn maybe_rotate_board_skips_when_too_recent() {
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        // last_board_rotation is now (set by builder) — should skip
        daemon.maybe_rotate_board().unwrap();
        // No crash, just a no-op early return
    }

    // ── member_worktree_context ──────────────────────────────────

    #[test]
    fn member_worktree_context_returns_none_for_non_worktree_member() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let daemon = make_test_daemon(tmp.path(), vec![engineer]);
        assert!(daemon.member_worktree_context("eng-1").is_none());
    }

    #[test]
    fn member_worktree_context_returns_none_when_worktree_missing() {
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
            ..Default::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer])
            .build();
        daemon.is_git_repo = true;

        // Worktree directory doesn't exist
        assert!(daemon.member_worktree_context("eng-1").is_none());
    }

    // ── maybe_detect_pipeline_starvation ─────────────────────────

    #[test]
    fn pipeline_starvation_skipped_when_threshold_is_none() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: None,
            ..WorkflowPolicy::default()
        };

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .workflow_policy(policy)
            .build();

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(!daemon.pipeline_starvation_fired);
    }

    #[test]
    fn pipeline_starvation_skipped_when_no_idle_engineers() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
        };

        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };

        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Working);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![eng1])
            .states(states)
            .workflow_policy(policy)
            .build();

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(!daemon.pipeline_starvation_fired);
    }

    #[test]
    fn pipeline_starvation_fires_when_deficit_exceeds_threshold() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "architect").unwrap();

        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
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
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };

        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Idle);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect, eng1])
            .states(states)
            .workflow_policy(policy)
            .build();

        // No todo tasks at all, 1 idle engineer → deficit = 1 >= threshold 1
        daemon.maybe_detect_pipeline_starvation().unwrap();

        assert!(daemon.pipeline_starvation_fired);
        let pending = inbox::pending_messages(&inbox_root, "architect").unwrap();
        assert!(
            pending
                .iter()
                .any(|msg| msg.body.contains("Pipeline running dry")),
            "architect should be notified"
        );
    }

    #[test]
    fn pipeline_starvation_suppressed_when_enough_todo_tasks() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::standup::MemberState;
        use crate::team::test_support::{TestDaemonBuilder, write_open_task_file};

        let tmp = tempfile::tempdir().unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "architect").unwrap();

        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
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
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };

        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Idle);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect, eng1])
            .states(states)
            .workflow_policy(policy)
            .build();

        // 1 unclaimed todo task >= 1 idle engineer → no starvation
        write_open_task_file(tmp.path(), 90, "available-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(!daemon.pipeline_starvation_fired);
    }

    #[test]
    fn pipeline_starvation_suppressed_when_manager_working() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
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
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };

        let mut states = std::collections::HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Idle);
        states.insert("manager".to_string(), MemberState::Working);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect, manager, eng1])
            .states(states)
            .workflow_policy(policy)
            .build();

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(
            !daemon.pipeline_starvation_fired,
            "should suppress when manager is working"
        );
    }

    #[test]
    fn auto_retro_does_not_fire_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::daemon_started(),
                TeamEvent::task_assigned("eng-1", "45"),
                TeamEvent::task_completed("eng-1", Some("45")),
                TeamEvent::daemon_stopped(),
            ],
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .orchestrator_pane(false)
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        daemon.maybe_generate_retrospective().unwrap();
        daemon.maybe_generate_retrospective().unwrap();

        let retro_dir = tmp.path().join(".batty").join("retrospectives");
        let reports = std::fs::read_dir(&retro_dir).unwrap().count();
        assert_eq!(reports, 1);

        let events = crate::team::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
        );
    }

    // ── maybe_auto_archive ───────────────────────────────────────────

    /// Helper: write a done task with a specific completed date (RFC3339).
    fn write_done_task_with_completed(project_root: &Path, id: u32, title: &str, completed: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: done\npriority: high\ncompleted: \"{completed}\"\nclass: standard\n---\n\nTask.\n"
        );
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    /// Backdate the rate-limit timer so the archive check fires immediately.
    fn backdate_auto_archive(daemon: &mut TeamDaemon) {
        daemon.last_auto_archive = Instant::now() - Duration::from_secs(120);
    }

    #[test]
    fn auto_archive_moves_old_done_tasks() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let policy = WorkflowPolicy {
            auto_archive_done_after_secs: Some(60),
            ..WorkflowPolicy::default()
        };
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .workflow_policy(policy)
            .build();
        backdate_auto_archive(&mut daemon);

        // A task completed 2 hours ago — should be archived
        write_done_task_with_completed(tmp.path(), 1, "old-done", "2020-01-01T00:00:00+00:00");

        daemon.maybe_auto_archive().unwrap();

        let archive_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("archive");
        assert!(archive_dir.join("001-old-done.md").exists());

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "board_task_archived" && event.task.as_deref() == Some("1")
        }));
    }

    #[test]
    fn auto_archive_skips_recent_done() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let policy = WorkflowPolicy {
            auto_archive_done_after_secs: Some(86400), // 24h
            ..WorkflowPolicy::default()
        };
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .workflow_policy(policy)
            .build();
        backdate_auto_archive(&mut daemon);

        // A task completed just now — should NOT be archived
        let now = chrono::Utc::now().to_rfc3339();
        write_done_task_with_completed(tmp.path(), 2, "recent-done", &now);

        daemon.maybe_auto_archive().unwrap();

        let archive_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("archive");
        assert!(!archive_dir.exists() || !archive_dir.join("002-recent-done.md").exists());
    }

    #[test]
    fn auto_archive_respects_config_threshold() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        // Very large threshold — nothing should be archived
        let policy = WorkflowPolicy {
            auto_archive_done_after_secs: Some(999_999_999),
            ..WorkflowPolicy::default()
        };
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .workflow_policy(policy)
            .build();
        backdate_auto_archive(&mut daemon);

        // Even an old task shouldn't be archived with a huge threshold
        write_done_task_with_completed(tmp.path(), 3, "old-but-kept", "2024-01-01T00:00:00+00:00");

        daemon.maybe_auto_archive().unwrap();

        // Task file should still be in tasks/
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        assert!(tasks_dir.join("003-old-but-kept.md").exists());
    }

    #[test]
    fn auto_archive_noop_when_disabled() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        // Disabled: auto_archive_done_after_secs = Some(0)
        let policy = WorkflowPolicy {
            auto_archive_done_after_secs: Some(0),
            ..WorkflowPolicy::default()
        };
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .workflow_policy(policy)
            .build();
        backdate_auto_archive(&mut daemon);

        write_done_task_with_completed(
            tmp.path(),
            4,
            "disabled-archive",
            "2020-01-01T00:00:00+00:00",
        );

        daemon.maybe_auto_archive().unwrap();

        // Task should remain in tasks/
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        assert!(tasks_dir.join("004-disabled-archive.md").exists());
    }

    #[test]
    fn auto_archive_noop_when_none() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        // Disabled: auto_archive_done_after_secs = None (default)
        let policy = WorkflowPolicy {
            auto_archive_done_after_secs: None,
            ..WorkflowPolicy::default()
        };
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .workflow_policy(policy)
            .build();
        backdate_auto_archive(&mut daemon);

        write_done_task_with_completed(tmp.path(), 5, "none-archive", "2020-01-01T00:00:00+00:00");

        daemon.maybe_auto_archive().unwrap();

        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        assert!(tasks_dir.join("005-none-archive.md").exists());
    }

    // ── pipeline starvation time-bounded manager suppression ──

    #[test]
    fn pipeline_starvation_fires_when_manager_working_too_long() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "architect").unwrap();

        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
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
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };

        let mut states = std::collections::HashMap::new();
        states.insert("manager".to_string(), MemberState::Working);
        states.insert("eng-1".to_string(), MemberState::Idle);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect, manager, eng1])
            .states(states)
            .workflow_policy(policy)
            .build();

        // Manager has no shim handle (non-shim mode) and no idle_started_at
        // entry, meaning the old code would suppress starvation. But since
        // there's no shim handle, the fallback checks idle_started_at. Without
        // an entry, it falls back to suppressed. To test the shim path:
        // Insert a mock shim handle for the manager that's been working for
        // 20 minutes (past the 10-minute grace).
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Working);
        // Backdate to 20 minutes ago
        handle.state_changed_at = std::time::Instant::now() - std::time::Duration::from_secs(1200);
        daemon.shim_handles.insert("manager".to_string(), handle);

        daemon.maybe_detect_pipeline_starvation().unwrap();

        assert!(
            daemon.pipeline_starvation_fired,
            "starvation should fire when manager has been working for >10 minutes"
        );
    }

    #[test]
    fn pipeline_starvation_suppressed_when_manager_recently_working() {
        use crate::team::config::WorkflowPolicy;
        use crate::team::standup::MemberState;
        use crate::team::test_support::TestDaemonBuilder;

        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "architect").unwrap();

        let policy = WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
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
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };

        let mut states = std::collections::HashMap::new();
        states.insert("manager".to_string(), MemberState::Working);
        states.insert("eng-1".to_string(), MemberState::Idle);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect, manager, eng1])
            .states(states)
            .workflow_policy(policy)
            .build();

        // Insert a mock shim handle for manager that's been working for only
        // 2 minutes (within the 10-minute grace).
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Working);
        // Just started working — within grace period
        daemon.shim_handles.insert("manager".to_string(), handle);

        daemon.maybe_detect_pipeline_starvation().unwrap();

        assert!(
            !daemon.pipeline_starvation_fired,
            "starvation should be suppressed when manager is recently working"
        );
    }

    #[test]
    fn claim_ttl_initializes_tracking_for_claimed_in_progress_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-init");
        write_owned_task_file(&repo, 42, "ttl-init", "in-progress", "eng-1");

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .build();

        daemon.maybe_manage_task_claim_ttls().unwrap();

        let task = crate::task::Task::from_file(&board_task_path(&repo, 42)).unwrap();
        assert_eq!(task.claim_ttl_secs, Some(900));
        assert!(task.claimed_at.is_some());
        assert!(task.claim_expires_at.is_some());
        assert!(task.last_progress_at.is_some());

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_claim_created" && event.task.as_deref() == Some("42")
        }));
    }

    #[test]
    fn fresh_claim_grace_suppresses_stale_aging_with_old_progress_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "fresh-claim-grace");
        write_board_task_file(
            &repo,
            42,
            "fresh-claim-grace",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        let task_path = board_task_path(&repo, 42);
        let now = chrono::Utc::now();
        let old_started = (now - chrono::Duration::hours(8)).to_rfc3339();
        let stale_progress = (now - chrono::Duration::hours(7)).to_rfc3339();
        let fresh_claim = now.to_rfc3339();
        update_task_frontmatter(&task_path, |mapping| {
            set_optional_string(mapping, "started", Some(&old_started));
            set_optional_string(mapping, "updated", Some(&old_started));
            set_optional_string(mapping, "claimed_at", Some(&fresh_claim));
            set_optional_string(mapping, "last_progress_at", Some(&stale_progress));
            set_optional_u64(mapping, "last_output_bytes", Some(0));
        })
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                stale_in_progress_hours: 1,
                ..WorkflowPolicy::default()
            })
            .build();
        daemon
            .config
            .team_config
            .automation
            .intervention_cooldown_secs = 300;

        daemon.maybe_emit_task_aging_alerts().unwrap();

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(
            !events.iter().any(|event| {
                matches!(event.event.as_str(), "task_stale" | "task_escalated")
                    && event.task.as_deref() == Some("42")
            }),
            "fresh claims should suppress stale aging even when last_progress_at is old"
        );
    }

    #[test]
    fn claim_ttl_reclaims_expired_task_without_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-expire");
        write_owned_task_file(&repo, 42, "ttl-expire", "in-progress", "eng-1");

        let task_path = board_task_path(&repo, 42);
        let now = chrono::Utc::now();
        let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
        let recent_progress_time = now.to_rfc3339();
        update_task_frontmatter(&task_path, |mapping| {
            set_optional_string(mapping, "claimed_at", Some(&stale_time));
            set_optional_u64(mapping, "claim_ttl_secs", Some(60));
            set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
            set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
            set_optional_u64(mapping, "last_output_bytes", Some(0));
            mapping.insert(
                yaml_key("claim_extensions"),
                serde_yaml::Value::Number(0.into()),
            );
        })
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        daemon.maybe_manage_task_claim_ttls().unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.claimed_by.is_none());
        assert!(
            task.next_action
                .as_deref()
                .unwrap_or("")
                .contains("Reclaimed after TTL expiry")
        );
        assert_eq!(daemon.active_task_id("eng-1"), None);

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_claim_expired"
                && event.task.as_deref() == Some("42")
                && event
                    .reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("time_held_secs=")
        }));
    }

    #[test]
    fn claim_ttl_reclaim_resets_engineer_worktree_to_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-reset");
        write_owned_task_file(&repo, 42, "ttl-reset", "in-progress", "eng-1");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/42"]);
        std::fs::write(worktree_dir.join("tracked.txt"), "tracked reclaim work\n").unwrap();
        git_ok(&worktree_dir, &["add", "tracked.txt"]);
        std::fs::write(
            worktree_dir.join("untracked.txt"),
            "untracked reclaim work\n",
        )
        .unwrap();

        let task_path = board_task_path(&repo, 42);
        let now = chrono::Utc::now();
        let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
        let recent_progress_time = now.to_rfc3339();
        update_task_frontmatter(&task_path, |mapping| {
            set_optional_string(mapping, "claimed_at", Some(&stale_time));
            set_optional_u64(mapping, "claim_ttl_secs", Some(60));
            set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
            set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
            set_optional_u64(mapping, "last_output_bytes", Some(0));
            mapping.insert(
                yaml_key("claim_extensions"),
                serde_yaml::Value::Number(0.into()),
            );
        })
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        daemon.maybe_manage_task_claim_ttls().unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
        assert_eq!(
            git_stdout(&repo, &["show", "eng-1/42:tracked.txt"]),
            "tracked reclaim work"
        );
        assert_eq!(
            git_stdout(&repo, &["show", "eng-1/42:untracked.txt"]),
            "untracked reclaim work"
        );
    }

    #[test]
    fn claim_ttl_reclaim_blocks_lane_when_preserve_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-blocked");
        write_owned_task_file(&repo, 42, "ttl-blocked", "in-progress", "eng-1");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/42"]);
        std::fs::write(worktree_dir.join("tracked.txt"), "tracked reclaim work\n").unwrap();
        git_ok(&worktree_dir, &["add", "tracked.txt"]);
        let git_dir =
            std::path::PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            worktree_dir.join(git_dir)
        };
        std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

        let task_path = board_task_path(&repo, 42);
        let now = chrono::Utc::now();
        let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
        let recent_progress_time = now.to_rfc3339();
        update_task_frontmatter(&task_path, |mapping| {
            set_optional_string(mapping, "claimed_at", Some(&stale_time));
            set_optional_u64(mapping, "claim_ttl_secs", Some(60));
            set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
            set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
            set_optional_u64(mapping, "last_output_bytes", Some(0));
            mapping.insert(
                yaml_key("claim_extensions"),
                serde_yaml::Value::Number(0.into()),
            );
        })
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        daemon.maybe_manage_task_claim_ttls().unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "blocked");
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.iter().any(|message| {
            message.body.contains(
                "could not safely auto-save eng-1's dirty worktree before TTL reclaim/reset",
            )
        }));
    }

    #[test]
    fn claim_ttl_reclaim_marks_same_engineer_as_recent_dispatch_and_prefers_alternative() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-redispatch");
        write_owned_task_file(&repo, 42, "ttl-redispatch", "in-progress", "eng-1");
        write_open_task_file(&repo, 99, "background-done", "done");

        let task_path = board_task_path(&repo, 42);
        let now = chrono::Utc::now();
        let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
        let recent_progress_time = now.to_rfc3339();
        update_task_frontmatter(&task_path, |mapping| {
            set_optional_string(mapping, "claimed_at", Some(&stale_time));
            set_optional_u64(mapping, "claim_ttl_secs", Some(60));
            set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
            set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
            set_optional_u64(mapping, "last_output_bytes", Some(0));
            mapping.insert(
                yaml_key("claim_extensions"),
                serde_yaml::Value::Number(0.into()),
            );
        })
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), false),
                engineer_member("eng-2", Some("manager"), false),
            ])
            .board(BoardConfig {
                auto_dispatch: true,
                dispatch_stabilization_delay_secs: 0,
                dispatch_dedup_window_secs: 60,
                ..BoardConfig::default()
            })
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        daemon.maybe_manage_task_claim_ttls().unwrap();

        assert!(
            daemon
                .recent_dispatches
                .contains_key(&(42, "eng-1".to_string()))
        );

        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );
        daemon.idle_started_at.insert(
            "eng-2".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        daemon.maybe_auto_dispatch().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].task_id, 42);
        assert_eq!(daemon.dispatch_queue[0].engineer, "eng-2");
    }

    #[test]
    fn claim_ttl_progress_from_output_extends_claim() {
        use crate::shim::protocol::socketpair;

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-output");
        write_owned_task_file(&repo, 42, "ttl-output", "in-progress", "eng-1");

        let task_path = board_task_path(&repo, 42);
        let now = chrono::Utc::now();
        let stale_time = (now - chrono::Duration::minutes(20)).to_rfc3339();
        let recent_progress_time = now.to_rfc3339();
        update_task_frontmatter(&task_path, |mapping| {
            set_optional_string(mapping, "claimed_at", Some(&stale_time));
            set_optional_u64(mapping, "claim_ttl_secs", Some(1800));
            set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
            set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
            set_optional_u64(mapping, "last_output_bytes", Some(0));
            mapping.insert(
                yaml_key("claim_extensions"),
                serde_yaml::Value::Number(0.into()),
            );
        })
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .build();
        let (parent_sock, _child_sock) = socketpair().unwrap();
        let mut handle = super::super::agent_handle::AgentHandle::new(
            "eng-1".to_string(),
            crate::shim::protocol::Channel::new(parent_sock),
            12345,
            "codex".to_string(),
            "codex".to_string(),
            repo.to_path_buf(),
        );
        handle.record_output_bytes(128);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        daemon.maybe_manage_task_claim_ttls().unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.last_output_bytes, Some(128));
        assert!(task.claim_expires_at.as_deref().unwrap_or("") > stale_time.as_str());

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_claim_progress"
                && event.task.as_deref() == Some("42")
                && event.reason.as_deref() == Some("output")
        }));
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_stale_worktrees_rebases_clean_base_worktree_after_main_advances() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "worktree-auto-rebase");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();

        std::fs::write(repo.join("main.txt"), "advance main\n").unwrap();
        git_ok(&repo, &["add", "main.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "worktree_refreshed"
                && event.role.as_deref() == Some("eng-1")
                && event
                    .reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("rebased clean base worktree onto main")
        }));
    }

    #[test]
    fn reconcile_stale_worktrees_skips_base_worktree_when_engineer_has_active_task() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "worktree-auto-rebase-active");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        let before_head = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);

        std::fs::write(repo.join("main.txt"), "advance main\n").unwrap();
        git_ok(&repo, &["add", "main.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 488);

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"]),
            before_head
        );
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_stale_worktrees_resets_idle_branch_when_board_no_longer_authorizes_it() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "worktree-reset-unauthorized-branch");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "stale branch work\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "stale branch work"]);

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "worktree_reconciled"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
        }));
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some("branch_reset")
        }));
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_stale_worktrees_preserves_staged_done_lane_before_safe_branch_reset() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "worktree-reset-done-staged");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();
        std::fs::write(worktree_dir.join("staged-only.txt"), "baseline\n").unwrap();
        git_ok(&worktree_dir, &["add", "staged-only.txt"]);
        git_ok(
            &worktree_dir,
            &["commit", "-m", "baseline stale branch file"],
        );
        std::fs::write(
            worktree_dir.join("staged-only.txt"),
            "staged dirty done work\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "staged-only.txt"]);

        write_owned_task_file_with_context(
            &repo,
            42,
            "done-task",
            "done",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
        assert!(
            !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
            "worktree should be clean after preserving staged done-lane work"
        );
        let preserved_record =
            crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
        assert_eq!(
            preserved_record.artifact_kind,
            crate::team::checkpoint::PreservedLaneArtifactKind::Commit
        );
        assert_eq!(preserved_record.task_id, 42);
        assert_eq!(preserved_record.source_branch, "eng-1/42");
        let log_on_42 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/42", "-1"]);
        assert!(
            log_on_42.contains("preserve completed task #42 before branch recovery [eng-1/42]"),
            "expected preserve commit on stale branch, got:\n{log_on_42}"
        );
        let preserved = git_stdout(&worktree_dir, &["show", "eng-1/42:staged-only.txt"]);
        assert_eq!(preserved, "staged dirty done work");
        assert!(
            !worktree_dir.join("staged-only.txt").exists(),
            "reset to base branch should drop stale-branch-only file from current worktree"
        );

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "worktree_reconciled"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
        }));
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some("done_lane_reset")
        }));
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_stale_worktrees_preserves_unstaged_done_lane_before_safe_branch_reset() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "worktree-reset-done-unstaged");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();
        std::fs::write(
            worktree_dir.join("unstaged-only.txt"),
            "unstaged done work\n",
        )
        .unwrap();

        write_owned_task_file_with_context(
            &repo,
            42,
            "done-task",
            "done",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
        assert!(
            !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
            "worktree should be clean after preserving unstaged done-lane work"
        );
        let preserved_record =
            crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
        assert_eq!(
            preserved_record.artifact_kind,
            crate::team::checkpoint::PreservedLaneArtifactKind::Commit
        );
        assert_eq!(preserved_record.task_id, 42);
        assert_eq!(preserved_record.source_branch, "eng-1/42");
        let log_on_42 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/42", "-1"]);
        assert!(
            log_on_42.contains("preserve completed task #42 before branch recovery [eng-1/42]"),
            "expected preserve commit on stale branch, got:\n{log_on_42}"
        );
        let preserved = git_stdout(&worktree_dir, &["show", "eng-1/42:unstaged-only.txt"]);
        assert_eq!(preserved, "unstaged done work");
        assert!(
            !worktree_dir.join("unstaged-only.txt").exists(),
            "reset to base branch should drop stale-branch-only file from current worktree"
        );

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "worktree_reconciled"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
        }));
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some("done_lane_reset")
        }));
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_stale_worktrees_snapshots_dirty_done_lane_when_commit_preserve_fails() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "worktree-reset-done-preserve-failure");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42")
            .unwrap();
        std::fs::write(
            worktree_dir.join("blocked.txt"),
            "blocked dirty done work\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "blocked.txt"]);
        let git_dir =
            std::path::PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            worktree_dir.join(git_dir)
        };
        std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

        write_owned_task_file_with_context(
            &repo,
            42,
            "done-task",
            "done",
            "eng-1",
            "eng-1/42",
            ".batty/worktrees/eng-1",
        );

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
        assert!(
            !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
            "worktree should be clean after snapshot fallback recreates it"
        );
        let task = crate::task::load_task_by_id(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
            42,
        )
        .unwrap();
        assert_eq!(task.status, "done");
        let preserved_record =
            crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
        assert_eq!(
            preserved_record.artifact_kind,
            crate::team::checkpoint::PreservedLaneArtifactKind::Snapshot
        );
        let snapshot_rel = preserved_record.snapshot_path.clone().unwrap();
        let snapshot_abs = repo.join(&snapshot_rel);
        assert!(
            snapshot_abs.exists(),
            "expected snapshot at {}",
            snapshot_abs.display()
        );
        let snapshot = std::fs::read_to_string(&snapshot_abs).unwrap();
        assert!(snapshot.contains("blocked dirty done work"));
        assert!(snapshot.contains("blocked.txt"));

        let manager_messages = crate::team::inbox::pending_messages(
            &crate::team::inbox::inboxes_root(&repo),
            "manager",
        )
        .unwrap();
        assert!(manager_messages.is_empty());
        let engineer_messages =
            crate::team::inbox::pending_messages(&crate::team::inbox::inboxes_root(&repo), "eng-1")
                .unwrap();
        assert!(engineer_messages.is_empty());

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "worktree_reconciled"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
        }));
        assert!(events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some("done_lane_reset")
        }));
    }
}
