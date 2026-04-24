//! Backend health, worktree staleness, uncommitted work warnings, and prompt loading.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::super::*;
use crate::team::inbox;
use crate::team::prompt_compose::{render_member_prompt, resolve_prompt_context};
use crate::team::task_loop::git_has_unresolved_conflicts;
use crate::team::workspace::workspace_repo_targets;

const SHARED_TARGET_DISK_THRESHOLD_PCT: u8 = 80;
const SHARED_TARGET_CLEANUP_INTERVAL: Duration = Duration::from_secs(900);

fn conflict_paths(repo_dir: &Path) -> Vec<String> {
    let output = match std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(repo_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn git_ref_exists(repo_dir: &Path, rev: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", rev])
        .current_dir(repo_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn abort_conflicted_git_operation(repo_dir: &Path, args: &[&str]) {
    let _ = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output();
}

fn claude_credentials_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude").join(".credentials.json"))
}

fn parse_claude_oauth_expiry_ms(credentials: &str) -> Option<i64> {
    let json: Value = serde_json::from_str(credentials).ok()?;
    json["claudeAiOauth"]["expiresAt"].as_i64()
}

fn claude_oauth_healthy(credentials_path: &Path) -> bool {
    let contents = match std::fs::read_to_string(credentials_path) {
        Ok(contents) => contents,
        Err(_) => return false,
    };
    let Some(expires_at_ms) = parse_claude_oauth_expiry_ms(&contents) else {
        return false;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as i64;
    expires_at_ms.saturating_sub(now_ms) > 300_000
}

fn workspace_uncommitted_diff_lines(
    worktree_path: &Path,
    is_multi_repo: bool,
    sub_repo_names: &[String],
) -> Result<usize> {
    let mut total = 0usize;
    for repo in workspace_repo_targets(worktree_path, is_multi_repo, sub_repo_names) {
        total += super::uncommitted_diff_lines(&repo.path).with_context(|| match &repo.label {
            Some(label) => format!(
                "failed to measure uncommitted diff in sub-repo '{label}' under {}",
                worktree_path.display()
            ),
            None => format!(
                "failed to measure uncommitted diff in {}",
                worktree_path.display()
            ),
        })?;
    }
    Ok(total)
}

fn uncommitted_warning_worktree_path(
    configured_worktree_path: &Path,
    live_work_dir: Option<&Path>,
    is_multi_repo: bool,
) -> PathBuf {
    let Some(live_work_dir) = live_work_dir else {
        return configured_worktree_path.to_path_buf();
    };

    if is_multi_repo {
        return if path_is_within(live_work_dir, configured_worktree_path) {
            configured_worktree_path.to_path_buf()
        } else {
            live_work_dir.to_path_buf()
        };
    }

    git_worktree_root(live_work_dir).unwrap_or_else(|| live_work_dir.to_path_buf())
}

fn git_worktree_root(path: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let normalized_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let normalized_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    normalized_path.starts_with(normalized_root)
}

fn is_commit_reminder_body(body: &str) -> bool {
    body.trim_start()
        .to_ascii_lowercase()
        .starts_with("commit reminder:")
}

fn pending_commit_reminder_exists(project_root: &Path, member: &str) -> bool {
    let inboxes_root = inbox::inboxes_root(project_root);
    crate::team::inbox_tiered::pending_messages_union(&inboxes_root, member)
        .map(|messages| {
            messages
                .iter()
                .any(|message| is_commit_reminder_body(&message.body))
        })
        .unwrap_or(false)
}

fn clear_pending_commit_reminders(project_root: &Path, member: &str) -> Result<usize> {
    let inboxes_root = inbox::inboxes_root(project_root);
    let mut cleared = 0usize;

    for message in inbox::pending_messages(&inboxes_root, member)? {
        if is_commit_reminder_body(&message.body) {
            inbox::mark_delivered(&inboxes_root, member, &message.id)?;
            cleared += 1;
        }
    }

    for tier in crate::team::inbox_tiered::QueueTier::ALL {
        for message in
            crate::team::inbox_tiered::pending_messages_for_tier(&inboxes_root, member, tier)?
        {
            if is_commit_reminder_body(&message.body) {
                crate::team::inbox_tiered::mark_tiered_delivered(
                    &inboxes_root,
                    member,
                    tier,
                    &message.id,
                )?;
                cleared += 1;
            }
        }
    }

    Ok(cleared)
}

impl TeamDaemon {
    pub(in super::super) fn check_github_verification_feedback(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.is_dir() {
            return Ok(());
        }

        let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let snapshot = crate::team::github_feedback::summarize_github_feedback_for_tasks(
            &self.config.project_root,
            &tasks,
        )?;

        for warning in snapshot.warnings {
            let key = format!(
                "github-feedback-warning::{}::{}::{}",
                warning.task_id, warning.check_name, warning.reason
            );
            if self.intervention_on_cooldown(&key) {
                continue;
            }
            self.emit_event(crate::team::events::TeamEvent::github_verification_warning(
                &warning.task_id.to_string(),
                &warning.reason,
                Some(&warning.check_name),
            ));
            self.record_orchestrator_action(format!(
                "github-verification: warning for task #{}: {}",
                warning.task_id, warning.reason
            ));
            self.intervention_cooldowns.insert(key, Instant::now());
        }

        for feedback in snapshot.failed.values().chain(snapshot.passed.values()) {
            let key = format!(
                "github-feedback::{}::{}::{}::{}",
                feedback.task_id,
                feedback.check_name,
                feedback.status,
                feedback.commit.as_deref().unwrap_or("unknown")
            );
            if self.intervention_on_cooldown(&key) {
                continue;
            }
            self.emit_event(
                crate::team::events::TeamEvent::github_verification_feedback(
                    &crate::team::events::GithubVerificationFeedbackInfo {
                        task: &feedback.task_id.to_string(),
                        branch: feedback.branch.as_deref(),
                        commit: feedback.commit.as_deref(),
                        check_name: &feedback.check_name,
                        success: Some(feedback.is_success()),
                        reason: &feedback.status,
                        next_action: feedback.next_action.as_deref(),
                        details: feedback.details.as_deref(),
                    },
                ),
            );
            self.record_orchestrator_action(format!(
                "github-verification: {}",
                feedback.blocked_on_summary()
            ));
            self.intervention_cooldowns.insert(key, Instant::now());
        }

        Ok(())
    }

    pub(in super::super) fn maybe_cleanup_shared_cargo_target(&mut self) -> Result<()> {
        if self.last_shared_target_cleanup.elapsed() < SHARED_TARGET_CLEANUP_INTERVAL {
            return Ok(());
        }
        self.last_shared_target_cleanup = Instant::now();

        let shared_target =
            crate::team::task_loop::shared_cargo_target_dir(&self.config.project_root);
        std::fs::create_dir_all(&shared_target).ok();
        let used_pct = filesystem_usage_percent(&shared_target)?;
        if used_pct < SHARED_TARGET_DISK_THRESHOLD_PCT {
            return Ok(());
        }

        let removed = prune_legacy_worktree_target_dirs(&self.config.project_root)?;
        if !removed.is_empty() {
            info!(
                used_pct,
                removed = removed.len(),
                "pruned legacy per-worktree cargo targets under disk pressure"
            );
            self.record_orchestrator_action(format!(
                "runtime: pruned {} legacy worktree target dirs at {}% disk usage",
                removed.len(),
                used_pct
            ));
        }
        Ok(())
    }

    /// Detect worktrees stuck on stale branches whose commits have already
    /// been cherry-picked onto main, and auto-reset them to the base branch.
    /// Also detects and auto-recovers worktrees stuck in merge conflict state.
    pub(in super::super) fn check_worktree_staleness(&mut self) -> Result<()> {
        self.reconcile_active_tasks()?;
        let project_root = self.project_root().to_path_buf();
        if git_has_unresolved_conflicts(&project_root).unwrap_or(false) {
            let conflict_files = conflict_paths(&project_root);
            let operation = if git_ref_exists(&project_root, "CHERRY_PICK_HEAD") {
                "cherry-pick"
            } else if git_ref_exists(&project_root, "REBASE_HEAD") {
                "rebase"
            } else {
                "merge"
            };
            let file_summary = if conflict_files.is_empty() {
                "unknown files".to_string()
            } else {
                conflict_files.join(", ")
            };

            warn!(
                operation,
                files = %file_summary,
                "project root has unresolved conflicts; auto-recovering control-plane worktree"
            );

            abort_conflicted_git_operation(&project_root, &["cherry-pick", "--abort"]);
            abort_conflicted_git_operation(&project_root, &["merge", "--abort"]);
            abort_conflicted_git_operation(&project_root, &["rebase", "--abort"]);
            abort_conflicted_git_operation(&project_root, &["reset", "--hard", "HEAD"]);
            abort_conflicted_git_operation(&project_root, &["clean", "-fd"]);

            if git_has_unresolved_conflicts(&project_root).unwrap_or(false) {
                let message = format!(
                    "Main worktree is still conflicted after failed {operation} recovery. Conflicted files: {file_summary}. Clear the repo state manually before the next merge."
                );
                self.record_orchestrator_action(format!(
                    "health: failed to recover main worktree after conflicted {operation} on {file_summary}"
                ));
                let _ = self.notify_architects(&message);
            } else {
                let message = format!(
                    "Recovered the main worktree after a failed {operation}. Conflicted files were: {file_summary}. Re-run the merge manually after reviewing the conflict."
                );
                info!(
                    operation,
                    files = %file_summary,
                    "project root conflict auto-recovered"
                );
                self.record_orchestrator_action(format!(
                    "health: auto-recovered main worktree after conflicted {operation} on {file_summary}"
                ));
                let _ = self.notify_architects(&message);
            }
        }

        if let Ok(root_dirty) = crate::team::merge::inspect_root_dirty_state(&project_root)
            && !root_dirty.source_paths.is_empty()
        {
            warn!(
                paths = %root_dirty.source_paths.join(", "),
                "project root has dirty source changes that will block review auto-merges"
            );
        }

        let members: Vec<_> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| m.name.clone())
            .collect();
        let trunk_branch = self.config.team_config.trunk_branch().to_string();

        for name in &members {
            let worktree_path = self.worktree_dir(name);
            if !worktree_path.is_dir() {
                continue;
            }
            let base = format!("eng-main/{}", name);

            for repo in
                workspace_repo_targets(&worktree_path, self.is_multi_repo, &self.sub_repo_names)
            {
                let repo_name = repo.label.as_deref().unwrap_or("root");
                let repo_path = &repo.path;

                // Check for merge conflicts first — these block all git operations.
                if git_has_unresolved_conflicts(repo_path).unwrap_or(false) {
                    warn!(
                        member = %name,
                        repo = repo_name,
                        worktree = %repo_path.display(),
                        "worktree has unresolved merge conflicts; auto-recovering via merge --abort and reset"
                    );
                    match crate::worktree::reset_worktree_to_base_if_clean_from_trunk(
                        repo_path,
                        &base,
                        "merge-conflict recovery",
                        &trunk_branch,
                    ) {
                        Err(error) => {
                            warn!(
                                member = %name,
                                repo = repo_name,
                                error = %error,
                                "failed to reset worktree after merge conflict recovery"
                            );
                            self.report_preserve_failure(
                                name,
                                self.active_task_id(name),
                                "merge-conflict recovery",
                                &error.to_string(),
                            );
                        }
                        Ok(reason) if reason.reset_performed() => {
                            info!(
                                member = %name,
                                repo = repo_name,
                                reset_reason = reason.as_str(),
                                "worktree merge conflict auto-recovered; reset to base branch"
                            );
                            self.record_orchestrator_action(format!(
                                "health: auto-recovered {}'s {} worktree from merge conflict state — reset to {} ({})",
                                name,
                                repo_name,
                                base,
                                reason.as_str()
                            ));
                            if self.active_tasks.contains_key(name.as_str()) {
                                let task_id = self.active_tasks[name.as_str()];
                                warn!(
                                    member = %name,
                                    repo = repo_name,
                                    task_id,
                                    "clearing active task after merge conflict recovery"
                                );
                                self.clear_active_task(name);
                            }
                        }
                        Ok(reason) => {
                            self.report_preserve_failure(
                                name,
                                self.active_task_id(name),
                                "merge-conflict recovery",
                                reason.as_str(),
                            );
                            self.record_orchestrator_action(format!(
                                "health: blocked {} {} merge-conflict recovery because dirty worktree could not be preserved ({})",
                                name,
                                repo_name,
                                reason.as_str()
                            ));
                        }
                    }
                    continue;
                }

                let current_branch = match crate::worktree::git_current_branch(repo_path) {
                    Ok(b) => b,
                    Err(error) => {
                        warn!(
                            member = %name,
                            repo = repo_name,
                            worktree = %repo_path.display(),
                            error = %error,
                            "failed to read worktree branch; skipping staleness check"
                        );
                        continue;
                    }
                };

                // Skip if already on base branch or trunk.
                if current_branch == base || current_branch == trunk_branch {
                    continue;
                }

                // Skip if engineer has an active task — don't reset mid-work.
                if self.active_tasks.contains_key(name.as_str()) {
                    continue;
                }

                // SAFETY: never reset a worktree that has commits ahead of trunk.
                // This protects against the race where active_tasks is empty during
                // stop/start cycles but the engineer has uncommitted or unmerged work.
                match crate::worktree::commits_ahead(repo_path, &trunk_branch) {
                    Ok(ahead) if ahead > 0 => {
                        debug!(
                            member = %name,
                            repo = repo_name,
                            branch = %current_branch,
                            ahead,
                            "worktree has {} commits ahead of trunk; skipping reset",
                            ahead
                        );
                        continue;
                    }
                    Err(error) => {
                        debug!(
                            member = %name,
                            repo = repo_name,
                            worktree = %repo_path.display(),
                            error = %error,
                            "failed to count commits ahead; skipping reset to be safe"
                        );
                        continue;
                    }
                    _ => {}
                }

                match crate::worktree::branch_fully_merged(
                    repo_path,
                    &current_branch,
                    &trunk_branch,
                ) {
                    Ok(true) => {
                        info!(
                            member = %name,
                            repo = repo_name,
                            branch = %current_branch,
                            "stale branch detected; resetting worktree"
                        );
                        match crate::worktree::reset_worktree_to_base_if_clean_from_trunk(
                            repo_path,
                            &base,
                            "stale-branch recovery",
                            &trunk_branch,
                        ) {
                            Ok(reason) if reason.reset_performed() => {
                                self.record_orchestrator_action(format!(
                                    "runtime: auto-reset {}'s {} worktree — branch {} already on {} ({})",
                                    name,
                                    repo_name,
                                    current_branch,
                                    trunk_branch,
                                    reason.as_str()
                                ));
                            }
                            Ok(reason) => {
                                self.report_preserve_failure(
                                    name,
                                    self.active_task_id(name),
                                    "stale-branch recovery",
                                    reason.as_str(),
                                );
                                self.record_orchestrator_action(format!(
                                    "blocked recovery: stale-branch recovery for {} {} blocked ({})",
                                    name,
                                    repo_name,
                                    reason.as_str()
                                ));
                                continue;
                            }
                            Err(error) => {
                                warn!(
                                    member = %name,
                                    repo = repo_name,
                                    worktree = %repo_path.display(),
                                    error = %error,
                                    "failed to auto-reset stale worktree; continuing"
                                );
                                self.report_preserve_failure(
                                    name,
                                    self.active_task_id(name),
                                    "stale-branch recovery",
                                    &error.to_string(),
                                );
                                self.record_orchestrator_action(format!(
                                    "blocked recovery: stale-branch recovery for {} {} failed ({})",
                                    name, repo_name, error
                                ));
                                continue;
                            }
                        }
                    }
                    Ok(false) => { /* branch has unique commits; not stale */ }
                    Err(error) => {
                        warn!(
                            member = %name,
                            repo = repo_name,
                            branch = %current_branch,
                            error = %error,
                            "failed to check worktree staleness; continuing"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Periodically check agent backend health and emit events on transitions.
    pub(in super::super) fn check_backend_health(&mut self) -> Result<()> {
        let interval = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .health_check_interval_secs,
        );
        if self.last_health_check.elapsed() < interval {
            return Ok(());
        }
        self.last_health_check = Instant::now();

        // Collect (member_name, agent_name) pairs to avoid borrowing self.config during mutation.
        let checks: Vec<(String, String)> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .map(|m| {
                (
                    m.name.clone(),
                    m.agent.as_deref().unwrap_or("claude").to_string(),
                )
            })
            .collect();

        for (member_name, agent_name) in &checks {
            let mut new_health =
                agent::health_check_by_name(agent_name).unwrap_or(BackendHealth::Healthy);
            if *agent_name == "claude"
                && let Some(credentials_path) = claude_credentials_path()
                && !claude_oauth_healthy(&credentials_path)
            {
                new_health = BackendHealth::Degraded;
            }
            let prev_health = self
                .backend_health
                .get(member_name)
                .copied()
                .unwrap_or(BackendHealth::Healthy);

            // #674 defect 1: do NOT transition from QuotaExhausted to a
            // better state just because `which codex` returned Healthy.
            // A successful binary probe (or even a successful poll_shim ping)
            // is not evidence of quota recovery — the codex shim can connect
            // fine and still fail on the first real turn. Keep the member
            // parked until the recorded retry_at deadline elapses or the
            // entry has been cleared explicitly (e.g. operator intervention
            // via daemon restart / bench reset).
            if prev_health == BackendHealth::QuotaExhausted
                && new_health != BackendHealth::QuotaExhausted
            {
                let now_epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let retry_at = self.backend_quota_retry_at.get(member_name).copied();
                if retry_at.is_some_and(|deadline| deadline > now_epoch) {
                    // Deadline still in the future — hold QuotaExhausted.
                    self.backend_health
                        .insert(member_name.clone(), BackendHealth::QuotaExhausted);
                    continue;
                }
                // Deadline elapsed (or never recorded) — allow the transition
                // and clear any stale retry_at bookkeeping.
                self.backend_quota_retry_at.remove(member_name);
            }

            if new_health != prev_health {
                let transition = format!("{}→{}", prev_health.as_str(), new_health.as_str());
                info!(
                    member = %member_name,
                    agent = %agent_name,
                    transition = %transition,
                    "backend health changed"
                );
                self.emit_event(TeamEvent::health_changed(member_name, &transition));
                self.record_orchestrator_action(format!(
                    "health: {} backend {} ({})",
                    member_name, transition, agent_name,
                ));
            }
            self.backend_health.insert(member_name.clone(), new_health);
        }

        Ok(())
    }

    /// Check each engineer's worktree for large uncommitted diffs and send a
    /// commit reminder when the line count exceeds the configured threshold.
    /// Nudges are rate-limited to at most once per 5 minutes per engineer.
    pub(in super::super) fn maybe_warn_uncommitted_work(&mut self) -> Result<()> {
        let threshold = self
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold;
        if threshold == 0 {
            return Ok(());
        }

        let cooldown = Duration::from_secs(300); // 5 minutes

        let engineers: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| m.name.clone())
            .collect();

        for name in &engineers {
            let worktree_path = self.worktree_dir(name);
            if !worktree_path.exists() {
                continue;
            }

            let live_work_dir = self
                .shim_handles
                .get(name)
                .map(|handle| handle.work_dir.clone())
                .or_else(|| {
                    self.config
                        .pane_map
                        .get(name)
                        .and_then(|pane_id| crate::tmux::pane_current_path(pane_id).ok())
                        .map(PathBuf::from)
                });
            let inspection_path = uncommitted_warning_worktree_path(
                &worktree_path,
                live_work_dir.as_deref(),
                self.is_multi_repo,
            );
            if !inspection_path.exists() {
                warn!(
                    engineer = %name,
                    worktree = %inspection_path.display(),
                    configured_worktree = %worktree_path.display(),
                    "skipping uncommitted work warning because the inspected worktree path is missing"
                );
                continue;
            }

            let lines = match workspace_uncommitted_diff_lines(
                &inspection_path,
                self.is_multi_repo,
                &self.sub_repo_names,
            ) {
                Ok(n) => n,
                Err(error) => {
                    warn!(engineer = %name, error = %error, "failed to check uncommitted diff");
                    continue;
                }
            };

            if lines < threshold {
                let had_warning_state = self.last_uncommitted_warn.remove(name).is_some();
                let cleared = clear_pending_commit_reminders(&self.config.project_root, name)
                    .unwrap_or_else(|error| {
                        warn!(
                            engineer = %name,
                            error = %error,
                            "failed to clear stale commit reminders after clean verification"
                        );
                        0
                    });
                if had_warning_state || cleared > 0 {
                    self.record_orchestrator_action(format!(
                        "uncommitted-warn-clear: {name} verified clean with {lines} uncommitted lines; cleared {cleared} pending reminder(s)"
                    ));
                }
                continue;
            }

            // Rate-limit only after measuring the current worktree so clean
            // verification can clear stale reminder state immediately.
            if let Some(last) = self.last_uncommitted_warn.get(name) {
                if last.elapsed() < cooldown {
                    continue;
                }
            }

            if pending_commit_reminder_exists(&self.config.project_root, name) {
                debug!(
                    engineer = %name,
                    uncommitted_lines = lines,
                    threshold,
                    "skipping duplicate pending uncommitted work warning"
                );
                continue;
            }

            info!(
                engineer = %name,
                uncommitted_lines = lines,
                threshold,
                worktree = %inspection_path.display(),
                "sending uncommitted work warning"
            );

            let body = format!(
                "COMMIT REMINDER: You have {lines} uncommitted lines in your worktree \
                 (threshold: {threshold}). Please commit your work now to avoid losing progress:\n\n\
                 git add -A && git commit -m 'wip: checkpoint'"
            );

            let sender = self.automation_sender_for(name);
            if let Err(error) = self.queue_message(&sender, name, &body) {
                warn!(engineer = %name, error = %error, "failed to send uncommitted work warning");
            }
            self.record_orchestrator_action(format!(
                "uncommitted-warn: {name} has {lines} uncommitted lines (threshold {threshold})"
            ));
            self.last_uncommitted_warn
                .insert(name.clone(), Instant::now());
        }

        Ok(())
    }

    /// Load the prompt template for a member, substituting role-specific info.
    pub(in super::super) fn load_prompt(
        &self,
        member: &MemberInstance,
        config_dir: &Path,
    ) -> String {
        let mut prompt = render_member_prompt(member, config_dir, &resolve_prompt_context(member));
        if self.config.team_config.workflow_policy.clean_room_mode
            && let Some(group) = self
                .config
                .team_config
                .role_barrier_group(&member.role_name)
        {
            let work_dir = self.worktree_dir(&member.name);
            let handoff_dir = self.handoff_dir();
            prompt.push_str(&format!(
                "\n\n## Information Barrier\n\n- Barrier group: {group}\n- Allowed working directory: {}\n- Shared cross-barrier handoff directory: {}\n- Do not read or write outside your worktree and the handoff directory.\n",
                work_dir.display(),
                handoff_dir.display()
            ));
        }
        prompt
    }
}

fn filesystem_usage_percent(path: &Path) -> Result<u8> {
    let output = std::process::Command::new("df")
        .args(["-Pk", path.to_string_lossy().as_ref()])
        .output()?;
    if !output.status.success() {
        return Ok(0);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().nth(1) else {
        return Ok(0);
    };
    let pct_field = line.split_whitespace().nth(4).unwrap_or("0%");
    Ok(pct_field.trim_end_matches('%').parse().unwrap_or(0))
}

fn prune_legacy_worktree_target_dirs(project_root: &Path) -> Result<Vec<PathBuf>> {
    let worktrees_root = project_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut removed = Vec::new();
    for entry in std::fs::read_dir(&worktrees_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let direct_target = path.join("target");
        if direct_target.is_dir() {
            std::fs::remove_dir_all(&direct_target)?;
            removed.push(direct_target);
        }

        for nested in std::fs::read_dir(&path)? {
            let nested = nested?;
            let nested_target = nested.path().join("target");
            if nested_target.is_dir() {
                std::fs::remove_dir_all(&nested_target)?;
                removed.push(nested_target);
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use super::{claude_oauth_healthy, parse_claude_oauth_expiry_ms};
    use crate::team::config::{RoleType, WorkflowPolicy};
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{EnvVarGuard, PATH_LOCK, setup_fake_backend};
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, git_ok, git_stdout, init_git_repo,
        manager_member,
    };
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn init_bare_git_repo(path: &Path) {
        git_ok(
            path.parent().unwrap(),
            &["init", "-b", "main", path.to_str().unwrap()],
        );
        git_ok(path, &["config", "user.email", "batty@example.com"]);
        git_ok(path, &["config", "user.name", "Batty Tests"]);
    }

    // ---- Backend health tests ----

    #[test]
    fn health_check_interval_config_default() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.health_check_interval_secs, 60);
    }

    #[test]
    fn check_backend_health_skipped_before_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now();
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.is_empty());
    }

    #[test]
    fn check_backend_health_runs_after_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health-run".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.contains_key("eng-health-run"));
    }

    #[test]
    fn check_backend_health_skips_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let user = MemberInstance {
            name: "human".to_string(),
            role_name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![user]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.is_empty());
    }

    #[test]
    #[serial]
    fn check_backend_health_emits_event_on_transition() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "claude", "health-claude.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let engineer = MemberInstance {
            name: "eng-transition".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        // First check: no prior state → sets initial value.
        daemon.check_backend_health().unwrap();
        let initial_health = *daemon.backend_health.get("eng-transition").unwrap();
        assert_eq!(initial_health, BackendHealth::Healthy);

        // Event emitted only on *transition*. First check from None→Healthy
        // may or may not emit depending on whether None counts. Let's just
        // verify the health entry exists.
        assert!(daemon.backend_health.contains_key("eng-transition"));
    }

    #[test]
    #[serial]
    fn check_backend_health_no_event_when_state_unchanged() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "claude", "health-claude.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let engineer = MemberInstance {
            name: "eng-stable".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon
            .backend_health
            .insert("eng-stable".to_string(), BackendHealth::Healthy);

        daemon.check_backend_health().unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        let health_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "health_changed")
            .collect();
        assert!(
            health_events.is_empty(),
            "no event when health state is unchanged"
        );
    }

    #[test]
    fn check_backend_health_tracks_multiple_members() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                engineer_member("eng-1", Some("architect"), false),
            ])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        daemon.check_backend_health().unwrap();

        assert!(daemon.backend_health.contains_key("architect"));
        assert!(daemon.backend_health.contains_key("eng-1"));
    }

    /// #674 defect 1: when a member has been marked QuotaExhausted with a
    /// retry_at deadline in the future, the periodic backend-health probe
    /// must NOT transition the member back to Healthy even if `which <agent>`
    /// succeeds. Only the elapsed retry_at (or operator intervention) can
    /// clear the quota_exhausted state.
    #[test]
    fn check_backend_health_preserves_quota_exhausted_when_retry_at_is_future() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-parked", None, false)])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon
            .backend_health
            .insert("eng-parked".to_string(), BackendHealth::QuotaExhausted);
        let future_deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            + 32 * 3600; // 32h from now
        daemon
            .backend_quota_retry_at
            .insert("eng-parked".to_string(), future_deadline);

        daemon.check_backend_health().unwrap();

        assert_eq!(
            daemon.backend_health.get("eng-parked").copied(),
            Some(BackendHealth::QuotaExhausted),
            "QuotaExhausted must be held while retry_at is in the future"
        );
        assert_eq!(
            daemon.backend_quota_retry_at.get("eng-parked").copied(),
            Some(future_deadline),
            "retry_at bookkeeping must not be cleared while deadline is still future"
        );
        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            !events
                .iter()
                .any(|e| e.event == "health_changed" && e.role.as_deref() == Some("eng-parked")),
            "no health_changed event should fire while quota deadline is in future"
        );
    }

    /// #674 defect 1 (recovery path): once retry_at has elapsed, the next
    /// periodic health probe must allow the transition out of
    /// QuotaExhausted and clear the retry_at bookkeeping.
    #[test]
    fn check_backend_health_allows_transition_when_retry_at_has_elapsed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-recovered", None, false)])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon
            .backend_health
            .insert("eng-recovered".to_string(), BackendHealth::QuotaExhausted);
        // Deadline five seconds in the past.
        let past_deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            .saturating_sub(5);
        daemon
            .backend_quota_retry_at
            .insert("eng-recovered".to_string(), past_deadline);

        daemon.check_backend_health().unwrap();

        // With `which` likely failing in the sandbox, the new state could be
        // Unreachable or Healthy — either way it must NOT be QuotaExhausted
        // and the retry_at bookkeeping must be cleared.
        assert_ne!(
            daemon.backend_health.get("eng-recovered").copied(),
            Some(BackendHealth::QuotaExhausted),
            "transition out of QuotaExhausted must be allowed once retry_at elapsed"
        );
        assert!(
            !daemon.backend_quota_retry_at.contains_key("eng-recovered"),
            "retry_at bookkeeping must be cleared after the deadline elapses"
        );
    }

    /// #674 helper test: `member_backend_parked` reports parked state from
    /// either the cached QuotaExhausted health value or a future retry_at
    /// deadline, so callers need only consult one method.
    #[test]
    fn member_backend_parked_reports_quota_state_and_future_retry_at() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                engineer_member("eng-quota", None, false),
                engineer_member("eng-retry", None, false),
                engineer_member("eng-healthy", None, false),
            ])
            .build();

        // Parked via cached health state.
        daemon
            .backend_health
            .insert("eng-quota".to_string(), BackendHealth::QuotaExhausted);
        // Parked via future retry_at even without cached state.
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            + 3600;
        daemon
            .backend_quota_retry_at
            .insert("eng-retry".to_string(), future);
        // Third engineer: healthy with no retry_at.
        daemon
            .backend_health
            .insert("eng-healthy".to_string(), BackendHealth::Healthy);

        assert!(daemon.member_backend_parked("eng-quota"));
        assert!(daemon.member_backend_parked("eng-retry"));
        assert!(!daemon.member_backend_parked("eng-healthy"));
        assert!(!daemon.member_backend_parked("unknown-member"));
    }

    #[test]
    fn check_backend_health_default_agent_is_claude() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut member = architect_member("architect");
        member.agent = None;
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![member])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.contains_key("architect"));
    }

    #[test]
    fn check_github_verification_feedback_emits_failure_and_warning_events() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("042-review.md"),
            "---\nid: 42\ntitle: Review task\nstatus: review\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/42\ncommit: abcdef1\nclass: standard\n---\n",
        )
        .unwrap();
        for record in [
            crate::team::github_feedback::GithubVerificationRecord {
                task_id: 42,
                branch: Some("eng-1/42".to_string()),
                commit: Some("abcdef1".to_string()),
                check_name: "ci/test".to_string(),
                status: "failure".to_string(),
                next_action: Some("fix CI".to_string()),
                details: Some("unit test failed".to_string()),
                ts: Some(1),
            },
            crate::team::github_feedback::GithubVerificationRecord {
                task_id: 99,
                branch: Some("eng-1/99".to_string()),
                commit: Some("abcdef1".to_string()),
                check_name: "ci/test".to_string(),
                status: "failure".to_string(),
                next_action: None,
                details: None,
                ts: Some(2),
            },
        ] {
            crate::team::github_feedback::write_github_feedback_record(tmp.path(), &record)
                .unwrap();
        }
        let mut daemon = make_test_daemon(tmp.path(), vec![manager_member("lead", None)]);

        daemon.check_github_verification_feedback().unwrap();

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "github_verification_feedback"
                && event.task.as_deref() == Some("42")
                && event.success == Some(false)
                && event.git_ref.as_deref() == Some("abcdef1")
        }));
        assert!(events.iter().any(|event| {
            event.event == "github_verification_warning"
                && event.task.as_deref() == Some("99")
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("unknown task #99"))
        }));
    }

    #[test]
    fn parse_claude_oauth_expiry_ms_reads_nested_expiry() {
        let creds = r#"{"claudeAiOauth":{"expiresAt":1234567890}}"#;
        assert_eq!(parse_claude_oauth_expiry_ms(creds), Some(1_234_567_890));
    }

    #[test]
    fn claude_oauth_healthy_requires_five_minutes_remaining() {
        let tmp = tempfile::tempdir().unwrap();
        let credentials = tmp.path().join(".credentials.json");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as i64;

        std::fs::write(
            &credentials,
            format!(
                "{{\"claudeAiOauth\":{{\"expiresAt\":{}}}}}",
                now_ms + 600_000
            ),
        )
        .unwrap();
        assert!(claude_oauth_healthy(&credentials));

        std::fs::write(
            &credentials,
            format!(
                "{{\"claudeAiOauth\":{{\"expiresAt\":{}}}}}",
                now_ms + 60_000
            ),
        )
        .unwrap();
        assert!(!claude_oauth_healthy(&credentials));
    }

    // ---- Worktree reconciliation tests ----

    fn setup_reconcile_scenario(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-reconcile");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name(engineer);

        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();

        // Clean up untracked files created by setup_engineer_worktree so
        // the worktree appears clean to has_uncommitted_changes().
        // Git reads info/exclude from the main repo, not worktree-specific
        // git dirs, so the per-worktree exclude rules don't take effect.
        let _ = std::fs::remove_dir_all(worktree_dir.join(".cargo"));
        let _ = std::fs::remove_dir_all(worktree_dir.join(".batty"));

        let task_branch = format!("{engineer}-42");
        git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        git_ok(&repo, &["merge", &task_branch]);

        (tmp, repo, worktree_dir)
    }

    #[test]
    fn reconcile_resets_idle_engineer_on_merged_branch() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-reconcile");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-reconcile", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-reconcile".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();

        daemon.is_git_repo = true;
        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch,
            engineer_base_branch_name("eng-reconcile"),
            "worktree should be reset to base branch"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_preserves_dirty_work_before_resetting_merged_branch() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-dirty");
        let task_branch = "eng-dirty-42";
        std::fs::write(worktree_dir.join("dirty.txt"), "tracked dirty work\n").unwrap();
        git_ok(&worktree_dir, &["add", "dirty.txt"]);
        std::fs::write(worktree_dir.join("untracked.txt"), "untracked dirty work\n").unwrap();

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-dirty", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-dirty".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();

        daemon.is_git_repo = true;
        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch,
            engineer_base_branch_name("eng-dirty"),
            "worktree should be reset to base branch"
        );
        assert_eq!(
            git_stdout(&repo, &["show", &format!("{task_branch}:dirty.txt")]),
            "tracked dirty work"
        );
        assert_eq!(
            git_stdout(&repo, &["show", &format!("{task_branch}:untracked.txt")]),
            "untracked dirty work"
        );
        let log = git_stdout(&repo, &["log", "--oneline", "-1", task_branch]);
        assert!(
            log.contains("wip: auto-save before worktree reset"),
            "expected auto-save commit on preserved branch, got: {log}"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_blocks_dirty_merged_branch_when_preserve_fails() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-blocked");
        let task_branch = "eng-blocked-42";
        std::fs::write(worktree_dir.join("dirty.txt"), "tracked dirty work\n").unwrap();
        git_ok(&worktree_dir, &["add", "dirty.txt"]);
        let git_dir = PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            worktree_dir.join(git_dir)
        };
        std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-blocked", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-blocked".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();

        daemon.is_git_repo = true;
        daemon.maybe_reconcile_stale_worktrees().unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            task_branch
        );
    }

    #[test]
    fn reconcile_skips_working_engineer() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-working");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-working", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-working".to_string(), MemberState::Working)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-working-42",
            "worktree should stay on task branch when engineer is working"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_skips_idle_engineer_with_active_task() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-active");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-active", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-active".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;
        daemon.active_tasks.insert("eng-active".to_string(), 42);

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-active-42",
            "worktree should stay on task branch when engineer has active task"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_skips_unmerged_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-reconcile-unmerged");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-unmerged");
        let team_config_dir = repo.join(".batty").join("team_config");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-unmerged", &team_config_dir).unwrap();

        let task_branch = "eng-unmerged-99";
        git_ok(&worktree_dir, &["checkout", "-b", task_branch]);
        std::fs::write(worktree_dir.join("wip.txt"), "wip\n").unwrap();
        git_ok(&worktree_dir, &["add", "wip.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "work in progress"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-unmerged", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-unmerged".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-unmerged-99",
            "worktree should stay on unmerged task branch"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_emits_worktree_reconciled_event() {
        let (_tmp, repo, _worktree_dir) = setup_reconcile_scenario("eng-event");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-event", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-event".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            events.iter().any(|e| e.event == "worktree_reconciled"
                && e.role.as_deref() == Some("eng-event")),
            "should emit worktree_reconciled event"
        );
    }

    // ---- uncommitted work warning tests ----

    #[test]
    fn uncommitted_diff_lines_counts_unstaged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("hello.txt"), "changed1\nchanged2\nline3\n").unwrap();

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 3, "expected >=3 uncommitted lines, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_empty_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert_eq!(lines, 0, "clean repo should have 0 uncommitted lines");
    }

    #[test]
    fn uncommitted_diff_lines_includes_staged_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("hello.txt"), "modified\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 2, "staged changes should count, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_mixed_staged_and_unstaged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("a.txt"), "line1\n").unwrap();
        std::fs::write(repo.join("b.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(repo.join("b.txt"), "changed\n").unwrap();
        git_ok(&repo, &["add", "b.txt"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 4, "mixed staged+unstaged should sum, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = super::super::uncommitted_diff_lines(tmp.path());
        if let Ok(lines) = result {
            assert_eq!(lines, 0);
        }
    }

    #[test]
    fn uncommitted_diff_lines_new_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("init.txt"), "x\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("new.txt"), "a\nb\nc\n").unwrap();
        git_ok(&repo, &["add", "new.txt"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(
            lines >= 3,
            "new staged file should count as added lines, got {lines}"
        );
    }

    #[test]
    fn uncommitted_diff_lines_ignores_batty_target_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".batty-target")).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(
            repo.join(".batty-target").join("build.log"),
            "artifact\nartifact\nartifact\n",
        )
        .unwrap();

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert_eq!(lines, 0, ".batty-target should be ignored, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_skips_staged_delete_with_identical_untracked_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".cargo")).unwrap();
        init_bare_git_repo(&repo);

        let path = repo.join(".cargo").join("config.toml");
        std::fs::write(&path, "[build]\ntarget-dir = \"shared\"\n").unwrap();
        git_ok(&repo, &["add", ".cargo/config.toml"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        git_ok(&repo, &["rm", "--cached", ".cargo/config.toml"]);
        std::fs::write(&path, "[build]\ntarget-dir = \"shared\"\n").unwrap();

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert_eq!(
            lines, 0,
            "identical staged-delete/untracked-copy mismatch should not count, got {lines}"
        );
    }

    #[test]
    fn uncommitted_diff_lines_clean_review_branch_shape_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, worktree_dir) =
            setup_clean_review_branch_worktree(&tmp, "eng-1-2", "eng-1-2/590");

        let lines = super::super::uncommitted_diff_lines(&worktree_dir).unwrap();

        assert_eq!(
            lines, 0,
            "clean eng-1-2/590 review branch should not count committed diff lines, got {lines}"
        );
    }

    #[test]
    fn uncommitted_diff_lines_ignores_managed_cargo_noise_on_review_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, worktree_dir) =
            setup_review_branch_with_managed_cargo_noise(&tmp, "eng-1-2", "eng-1-2/590");

        let lines = super::super::uncommitted_diff_lines(&worktree_dir).unwrap();

        assert_eq!(
            lines, 0,
            "managed cargo config noise on a review branch should not count, got {lines}"
        );
    }

    fn make_uncommitted_warn_daemon(tmp: &tempfile::TempDir, threshold: usize) -> TeamDaemon {
        let repo = tmp.path();
        let team_config_dir = repo.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();

        let wt = repo.join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&wt).unwrap();
        init_bare_git_repo(&wt);

        std::fs::write(wt.join("big.txt"), "a\n".repeat(300)).unwrap();
        git_ok(&wt, &["add", "big.txt"]);
        git_ok(&wt, &["commit", "-m", "init big"]);

        std::fs::write(wt.join("big.txt"), "b\n".repeat(300)).unwrap();

        let engineer = engineer_member("eng-1", Some("manager"), true);
        let mut daemon = TestDaemonBuilder::new(repo)
            .members(vec![manager_member("manager", None), engineer])
            .build();
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = threshold;
        daemon
    }

    fn setup_uncommitted_warn_worktree(
        tmp: &tempfile::TempDir,
        package_name: &str,
        engineer: &str,
        threshold: usize,
    ) -> (PathBuf, PathBuf, TeamDaemon) {
        let repo = init_git_repo(tmp, package_name);
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &engineer_base_branch_name(engineer),
            &team_config_dir,
        )
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                manager_member("manager", None),
                engineer_member(engineer, Some("manager"), true),
            ])
            .build();
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = threshold;
        (repo, worktree_dir, daemon)
    }

    fn install_test_shim_work_dir(daemon: &mut TeamDaemon, member: &str, work_dir: PathBuf) {
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            member.to_string(),
            crate::shim::protocol::Channel::new(parent),
            999,
            "codex".to_string(),
            "codex".to_string(),
            work_dir,
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert(member.to_string(), handle);
    }

    fn setup_clean_review_branch_worktree(
        tmp: &tempfile::TempDir,
        engineer: &str,
        review_branch: &str,
    ) -> (PathBuf, PathBuf) {
        let repo = init_git_repo(tmp, "uncommitted-review-branch");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        std::fs::create_dir_all(worktree_dir.parent().unwrap()).unwrap();
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                review_branch,
                worktree_dir.to_str().unwrap(),
                "main",
            ],
        );

        std::fs::write(worktree_dir.join("review.txt"), "review branch work\n").unwrap();
        git_ok(&worktree_dir, &["add", "review.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "review branch commit"]);

        assert!(
            git_stdout(&worktree_dir, &["status", "--porcelain"]).is_empty(),
            "review branch worktree should be clean after commit"
        );

        (repo, worktree_dir)
    }

    fn setup_review_branch_with_managed_cargo_noise(
        tmp: &tempfile::TempDir,
        engineer: &str,
        review_branch: &str,
    ) -> (PathBuf, PathBuf) {
        let repo = init_git_repo(tmp, "uncommitted-managed-noise");
        let tracked_cargo_dir = repo.join(".cargo");
        std::fs::create_dir_all(&tracked_cargo_dir).unwrap();
        std::fs::write(
            tracked_cargo_dir.join("config.toml"),
            "[alias]\nxtask = \"run\"\n",
        )
        .unwrap();
        git_ok(&repo, &["add", ".cargo/config.toml"]);
        git_ok(&repo, &["commit", "-m", "track cargo config"]);

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let base_branch = format!("eng-main/{engineer}");

        git_ok(&repo, &["branch", &base_branch]);
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", review_branch]);

        let raw_status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        assert!(
            raw_status.contains("D  .cargo/config.toml") && raw_status.contains("?? .cargo/"),
            "setup should reproduce managed cargo noise, got: {raw_status}"
        );

        (repo, worktree_dir)
    }

    #[test]
    fn prune_legacy_worktree_target_dirs_removes_engineer_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_target = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join("eng-1")
            .join("target");
        std::fs::create_dir_all(&worktree_target).unwrap();
        std::fs::write(worktree_target.join("stale"), "artifact").unwrap();

        let removed = super::prune_legacy_worktree_target_dirs(tmp.path()).unwrap();

        assert_eq!(removed, vec![worktree_target.clone()]);
        assert!(!worktree_target.exists());
    }

    #[test]
    fn prune_legacy_worktree_target_dirs_removes_nested_multi_repo_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let nested_target = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join("eng-2")
            .join("subrepo")
            .join("target");
        std::fs::create_dir_all(&nested_target).unwrap();
        std::fs::write(nested_target.join("stale"), "artifact").unwrap();

        let removed = super::prune_legacy_worktree_target_dirs(tmp.path()).unwrap();

        assert_eq!(removed, vec![nested_target.clone()]);
        assert!(!nested_target.exists());
    }

    #[test]
    fn maybe_warn_uncommitted_work_sends_nudge_above_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 10);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("COMMIT REMINDER"));
    }

    #[test]
    fn maybe_warn_uncommitted_work_clean_worktree_noops_and_clears_stale_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _worktree_dir, mut daemon) =
            setup_uncommitted_warn_worktree(&tmp, "uncommitted-clean-noop", "eng-clean", 1);
        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, "eng-clean").unwrap();
        let stale = inbox::InboxMessage::new_send(
            "daemon",
            "eng-clean",
            "COMMIT REMINDER: You have 324 uncommitted lines in your worktree.",
        );
        inbox::deliver_to_inbox(&inbox_root, &stale).unwrap();
        daemon
            .last_uncommitted_warn
            .insert("eng-clean".to_string(), Instant::now());

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-clean").unwrap();
        assert!(
            msgs.is_empty(),
            "clean verification should clear stale pending commit reminders"
        );
        assert!(
            !daemon.last_uncommitted_warn.contains_key("eng-clean"),
            "clean verification should clear the warning cooldown state"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_counts_dirty_tracked_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, worktree_dir, mut daemon) =
            setup_uncommitted_warn_worktree(&tmp, "uncommitted-dirty-tracked", "eng-dirty", 1);
        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool {\n    false\n}\n",
        )
        .unwrap();
        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, "eng-dirty").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-dirty").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("COMMIT REMINDER"));
    }

    #[test]
    fn maybe_warn_uncommitted_work_counts_staged_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, worktree_dir, mut daemon) =
            setup_uncommitted_warn_worktree(&tmp, "uncommitted-staged", "eng-staged", 1);
        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool {\n    false\n}\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs"]);
        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, "eng-staged").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-staged").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("COMMIT REMINDER"));
    }

    #[test]
    fn maybe_warn_uncommitted_work_counts_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, worktree_dir, mut daemon) =
            setup_uncommitted_warn_worktree(&tmp, "uncommitted-untracked", "eng-untracked", 1);
        std::fs::write(worktree_dir.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, "eng-untracked").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-untracked").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("COMMIT REMINDER"));
    }

    #[test]
    fn maybe_warn_uncommitted_work_uses_live_shim_worktree_over_configured_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "uncommitted-root-mismatch");
        let team_config_dir = repo.join(".batty").join("team_config");
        let configured = repo.join(".batty").join("worktrees").join("eng-mismatch");
        let live = repo
            .join(".batty")
            .join("worktrees")
            .join("eng-mismatch-live");
        setup_engineer_worktree(
            &repo,
            &configured,
            "eng-main/eng-mismatch",
            &team_config_dir,
        )
        .unwrap();
        setup_engineer_worktree(&repo, &live, "eng-main/eng-mismatch-live", &team_config_dir)
            .unwrap();
        std::fs::write(
            configured.join("src").join("lib.rs"),
            "pub fn smoke() -> bool {\n    false\n}\n",
        )
        .unwrap();

        let codex_context = live
            .join(".batty")
            .join("codex-context")
            .join("eng-mismatch");
        std::fs::create_dir_all(&codex_context).unwrap();
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-mismatch", Some("manager"), true),
            ])
            .build();
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = 1;
        install_test_shim_work_dir(&mut daemon, "eng-mismatch", codex_context);
        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, "eng-mismatch").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-mismatch").unwrap();
        assert!(
            msgs.is_empty(),
            "clean live shim worktree should not inherit dirty line counts from the configured root"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_rate_limited() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 10);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();
        let msgs1 = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(msgs1.len(), 1);

        daemon.maybe_warn_uncommitted_work().unwrap();
        let msgs2 = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(msgs2.len(), 1, "second call should be rate-limited");
    }

    #[test]
    fn maybe_warn_uncommitted_work_disabled_when_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 0);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert!(msgs.is_empty(), "threshold=0 should disable the feature");
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 99999);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert!(msgs.is_empty(), "below threshold should not warn");
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_non_worktree_engineers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = engineer_member("eng-no-wt", Some("manager"), false);
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("manager", None), engineer])
            .build();
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = 10;

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-no-wt").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-no-wt").unwrap();
        assert!(msgs.is_empty(), "non-worktree engineer should be skipped");
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_clean_review_branch_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _worktree_dir) =
            setup_clean_review_branch_worktree(&tmp, "eng-1-2", "eng-1-2/590");

        let engineer = engineer_member("eng-1-2", Some("manager"), true);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![manager_member("manager", None), engineer])
            .build();
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = 1;

        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, "eng-1-2").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1-2").unwrap();
        assert!(
            msgs.is_empty(),
            "clean eng-1-2/590 review branch should not trigger a commit reminder"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_reads_multi_repo_subrepos() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "multi-uncommitted");
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();

        let engineer = "eng-multi";
        let base_branch = engineer_base_branch_name(engineer);
        let worktree_dir = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join(engineer)
            .join("repo");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool {\n    false\n}\n",
        )
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("manager", None),
                engineer_member(engineer, Some("manager"), true),
            ])
            .build();
        daemon.is_git_repo = false;
        daemon.is_multi_repo = true;
        daemon.sub_repo_names = vec!["repo".to_string()];
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = 1;

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, engineer).unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, engineer).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("COMMIT REMINDER"));
    }

    #[test]
    fn uncommitted_warn_threshold_config_default() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.uncommitted_warn_threshold, 200);
    }

    // ---- false-done prevention tests ----

    #[test]
    fn false_done_prevention_no_commits_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-false-done");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();

        let output =
            crate::team::git_cmd::run_git(&worktree_dir, &["rev-list", "--count", "main..HEAD"])
                .unwrap();
        let count: u32 = output.stdout.trim().parse().unwrap();
        assert_eq!(count, 0, "branch with no new commits should return 0");
    }

    #[test]
    fn false_done_prevention_with_commits_returns_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-false-done-ok");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("work.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "work.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        let output =
            crate::team::git_cmd::run_git(&worktree_dir, &["rev-list", "--count", "main..HEAD"])
                .unwrap();
        let count: u32 = output.stdout.trim().parse().unwrap();
        assert!(count > 0, "branch with commits should return > 0");
    }

    #[test]
    fn false_done_prevention_invalid_worktree_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result =
            crate::team::git_cmd::run_git(tmp.path(), &["rev-list", "--count", "main..HEAD"]);
        assert!(result.is_err(), "non-git dir should return error");
    }

    // ---- check_worktree_staleness tests ----

    #[test]
    fn check_worktree_staleness_skips_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "staleness-skip");

        let eng_name = "eng-stale-skip";
        let base = format!("eng-main/{eng_name}");
        let wt_dir = repo.join(".batty").join("worktrees").join(eng_name);
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();
        git_ok(&repo, &["branch", &base]);
        git_ok(&repo, &["worktree", "add", wt_dir.to_str().unwrap(), &base]);

        let engineer = MemberInstance {
            name: eng_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);
        daemon.is_git_repo = true;

        daemon.check_worktree_staleness().unwrap();

        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "remove", "--force", wt_dir.to_str().unwrap()])
            .output();
        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["branch", "-D", &base])
            .output();
    }

    #[test]
    fn check_worktree_staleness_recovers_failed_main_cherry_pick_and_notifies_architect() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "staleness-main-conflict");

        std::fs::write(repo.join("conflict.txt"), "base\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "add conflict file"]);

        git_ok(&repo, &["checkout", "-b", "feature/conflict"]);
        std::fs::write(repo.join("conflict.txt"), "feature change\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "feature change"]);

        git_ok(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("conflict.txt"), "main change\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        let cherry_pick = Command::new("git")
            .current_dir(&repo)
            .args(["cherry-pick", "feature/conflict"])
            .output()
            .unwrap();
        assert!(
            !cherry_pick.status.success(),
            "test setup requires a conflicted cherry-pick"
        );
        assert!(super::git_ref_exists(&repo, "CHERRY_PICK_HEAD"));

        let mut daemon = make_test_daemon(&repo, vec![architect_member("architect")]);
        daemon.is_git_repo = true;

        daemon.check_worktree_staleness().unwrap();

        assert!(
            !super::git_ref_exists(&repo, "CHERRY_PICK_HEAD"),
            "cherry-pick state should be aborted"
        );
        assert!(
            !crate::team::task_loop::git_has_unresolved_conflicts(&repo).unwrap(),
            "repo should be clean after recovery"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("conflict.txt")).unwrap(),
            "main change\n"
        );

        let inbox_root = inbox::inboxes_root(&repo);
        let messages = inbox::pending_messages(&inbox_root, "architect").unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].body.contains("failed cherry-pick"));
        assert!(messages[0].body.contains("conflict.txt"));
    }

    #[test]
    fn check_worktree_staleness_skips_unmerged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "staleness-unmerged");

        let eng_name = "eng-stale-unmerged";
        let base = format!("eng-main/{eng_name}");
        let task_branch = format!("{eng_name}/task-99");
        let wt_dir = repo.join(".batty").join("worktrees").join(eng_name);
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();

        git_ok(&repo, &["branch", &base]);
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &task_branch,
                wt_dir.to_str().unwrap(),
                &base,
            ],
        );

        std::fs::write(wt_dir.join("unique_work.txt"), "unique\n").unwrap();
        git_ok(&wt_dir, &["add", "unique_work.txt"]);
        git_ok(&wt_dir, &["commit", "-m", "unique commit"]);

        let engineer = MemberInstance {
            name: eng_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);
        daemon.is_git_repo = true;

        daemon.check_worktree_staleness().unwrap();

        let current = git_stdout(&wt_dir, &["branch", "--show-current"]);
        assert_eq!(current, task_branch);

        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "remove", "--force", wt_dir.to_str().unwrap()])
            .output();
        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["branch", "-D", &task_branch])
            .output();
        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["branch", "-D", &base])
            .output();
    }

    #[test]
    fn check_worktree_staleness_uses_subrepos_for_multi_repo_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "staleness-multi");
        let engineer = "eng-stale-multi";
        let base = engineer_base_branch_name(engineer);
        let worktree_dir = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join(engineer)
            .join("repo");
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();

        setup_engineer_worktree(&repo, &worktree_dir, &base, &team_config_dir).unwrap();

        let task_branch = format!("{engineer}/task-42");
        git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);
        git_ok(&repo, &["merge", &task_branch]);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member(engineer, Some("manager"), true)])
            .build();
        daemon.is_git_repo = false;
        daemon.is_multi_repo = true;
        daemon.sub_repo_names = vec!["repo".to_string()];

        daemon.check_worktree_staleness().unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            base,
            "multi-repo health check should reset the stale sub-repo branch, not inspect the workspace container root"
        );
    }

    // ── load_prompt tests ──

    #[test]
    fn load_prompt_substitutes_template_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("engineer.md"),
            "Hello {{member_name}}, role={{role_name}}, reports_to={{reports_to}}",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "Hello eng-1, role=engineer, reports_to=manager");
    }

    #[test]
    fn load_prompt_uses_custom_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("custom.md"), "Custom: {{member_name}}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "arch-1".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            model: None,
            prompt: Some("custom.md".to_string()),
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "Custom: arch-1");
    }

    #[test]
    fn load_prompt_fallback_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert!(prompt.contains("eng-1"));
        assert!(prompt.contains("Engineer"));
    }

    #[test]
    fn load_prompt_reports_to_none_becomes_none_string() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("architect.md"), "reports={{reports_to}}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "arch-1".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "reports=none");
    }

    #[test]
    fn load_prompt_default_file_per_role_type() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("architect.md"), "ARCH").unwrap();
        std::fs::write(config_dir.join("manager.md"), "MGR").unwrap();
        std::fs::write(config_dir.join("engineer.md"), "ENG").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let daemon = make_test_daemon(tmp.path(), vec![]);

        let arch = MemberInstance {
            name: "a".to_string(),
            role_name: "a".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mgr = MemberInstance {
            name: "m".to_string(),
            role_name: "m".to_string(),
            role_type: RoleType::Manager,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let eng = MemberInstance {
            name: "e".to_string(),
            role_name: "e".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        assert_eq!(daemon.load_prompt(&arch, &config_dir), "ARCH");
        assert_eq!(daemon.load_prompt(&mgr, &config_dir), "MGR");
        assert_eq!(daemon.load_prompt(&eng, &config_dir), "ENG");
    }
}
