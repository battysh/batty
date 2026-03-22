//! Periodic automation subsystems extracted from daemon.rs.
//!
//! Review timeout, dependency unblocking, pipeline starvation,
//! worktree reconciliation, board rotation, cron, retrospectives.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::{info, warn};

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
            let nudge_threshold = super::super::policy::effective_nudge_threshold(&policy, &task.priority);
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

    pub(super) fn auto_unblock_notification_recipient(&self, task: &crate::task::Task) -> Option<String> {
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

    pub(super) fn member_worktree_context(&self, member_name: &str) -> Option<MemberWorktreeContext> {
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
        if !self.is_git_repo {
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

            let merged = match branch_is_merged_into(
                &self.config.project_root,
                &branch,
                "main",
            ) {
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

        let events = super::super::events::read_events(&events_path).unwrap();
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

        let events = super::super::events::read_events(&events_path).unwrap();
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

        let events = super::super::events::read_events(&events_path).unwrap();
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

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
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

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
        );
    }
}
