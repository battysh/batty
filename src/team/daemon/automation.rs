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

fn claim_time_held_secs(task: &crate::task::Task, now: DateTime<Utc>) -> Option<u64> {
    task.claimed_at
        .as_deref()
        .and_then(parse_rfc3339_utc)
        .and_then(|claimed_at| {
            let held_secs = now.signed_duration_since(claimed_at).num_seconds();
            (held_secs >= 0).then_some(held_secs as u64)
        })
}

fn reset_claimed_worktree_to_base(work_dir: &std::path::Path, base_branch: &str) -> Result<()> {
    crate::team::git_cmd::run_git(work_dir, &["reset", "--hard"])
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    crate::team::git_cmd::run_git(work_dir, &["clean", "-fd"])
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    crate::team::git_cmd::checkout_new_branch(work_dir, base_branch, "main")
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    crate::team::git_cmd::run_git(work_dir, &["reset", "--hard", "main"])
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(())
}

impl TeamDaemon {
    pub(in crate::team) fn deliver_automation_nudge(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
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

            if should_check_progress
                && let Some(progress_type) =
                    task_claim_progress_type(task, &work_dir, current_output_bytes)
            {
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
                    .insert(progress_key, Instant::now());
                continue;
            }

            self.intervention_cooldowns
                .insert(progress_key, Instant::now());

            let Some(expires_at) = task.claim_expires_at.as_deref().and_then(parse_rfc3339_utc)
            else {
                continue;
            };

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
            let _ = self.preserve_member_worktree(
                engineer,
                &format!("wip: auto-save before claim reclaim [{engineer}]"),
            );
            if use_worktrees {
                let base_branch = engineer_base_branch_name(engineer);
                match reset_claimed_worktree_to_base(&work_dir, &base_branch) {
                    Ok(()) => self.record_orchestrator_action(format!(
                        "claim ttl: reset {} worktree to {} before reclaiming task #{}",
                        engineer, base_branch, task.id
                    )),
                    Err(error) => warn!(
                        engineer = %engineer,
                        task_id = task.id,
                        worktree = %work_dir.display(),
                        error = %error,
                        "claim ttl: failed to reset engineer worktree before reclaim"
                    ),
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

    pub(super) fn claim_ttl_secs_for_priority(&self, priority: &str) -> u64 {
        let policy = &self.config.team_config.workflow_policy.claim_ttl;
        if priority.eq_ignore_ascii_case("critical") {
            policy.critical_secs
        } else {
            policy.default_secs
        }
    }

    pub(super) fn reconcile_active_tasks(&mut self) -> Result<()> {
        let tasks_dir = self.board_dir().join("tasks");
        let board_tasks = if tasks_dir.exists() {
            crate::task::load_tasks_from_dir(&tasks_dir)?
        } else {
            Vec::new()
        };
        if self.state_reconciliation_audit_due() {
            debug!("state reconciliation audit due — adopting board-owned tasks");
            self.adopt_board_owned_tasks(&board_tasks);
            self.mark_state_reconciliation_audit();
        }
        if self.active_tasks.is_empty() {
            return Ok(());
        }
        let stale: Vec<(String, u32, &'static str)> = self
            .active_tasks
            .iter()
            .filter_map(|(engineer, task_id)| {
                let task_id = *task_id;
                match board_tasks.iter().find(|t| t.id == task_id) {
                    Some(task) if task.status == "done" || task.status == "archived" => {
                        Some((engineer.clone(), task_id, "task is done/archived"))
                    }
                    None => Some((engineer.clone(), task_id, "task no longer exists")),
                    Some(task) if task.claimed_by.as_deref() != Some(engineer.as_str()) => Some((
                        engineer.clone(),
                        task_id,
                        "task no longer claimed by this engineer",
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
        for (engineer, task_id, reason) in stale {
            info!(
                engineer = %engineer,
                task_id,
                reason,
                "Reconciled stale active_task: {engineer} was tracking task #{task_id} ({reason})"
            );
            self.record_state_reconciliation(Some(&engineer), Some(task_id), "clear");
            self.record_orchestrator_action(format!(
                "state reconciliation: cleared stale active task #{} for {} ({})",
                task_id, engineer, reason
            ));
            self.clear_active_task(&engineer);
        }

        // WIP reconciliation: if an engineer has multiple claimed non-done tasks,
        // unclaim the extras (keep the one with lowest ID / highest priority).
        // This catches cases where the manager claims tasks via kanban-md directly,
        // bypassing the daemon's WIP guard.
        let engineer_names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .map(|m| m.name.clone())
            .collect();
        let board_dir = self.board_dir();
        for eng in &engineer_names {
            // Only count in-progress and review as active WIP.
            // Claimed todo/backlog tasks are reservations, not active work —
            // they shouldn't block dispatch or count toward WIP limits.
            let mut claimed: Vec<&crate::task::Task> = board_tasks
                .iter()
                .filter(|t| {
                    t.claimed_by.as_deref() == Some(eng.as_str())
                        && matches!(t.status.as_str(), "in-progress" | "review")
                })
                .collect();
            if claimed.len() <= 1 {
                continue;
            }
            let in_progress_count = claimed
                .iter()
                .filter(|task| task.status == "in-progress")
                .count();
            if in_progress_count > 1 {
                warn!(
                    engineer = eng.as_str(),
                    in_progress_count,
                    "WIP reconciliation: skipping auto-unclaim because multiple in-progress tasks are claimed"
                );
                continue;
            }
            // Keep the in-progress one, or the lowest ID if none in-progress
            claimed.sort_by_key(|t| (if t.status == "in-progress" { 0 } else { 1 }, t.id));
            let keep = claimed[0].id;
            for task in &claimed[1..] {
                warn!(
                    engineer = eng.as_str(),
                    task_id = task.id,
                    kept_task_id = keep,
                    "WIP reconciliation: unclaiming excess task #{} from {} (keeping #{})",
                    task.id,
                    eng,
                    keep
                );
                if let Err(e) = crate::team::task_cmd::unclaim_task(&board_dir, task.id) {
                    warn!(
                        task_id = task.id,
                        error = %e,
                        "failed to unclaim excess task"
                    );
                }
                // Move back to todo so it's dispatchable again
                if matches!(task.status.as_str(), "in-progress" | "backlog" | "review") {
                    // review -> todo requires going through in-progress first
                    if task.status == "review" {
                        let _ = crate::team::task_cmd::transition_task(
                            &board_dir,
                            task.id,
                            "in-progress",
                        );
                    }
                    let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "todo");
                }
            }
        }

        // Skip tasks that are currently tracked in active_tasks — those were just
        // dispatched this cycle and the board file may not reflect the claim yet.
        let actively_tracked: std::collections::HashSet<u32> =
            self.active_tasks.values().copied().collect();

        // Orphaned review rescue: tasks in "review" that are stuck because
        // they have no review_owner (nobody assigned to review), or no
        // claimed_by at all. Move them back to todo for re-dispatch.
        for task in &board_tasks {
            if task.status == "review"
                && (task.claimed_by.is_none() || task.review_owner.is_none())
                && !actively_tracked.contains(&task.id)
            {
                warn!(
                    task_id = task.id,
                    "orphaned review task #{} has no owner — moving back to todo", task.id
                );
                let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "in-progress");
                let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "todo");
                let _ = crate::team::task_cmd::unclaim_task(&board_dir, task.id);
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

    fn adopt_board_owned_tasks(&mut self, board_tasks: &[crate::task::Task]) {
        let mut candidates: HashMap<String, Vec<u32>> = HashMap::new();
        for task in board_tasks {
            if !super::interventions::task_needs_owned_intervention(task.status.as_str()) {
                continue;
            }
            let Some(engineer) = task.claimed_by.as_deref() else {
                continue;
            };
            if self.active_tasks.contains_key(engineer) {
                continue;
            }
            candidates
                .entry(engineer.to_string())
                .or_default()
                .push(task.id);
        }

        for (engineer, task_ids) in candidates {
            if let [task_id] = task_ids.as_slice() {
                self.active_tasks.insert(engineer.clone(), *task_id);
                self.record_state_reconciliation(Some(&engineer), Some(*task_id), "adopt");
                self.record_orchestrator_action(format!(
                    "state reconciliation: adopted board-owned task #{} for {}",
                    task_id, engineer
                ));
            } else {
                warn!(
                    engineer = %engineer,
                    ?task_ids,
                    "state reconciliation: skipping adopt due to multiple claimed board tasks"
                );
            }
        }
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

        let active_keys: HashSet<String> = report
            .stale_in_progress
            .iter()
            .map(|task| aging_cooldown_key("task_stale", task.task_id))
            .chain(
                report
                    .aged_todo
                    .iter()
                    .map(|task| aging_cooldown_key("task_aged", task.task_id)),
            )
            .chain(
                report
                    .stale_review
                    .iter()
                    .map(|task| aging_cooldown_key("review_stale", task.task_id)),
            )
            .collect();
        self.intervention_cooldowns
            .retain(|key, _| !key.starts_with("aging::") || active_keys.contains(key));

        for task in &report.stale_in_progress {
            let key = aging_cooldown_key("task_stale", task.task_id);
            if self.aging_alert_on_cooldown(&key) {
                continue;
            }

            let owner = task.claimed_by.as_deref().unwrap_or("unassigned");
            let reason = format!(
                "stale in-progress after {}s with no commits ahead of main",
                task.age_secs
            );
            self.emit_event(TeamEvent::task_stale(
                owner,
                &task.task_id.to_string(),
                &reason,
            ));
            self.record_task_escalated(owner, task.task_id.to_string(), Some("task_stale"));

            if let Some(recipient) = task
                .claimed_by
                .as_deref()
                .and_then(|owner| self.manager_for_member_name(owner))
                .map(str::to_string)
                .or_else(|| first_manager_name(&self.config.members))
            {
                let body = format!(
                    "Task #{} has been in progress for {}s with no commits ahead of `main`.\nTask: {}\nOwner: {}\nNext step: intervene, split the task, or confirm the engineer is still making progress.",
                    task.task_id, task.age_secs, task.title, owner
                );
                let _ = self.queue_daemon_message(&recipient, &body);
            }

            self.intervention_cooldowns.insert(key, Instant::now());
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

        for task in &report.stale_review {
            let key = aging_cooldown_key("review_stale", task.task_id);
            if self.aging_alert_on_cooldown(&key) {
                continue;
            }

            let review_owner = tasks_by_id
                .get(&task.task_id)
                .and_then(|task| task.review_owner.as_deref())
                .map(str::to_string)
                .or_else(|| {
                    task.claimed_by
                        .as_deref()
                        .and_then(|owner| self.manager_for_member_name(owner))
                        .map(str::to_string)
                })
                .or_else(|| first_manager_name(&self.config.members));
            let reason = format!("review queue stale after {}s", task.age_secs);
            self.emit_event(TeamEvent::review_stale(&task.task_id.to_string(), &reason));

            if let Some(recipient) = review_owner {
                let body = format!(
                    "Review urgency: task #{} has been in review for {}s.\nTask: {}\nNext step: merge it, request rework, or escalate immediately.",
                    task.task_id, task.age_secs, task.title
                );
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
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .planning_cycle_cooldown_secs,
        );

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
                self.record_tact_tasks_created(
                    &architect,
                    created as u32,
                    latency_secs,
                    true,
                    None,
                );
                info!(created, "applied planning cycle response");
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

            if crate::worktree::has_uncommitted_changes(&worktree_dir).unwrap_or(true) {
                debug!(
                    engineer = %engineer,
                    branch = %branch,
                    "worktree has uncommitted changes; skipping reconciliation"
                );
                continue;
            }

            if !is_worktree_safe_to_mutate(&worktree_dir).unwrap_or(false) {
                debug!(
                    engineer = %engineer,
                    branch = %branch,
                    "skipping worktree reconciliation — unsafe to mutate"
                );
                continue;
            }

            if let Err(error) = checkout_worktree_branch_from_main(&worktree_dir, &base_branch) {
                warn!(
                    engineer = %engineer,
                    branch = %branch,
                    error = %error,
                    "worktree reconciliation failed"
                );
                continue;
            }

            info!(
                engineer = %engineer,
                stale_branch = %branch,
                reset_to = %base_branch,
                "auto-reconciled stale worktree"
            );
            self.emit_event(TeamEvent::worktree_reconciled(&engineer, &branch));
            self.record_orchestrator_action(format!(
                "worktree: auto-reconciled {engineer} from stale branch '{branch}' to '{base_branch}'"
            ));
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

#[cfg(test)]
mod tests {
    use super::super::*;
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
        write_owned_task_file,
    };
    use std::collections::HashMap;

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
    fn reconcile_active_tasks_keeps_multiple_in_progress_claims_for_manager_override() {
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
        assert!(eng_tasks.contains(&(20, "in-progress")));
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

        let task = crate::task::Task::from_file(
            &repo.join(".batty/team_config/board/tasks/042-ttl-init.md"),
        )
        .unwrap();
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
    fn claim_ttl_reclaims_expired_task_without_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-expire");
        write_owned_task_file(&repo, 42, "ttl-expire", "in-progress", "eng-1");

        let task_path = repo.join(".batty/team_config/board/tasks/042-ttl-expire.md");
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

        let task_path = repo.join(".batty/team_config/board/tasks/042-ttl-reset.md");
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
    }

    #[test]
    fn claim_ttl_reclaim_marks_same_engineer_as_recent_dispatch_and_prefers_alternative() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "claim-ttl-redispatch");
        write_owned_task_file(&repo, 42, "ttl-redispatch", "in-progress", "eng-1");
        write_open_task_file(&repo, 99, "background-done", "done");

        let task_path = repo.join(".batty/team_config/board/tasks/042-ttl-redispatch.md");
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

        let task_path = repo.join(".batty/team_config/board/tasks/042-ttl-output.md");
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
}
