//! Periodic automation subsystems extracted from daemon.rs.
//!
//! Review timeout, dependency unblocking, pipeline starvation,
//! worktree reconciliation, board rotation, cron, retrospectives.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::helpers::MemberWorktreeContext;
use super::*;

impl TeamDaemon {
    pub(super) fn reconcile_active_tasks(&mut self) -> Result<()> {
        if self.active_tasks.is_empty() {
            return Ok(());
        }
        let tasks_dir = self.board_dir().join("tasks");
        let board_tasks = if tasks_dir.exists() {
            crate::task::load_tasks_from_dir(&tasks_dir)?
        } else {
            Vec::new()
        };
        let stale: Vec<(String, u32)> = self
            .active_tasks
            .iter()
            .filter(|(_engineer, task_id)| {
                let task_id = **task_id;
                match board_tasks.iter().find(|t| t.id == task_id) {
                    Some(task) => task.status == "done" || task.status == "archived",
                    None => true, // task no longer exists
                }
            })
            .map(|(engineer, task_id)| (engineer.clone(), *task_id))
            .collect();
        for (engineer, task_id) in stale {
            info!(
                engineer = %engineer,
                task_id,
                "Reconciled stale active_task: {engineer} was tracking done task #{task_id}"
            );
            self.clear_active_task(&engineer);
        }
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

        // Prune tracking maps for tasks no longer in review
        self.review_first_seen
            .retain(|id, _| review_task_ids.contains(id));
        self.review_nudge_sent
            .retain(|id| review_task_ids.contains(id));

        for task in &tasks {
            if task.status != "review" {
                continue;
            }

            let first_seen = *self.review_first_seen.entry(task.id).or_insert(now);
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
                let _ = self.queue_daemon_message(reviewer, &msg);
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

        // Suppress if manager is actively working (likely processing directives)
        let manager_working = self.config.members.iter().any(|m| {
            m.role_type == RoleType::Manager
                && self.states.get(&m.name) == Some(&MemberState::Working)
        });
        if manager_working {
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
        self.pipeline_starvation_fired = true;
        self.pipeline_starvation_last_fired = Some(Instant::now());
        Ok(())
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
            if branch == base_branch || branch == "HEAD" {
                continue;
            }

            let merged = match branch_is_merged_into(&self.config.project_root, &branch, "main") {
                Ok(m) => m,
                Err(_) => continue,
            };

            if !merged {
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

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::team::config::RoleType;
    use crate::team::events::TeamEvent;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{
        TestDaemonBuilder, write_board_task_file, write_owned_task_file,
    };

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
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
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
    fn reconcile_active_tasks_noop_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), Vec::new());
        // No active tasks — should return immediately
        daemon.reconcile_active_tasks().unwrap();
        assert!(daemon.active_tasks.is_empty());
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
        };
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
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
        };
        let daemon = make_test_daemon(tmp.path(), vec![engineer]);

        let task = crate::task::Task {
            id: 10,
            title: "test".to_string(),
            status: "blocked".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".to_string()),
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
        };
        let daemon = make_test_daemon(tmp.path(), vec![manager]);

        let task = crate::task::Task {
            id: 10,
            title: "test".to_string(),
            status: "blocked".to_string(),
            priority: "high".to_string(),
            claimed_by: None,
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
        };
        let daemon = make_test_daemon(tmp.path(), vec![manager]);

        let task = crate::task::Task {
            id: 10,
            title: "test".to_string(),
            status: "blocked".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("unknown-eng".to_string()),
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
        };
        let eng2 = MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let eng3 = MemberInstance {
            name: "eng-3".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };

        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![eng1, eng2, eng3])
            .states(states)
            .build();

        // eng-2 has an in-progress task on the board
        let tasks = vec![crate::task::Task {
            id: 1,
            title: "active-task".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-2".to_string()),
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
        };

        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![eng1])
            .states(states)
            .build();

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
        use crate::team::config::WorkflowPolicy;
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
        };
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
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
        assert!(
            pending_manager
                .iter()
                .any(|msg| msg.body.contains("Review nudge")),
            "manager should receive nudge"
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
        };
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
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
        };
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
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
        };
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let eng1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
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
}
