use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use tracing::warn;

use super::*;

const AUTO_DOCTOR_INTERVAL_CYCLES: u64 = 10;
const AUTO_DOCTOR_DONE_ARCHIVE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoFixAction {
    pub(crate) action_type: String,
    pub(crate) task_id: Option<u32>,
    pub(crate) engineer: Option<String>,
    pub(crate) details: String,
}

impl TeamDaemon {
    pub(crate) fn run_auto_doctor(&mut self) -> Result<Vec<AutoFixAction>> {
        if self.poll_cycle_count % AUTO_DOCTOR_INTERVAL_CYCLES != 0 {
            return Ok(Vec::new());
        }

        let mut actions = Vec::new();
        actions.extend(self.auto_doctor_reset_orphaned_in_progress()?);
        actions.extend(self.auto_doctor_reclaim_stale_claims()?);
        actions.extend(self.auto_doctor_archive_done_tasks()?);
        actions.extend(self.auto_doctor_recreate_missing_worktrees()?);
        actions.extend(self.auto_doctor_detect_dependency_cycles()?);

        if !actions.is_empty() {
            self.notify_auto_doctor_summary(&actions);
        }

        Ok(actions)
    }

    pub(in super::super) fn auto_doctor_reset_orphaned_in_progress(
        &mut self,
    ) -> Result<Vec<AutoFixAction>> {
        let tasks = self.load_board_tasks()?;
        let mut actions = Vec::new();

        for task in tasks
            .into_iter()
            .filter(|task| task.status == "in-progress")
        {
            let Some(engineer) = task.claimed_by.as_deref() else {
                continue;
            };
            let is_engineer =
                self.config.members.iter().any(|member| {
                    member.name == engineer && member.role_type == RoleType::Engineer
                });
            let has_matching_assignment = self.active_task_id(engineer) == Some(task.id);
            if is_engineer && has_matching_assignment {
                continue;
            }

            // #683: after a hot-reload, `active_tasks` is cleared so the
            // board becomes the source of truth. If a task is still
            // in-progress and claimed by a valid engineer, trust that
            // claim and re-attach rather than resetting to todo. Resetting
            // used to drop the task back into the dispatch pool and cause
            // immediate misroutes to peers on the next tick — wasting
            // engineer context on a task that was intentionally parked.
            if is_engineer && self.active_task_id(engineer).is_none() {
                self.active_tasks
                    .insert(engineer.to_string(), task.id);
                let details = format!(
                    "re-attached in-progress task #{} to {} from board state (post hot-reload)",
                    task.id, engineer
                );
                self.log_auto_doctor_action(
                    "orphaned_in_progress_reattached",
                    Some(task.id),
                    Some(engineer),
                    details,
                    &mut actions,
                );
                continue;
            }

            let details = if is_engineer {
                format!(
                    "reset orphaned in-progress task #{}, daemon active assignment for {} was {:?}",
                    task.id,
                    engineer,
                    self.active_task_id(engineer)
                )
            } else {
                format!(
                    "reset orphaned in-progress task #{} claimed by unknown engineer {}",
                    task.id, engineer
                )
            };
            crate::team::task_cmd::reclaim_task_claim(
                &self.board_dir(),
                task.id,
                "Reset by auto-doctor after daemon lost active ownership.",
            )?;
            self.clear_active_task(engineer);
            // #684: same dispatch-cooldown pattern as runtime orphan rescue
            // — don't immediately re-dispatch a task that was just reset.
            self.recently_rescued_tasks.insert(task.id, Instant::now());
            self.log_auto_doctor_action(
                "orphaned_in_progress_reset",
                Some(task.id),
                Some(engineer),
                details,
                &mut actions,
            );
        }

        Ok(actions)
    }

    fn auto_doctor_reclaim_stale_claims(&mut self) -> Result<Vec<AutoFixAction>> {
        let tasks = self.load_board_tasks()?;
        let now = Utc::now();
        let mut actions = Vec::new();

        for task in tasks
            .into_iter()
            .filter(|task| task.status == "in-progress")
        {
            let Some(engineer) = task.claimed_by.as_deref() else {
                continue;
            };
            let Some(expires_at) =
                task_claim_expiry(&task, self.claim_ttl_secs_for_priority(&task.priority))
            else {
                continue;
            };
            if expires_at > now || task_has_claim_progress(&task, &self.worktree_dir(engineer)) {
                continue;
            }

            let details = format!(
                "reclaimed stale claim for task #{} from {} after expiry at {}",
                task.id,
                engineer,
                expires_at.to_rfc3339()
            );
            crate::team::task_cmd::reclaim_task_claim(
                &self.board_dir(),
                task.id,
                "Reclaimed by auto-doctor after claim TTL expired with no progress.",
            )?;
            self.clear_active_task(engineer);
            self.log_auto_doctor_action(
                "stale_claim_reclaimed",
                Some(task.id),
                Some(engineer),
                details,
                &mut actions,
            );
        }

        Ok(actions)
    }

    fn auto_doctor_archive_done_tasks(&mut self) -> Result<Vec<AutoFixAction>> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.is_dir() {
            return Ok(Vec::new());
        }

        let old_done = board::done_tasks_older_than(&board_dir, AUTO_DOCTOR_DONE_ARCHIVE_AGE)?;
        if old_done.is_empty() {
            return Ok(Vec::new());
        }

        board::archive_tasks(&board_dir, &old_done, false)?;
        let mut actions = Vec::new();
        for task in old_done {
            let details = format!("archived done task #{} after 24h", task.id);
            self.record_board_task_archived(task.id, task.claimed_by.as_deref());
            self.log_auto_doctor_action(
                "done_task_archived",
                Some(task.id),
                task.claimed_by.as_deref(),
                details,
                &mut actions,
            );
        }
        Ok(actions)
    }

    fn auto_doctor_recreate_missing_worktrees(&mut self) -> Result<Vec<AutoFixAction>> {
        let tasks = self.load_board_tasks()?;
        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let mut actions = Vec::new();

        for task in tasks
            .into_iter()
            .filter(|task| task.status == "in-progress")
        {
            let Some(engineer) = task.claimed_by.as_deref() else {
                continue;
            };
            if !self.member_uses_worktrees(engineer) {
                continue;
            }

            let worktree_dir = self.worktree_dir(engineer);
            if worktree_dir.exists() {
                continue;
            }

            let base_branch = engineer_base_branch_name(engineer);
            setup_engineer_worktree(
                &self.config.project_root,
                &worktree_dir,
                &base_branch,
                &team_config_dir,
            )?;
            let details = format!(
                "recreated missing worktree for {} at {} from {}",
                engineer,
                worktree_dir.display(),
                base_branch
            );
            self.log_auto_doctor_action(
                "missing_worktree_recreated",
                Some(task.id),
                Some(engineer),
                details,
                &mut actions,
            );
        }

        Ok(actions)
    }

    fn auto_doctor_detect_dependency_cycles(&mut self) -> Result<Vec<AutoFixAction>> {
        let tasks = self.load_board_tasks()?;
        let Some(cycle) = crate::team::deps::detect_cycle_for_tasks(&tasks) else {
            return Ok(Vec::new());
        };

        let details = format!(
            "dependency cycle detected: {}",
            cycle
                .iter()
                .map(|task_id| format!("#{task_id}"))
                .collect::<Vec<_>>()
                .join(" -> ")
        );
        warn!("{details}");
        let mut actions = Vec::new();
        self.log_auto_doctor_action(
            "dependency_cycle_detected",
            None,
            None,
            details,
            &mut actions,
        );
        Ok(actions)
    }

    fn load_board_tasks(&self) -> Result<Vec<crate::task::Task>> {
        let tasks_dir = self.board_dir().join("tasks");
        if !tasks_dir.is_dir() {
            return Ok(Vec::new());
        }
        crate::task::load_tasks_from_dir(&tasks_dir)
    }

    fn log_auto_doctor_action(
        &mut self,
        action_type: &str,
        task_id: Option<u32>,
        engineer: Option<&str>,
        details: String,
        actions: &mut Vec<AutoFixAction>,
    ) {
        self.record_auto_doctor_action(action_type, task_id, engineer, &details);
        self.record_orchestrator_action(format!("auto-doctor: {action_type} — {details}"));
        actions.push(AutoFixAction {
            action_type: action_type.to_string(),
            task_id,
            engineer: engineer.map(str::to_string),
            details,
        });
    }

    fn notify_auto_doctor_summary(&mut self, actions: &[AutoFixAction]) {
        let managers: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Manager)
            .map(|member| member.name.clone())
            .collect();
        if managers.is_empty() {
            return;
        }

        let mut lines = vec![format!(
            "Auto-doctor applied {} board health fix(es):",
            actions.len()
        )];
        lines.extend(actions.iter().map(|action| {
            let mut parts = vec![action.action_type.clone()];
            if let Some(task_id) = action.task_id {
                parts.push(format!("#{}", task_id));
            }
            if let Some(engineer) = action.engineer.as_deref() {
                parts.push(engineer.to_string());
            }
            parts.push(action.details.clone());
            format!("- {}", parts.join(" | "))
        }));
        let body = lines.join("\n");

        for manager in managers {
            if let Err(error) = self.queue_daemon_message(&manager, &body) {
                warn!(manager, error = %error, "failed to send auto-doctor summary");
            }
        }
    }
}

fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn task_claim_expiry(task: &crate::task::Task, default_ttl_secs: u64) -> Option<DateTime<Utc>> {
    if let Some(expires_at) = task.claim_expires_at.as_deref().and_then(parse_rfc3339_utc) {
        return Some(expires_at);
    }

    let claimed_at = task.claimed_at.as_deref().and_then(parse_rfc3339_utc)?;
    let ttl_secs = task.claim_ttl_secs.unwrap_or(default_ttl_secs);
    Some(claimed_at + chrono::Duration::seconds(ttl_secs as i64))
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

fn task_has_claim_progress(task: &crate::task::Task, work_dir: &std::path::Path) -> bool {
    let Some(last_progress_at) = task.last_progress_at.as_deref().and_then(parse_rfc3339_utc)
    else {
        return false;
    };
    if latest_commit_timestamp(work_dir).is_some_and(|ts| ts > last_progress_at) {
        return true;
    }
    if crate::team::git_cmd::has_user_changes(work_dir).unwrap_or(false) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::WorkflowPolicy;
    use crate::team::events::read_events;
    use crate::team::task_cmd::{set_optional_string, set_optional_u64, update_task_frontmatter};
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, init_git_repo, manager_member, write_board_task_file,
        write_owned_task_file,
    };

    fn auto_doctor_daemon(repo: &std::path::Path, use_worktrees: bool) -> TeamDaemon {
        let manager = manager_member("manager", None);
        let engineer = engineer_member("eng-1", Some("manager"), use_worktrees);
        TestDaemonBuilder::new(repo)
            .members(vec![manager, engineer])
            .workflow_policy(WorkflowPolicy {
                auto_archive_done_after_secs: Some(24 * 60 * 60),
                ..WorkflowPolicy::default()
            })
            .build()
    }

    fn set_cycle_ready(daemon: &mut TeamDaemon) {
        daemon.poll_cycle_count = AUTO_DOCTOR_INTERVAL_CYCLES;
    }

    #[test]
    fn orphaned_task_claimed_by_valid_engineer_reattaches() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_reattach");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_owned_task_file(&repo, 17, "orphaned", "in-progress", "eng-1");

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
        let task = tasks.into_iter().find(|task| task.id == 17).unwrap();
        assert_eq!(task.status, "in-progress");
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1"));
        assert_eq!(daemon.active_tasks.get("eng-1"), Some(&17));
        assert!(
            actions
                .iter()
                .any(|action| action.action_type == "orphaned_in_progress_reattached"
                    && action.task_id == Some(17))
        );
    }

    #[test]
    fn orphaned_task_claimed_by_unknown_engineer_still_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_unknown_claim");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_owned_task_file(&repo, 19, "ghost-claim", "in-progress", "ghost-user");

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
        let task = tasks.into_iter().find(|task| task.id == 19).unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(task.claimed_by, None);
        assert!(
            actions
                .iter()
                .any(|action| action.action_type == "orphaned_in_progress_reset"
                    && action.task_id == Some(19))
        );
    }

    #[test]
    fn stale_claim_gets_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_stale_claim");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_owned_task_file(&repo, 23, "stale-claim", "in-progress", "eng-1");
        daemon.active_tasks.insert("eng-1".to_string(), 23);

        let stale_time = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        update_task_frontmatter(
            &daemon.board_dir().join("tasks").join("023-stale-claim.md"),
            |mapping| {
                set_optional_string(mapping, "claimed_at", Some(&stale_time));
                set_optional_u64(mapping, "claim_ttl_secs", Some(60));
                set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
                set_optional_string(mapping, "last_progress_at", Some(&stale_time));
            },
        )
        .unwrap();

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        let task = crate::task::Task::from_file(
            &daemon.board_dir().join("tasks").join("023-stale-claim.md"),
        )
        .unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(task.claimed_by, None);
        assert!(
            actions
                .iter()
                .any(|action| action.action_type == "stale_claim_reclaimed"
                    && action.task_id == Some(23))
        );
    }

    #[test]
    fn done_task_archived_after_24h() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_archive_old");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_board_task_file(&repo, 31, "done-old", "done", None, &[], None);
        update_task_frontmatter(
            &daemon.board_dir().join("tasks").join("031-done-old.md"),
            |mapping| {
                set_optional_string(
                    mapping,
                    "completed",
                    Some(&(Utc::now() - chrono::Duration::hours(30)).to_rfc3339()),
                );
            },
        )
        .unwrap();

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        assert!(
            !daemon
                .board_dir()
                .join("tasks")
                .join("031-done-old.md")
                .exists()
        );
        assert!(
            daemon
                .board_dir()
                .join("archive")
                .join("031-done-old.md")
                .exists()
        );
        assert!(
            actions
                .iter()
                .any(|action| action.action_type == "done_task_archived"
                    && action.task_id == Some(31))
        );
    }

    #[test]
    fn recent_done_task_not_archived() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_archive_recent");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_board_task_file(&repo, 32, "done-recent", "done", None, &[], None);
        update_task_frontmatter(
            &daemon.board_dir().join("tasks").join("032-done-recent.md"),
            |mapping| {
                set_optional_string(
                    mapping,
                    "completed",
                    Some(&(Utc::now() - chrono::Duration::hours(2)).to_rfc3339()),
                );
            },
        )
        .unwrap();

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        assert!(
            daemon
                .board_dir()
                .join("tasks")
                .join("032-done-recent.md")
                .exists()
        );
        assert!(
            actions
                .iter()
                .all(|action| action.task_id != Some(32)
                    || action.action_type != "done_task_archived")
        );
    }

    #[test]
    fn missing_worktree_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_worktree");
        let mut daemon = auto_doctor_daemon(&repo, true);
        write_owned_task_file(&repo, 41, "missing-worktree", "in-progress", "eng-1");
        daemon.active_tasks.insert("eng-1".to_string(), 41);

        let worktree_dir = daemon.worktree_dir("eng-1");
        assert!(!worktree_dir.exists());

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        assert!(worktree_dir.exists());
        assert!(actions.iter().any(|action| {
            action.action_type == "missing_worktree_recreated" && action.task_id == Some(41)
        }));
    }

    #[test]
    fn dependency_cycle_detected_and_logged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_cycle");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_board_task_file(&repo, 51, "task-a", "todo", None, &[52], None);
        write_board_task_file(&repo, 52, "task-b", "todo", None, &[51], None);

        set_cycle_ready(&mut daemon);
        let actions = daemon.run_auto_doctor().unwrap();

        let events = read_events(&crate::team::team_events_path(&repo)).unwrap();
        assert!(
            actions
                .iter()
                .any(|action| action.action_type == "dependency_cycle_detected")
        );
        assert!(events.iter().any(|event| {
            event.event == "auto_doctor_action"
                && event.action_type.as_deref() == Some("dependency_cycle_detected")
        }));
    }

    #[test]
    fn auto_doctor_skipped_on_non_10th_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_skip");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_owned_task_file(&repo, 61, "skip-task", "in-progress", "eng-1");
        daemon.poll_cycle_count = AUTO_DOCTOR_INTERVAL_CYCLES - 1;

        let actions = daemon.run_auto_doctor().unwrap();

        let task = crate::task::Task::from_file(
            &daemon.board_dir().join("tasks").join("061-skip-task.md"),
        )
        .unwrap();
        assert!(actions.is_empty());
        assert_eq!(task.status, "in-progress");
    }

    #[test]
    fn auto_doctor_runs_on_10th_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto_doctor_run");
        let mut daemon = auto_doctor_daemon(&repo, false);
        write_owned_task_file(&repo, 62, "run-task", "in-progress", "ghost-user");
        set_cycle_ready(&mut daemon);

        let actions = daemon.run_auto_doctor().unwrap();

        assert!(!actions.is_empty());
        let task =
            crate::task::Task::from_file(&daemon.board_dir().join("tasks").join("062-run-task.md"))
                .unwrap();
        assert_eq!(task.status, "todo");
    }
}
