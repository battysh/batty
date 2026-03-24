//! Agent health monitoring, lifecycle management, and restart logic.
//!
//! Extracted from daemon.rs: watcher polling, context exhaustion,
//! stall detection, pane death, backend health, startup preflight.
//!
//! Decomposed into focused submodules:
//! - `preflight` — startup preflight and pane readiness
//! - `restart` — dead member restart and pane death handling
//! - `context_exhaustion` — context exhaustion restart and escalation
//! - `stall` — stall detection, restart, and escalation
//! - `checks` — backend health, worktree staleness, uncommitted work, prompt loading
//! - `poll_watchers` — watcher polling and state transitions

use std::time::Duration;

use super::*;
use anyhow::{Context, Result};

mod checks;
mod context_exhaustion;
mod ping_pong;
mod poll_shim;
mod poll_watchers;
mod preflight;
mod restart;
mod stall;

pub(super) const CONTEXT_RESTART_COOLDOWN: Duration = Duration::from_secs(30);
const STARTUP_PREFLIGHT_RESPAWN_DELAY: Duration = Duration::from_millis(200);

/// Format checkpoint content for inclusion in a restart notice.
///
/// Wraps the checkpoint content with `[RESUMING FROM CHECKPOINT]` and
/// `[END CHECKPOINT]` markers so the restarted agent can parse it.
fn format_checkpoint_section(cp_content: &str) -> String {
    format!("\n\n[RESUMING FROM CHECKPOINT]\n{cp_content}\n[END CHECKPOINT]")
}

impl TeamDaemon {
    #[allow(dead_code)]
    pub(super) fn handle_pane_death(&mut self, member_name: &str) -> Result<()> {
        self.restart_member(member_name)
    }

    fn active_task(&self, member_name: &str) -> Result<Option<crate::task::Task>> {
        let Some(task_id) = self.active_task_id(member_name) else {
            return Ok(None);
        };
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        Ok(tasks.into_iter().find(|task| task.id == task_id))
    }

    pub(super) fn context_restart_cooldown_key(member_name: &str) -> String {
        format!("context-restart::{member_name}")
    }

    pub(super) fn context_escalation_cooldown_key(member_name: &str) -> String {
        format!("context-escalation::{member_name}")
    }

    fn context_restart_count(&self, task_id: u32) -> Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let task_id = task_id.to_string();
        let count = super::super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .count() as u32;
        Ok(count)
    }

    pub(super) fn restart_assignment_message(task: &crate::task::Task) -> String {
        let mut message = format!(
            "Continuing Task #{}: {}\nPrevious session exhausted context; resume from the current worktree state and continue.\n\n{}",
            task.id, task.title, task.description
        );
        if let Some(branch) = task.branch.as_deref() {
            message.push_str(&format!("\n\nBranch: {branch}"));
        }
        if let Some(worktree_path) = task.worktree_path.as_deref() {
            message.push_str(&format!("\nWorktree: {worktree_path}"));
        }
        message
    }
}

/// Count total inserted + deleted lines from uncommitted changes in a worktree.
/// Runs `git diff --numstat` (unstaged) + `git diff --cached --numstat` (staged).
fn uncommitted_diff_lines(worktree: &std::path::Path) -> Result<usize> {
    let mut total = 0usize;
    for extra_args in [&["--numstat"] as &[&str], &["--cached", "--numstat"]] {
        let output = std::process::Command::new("git")
            .arg("diff")
            .args(extra_args)
            .current_dir(worktree)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .with_context(|| format!("failed to run git diff in {}", worktree.display()))?;
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let mut parts = line.split_whitespace();
            let added: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let removed: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            total += added + removed;
        }
    }
    Ok(total)
}

#[cfg(test)]
pub(super) mod test_helpers {
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use std::path::PathBuf;
    use std::sync::{LazyLock, Mutex};

    pub static PATH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    pub struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        pub fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.original.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    pub fn setup_fake_kanban(tmp: &tempfile::TempDir, script_name: &str) -> PathBuf {
        let fake_bin = tmp.path().join(format!("{script_name}-bin"));
        std::fs::create_dir_all(&fake_bin).unwrap();
        let fake_kanban = fake_bin.join("kanban-md");
        std::fs::write(
            &fake_kanban,
            r#"#!/bin/bash
if [ "$1" = "--help" ]; then
  echo "kanban-md fake help"
  exit 0
fi
if [ "$1" = "init" ]; then
  shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "--dir" ]; then
      shift
      mkdir -p "$1/tasks"
      exit 0
    fi
    shift
  done
fi
echo "unsupported fake kanban invocation" >&2
exit 1
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_kanban, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake_bin
    }

    pub fn test_team_config(name: &str) -> TeamConfig {
        TeamConfig {
            name: name.to_string(),
            agent: None,
            workflow_mode: WorkflowMode::Legacy,
            workflow_policy: WorkflowPolicy::default(),
            board: BoardConfig::default(),
            standup: StandupConfig::default(),
            automation: AutomationConfig::default(),
            automation_sender: None,
            external_senders: Vec::new(),
            orchestrator_pane: true,
            orchestrator_position: OrchestratorPosition::Bottom,
            layout: None,
            cost: Default::default(),
            grafana: Default::default(),
            use_shim: false,
            auto_respawn_on_crash: false,
            shim_health_check_interval_secs: 60,
            shim_health_timeout_secs: 120,
            shim_shutdown_timeout_secs: 30,
            event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
            retro_min_duration_secs: 60,
            roles: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::team::events::TeamEvent;
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{TestDaemonBuilder, manager_member, write_owned_task_file};
    use std::path::PathBuf;

    #[test]
    fn test_retry_count_increments_and_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        daemon.active_tasks.insert("eng-1".into(), 42);
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.active_task_id("eng-2"), None);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
        assert_eq!(daemon.increment_retry("eng-1"), 2);
        daemon.clear_active_task("eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
    }

    #[test]
    fn test_retry_count_triggers_escalation_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        daemon.active_tasks.insert("eng-1".into(), 42);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
        assert_eq!(daemon.increment_retry("eng-1"), 2);
        assert_eq!(daemon.increment_retry("eng-1"), 3);
        daemon.clear_active_task("eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn test_active_task_id_returns_none_for_unassigned() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();

        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn nonfatal_kanban_failures_are_relayed_to_known_members() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("manager", None)])
            .build();

        daemon.report_nonfatal_kanban_failure(
            "move task #42 to done",
            "kanban-md stderr goes here",
            ["manager"],
        );

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(messages[0].body.contains("move task #42 to done"));
        assert!(messages[0].body.contains("kanban-md stderr goes here"));
    }

    // ── restart_assignment_message tests ──

    #[test]
    fn restart_assignment_message_includes_task_id_and_title() {
        let task = crate::task::Task {
            id: 42,
            title: "implement widget".to_string(),
            description: "Add the new widget feature.".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".into()),
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
            batty_config: None,
            source_path: PathBuf::from("/tmp/task-42.md"),
        };
        let msg = TeamDaemon::restart_assignment_message(&task);
        assert!(msg.contains("Task #42"));
        assert!(msg.contains("implement widget"));
        assert!(msg.contains("Add the new widget feature."));
        assert!(msg.contains("Previous session exhausted context"));
        // No branch or worktree lines when those fields are None.
        assert!(!msg.contains("Branch:"));
        assert!(!msg.contains("Worktree:"));
    }

    #[test]
    fn restart_assignment_message_includes_branch_and_worktree() {
        let task = crate::task::Task {
            id: 99,
            title: "fix tests".to_string(),
            description: "Fix failing tests.".to_string(),
            status: "in-progress".to_string(),
            priority: "medium".to_string(),
            claimed_by: Some("eng-2".into()),
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: Some("/tmp/worktrees/eng-2".to_string()),
            branch: Some("eng-2/99".to_string()),
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            batty_config: None,
            source_path: PathBuf::from("/tmp/task-99.md"),
        };
        let msg = TeamDaemon::restart_assignment_message(&task);
        assert!(msg.contains("Branch: eng-2/99"));
        assert!(msg.contains("Worktree: /tmp/worktrees/eng-2"));
    }

    // ── cooldown key tests ──

    #[test]
    fn context_restart_cooldown_key_format() {
        assert_eq!(
            TeamDaemon::context_restart_cooldown_key("eng-1"),
            "context-restart::eng-1"
        );
    }

    #[test]
    fn context_escalation_cooldown_key_format() {
        assert_eq!(
            TeamDaemon::context_escalation_cooldown_key("eng-1"),
            "context-escalation::eng-1"
        );
    }

    // ── context_restart_count tests ──

    #[test]
    fn context_restart_count_returns_zero_with_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let daemon = make_test_daemon(tmp.path(), vec![]);
        assert_eq!(daemon.context_restart_count(42).unwrap(), 0);
    }

    #[test]
    fn context_restart_count_counts_all_reasons_for_task() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
                TeamEvent::agent_restarted("eng-1", "42", "stalled", 1),
                TeamEvent::agent_restarted("eng-1", "99", "context_exhausted", 1),
            ],
        );

        let daemon = make_test_daemon(tmp.path(), vec![]);
        // context_restart_count counts ALL agent_restarted events for the task
        // (not filtered by reason, unlike stall_restart_count)
        assert_eq!(daemon.context_restart_count(42).unwrap(), 2);
        assert_eq!(daemon.context_restart_count(99).unwrap(), 1);
        assert_eq!(daemon.context_restart_count(100).unwrap(), 0);
    }

    // ── active_task tests ──

    #[test]
    fn active_task_returns_none_when_no_active_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let daemon = make_test_daemon(tmp.path(), vec![]);
        let result = daemon.active_task("eng-1").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn active_task_returns_none_when_task_id_not_on_board() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        daemon.active_tasks.insert("eng-1".to_string(), 999);
        let result = daemon.active_task("eng-1").unwrap();
        assert!(result.is_none(), "nonexistent task should return None");
    }

    #[test]
    fn active_task_returns_task_when_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        write_owned_task_file(tmp.path(), 42, "my-task", "in-progress", "eng-1");

        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        daemon.active_tasks.insert("eng-1".to_string(), 42);
        let result = daemon.active_task("eng-1").unwrap();
        assert!(result.is_some());
        let task = result.unwrap();
        assert_eq!(task.id, 42);
        assert_eq!(task.title, "my-task");
    }

    // ── format_checkpoint_section tests ──

    #[test]
    fn restart_includes_checkpoint() {
        let cp_content = "# Progress Checkpoint: eng-1-1\n\n**Task:** #42 — Fix widget\n";
        let section = super::format_checkpoint_section(cp_content);
        assert!(
            section.contains("[RESUMING FROM CHECKPOINT]"),
            "must contain opening marker"
        );
        assert!(
            section.contains("[END CHECKPOINT]"),
            "must contain closing marker"
        );
        assert!(
            section.contains(cp_content),
            "must include the checkpoint content verbatim"
        );
    }

    #[test]
    fn handles_missing_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = crate::team::checkpoint::read_checkpoint(tmp.path(), "eng-no-such-role");
        assert!(cp.is_none(), "missing checkpoint must return None");

        let mut notice = String::from("Restarted after context exhaustion.");
        if let Some(cp_content) = cp {
            notice.push_str(&super::format_checkpoint_section(&cp_content));
        }
        assert!(
            !notice.contains("[RESUMING FROM CHECKPOINT]"),
            "no checkpoint marker when checkpoint is missing"
        );
        assert!(
            !notice.contains("[END CHECKPOINT]"),
            "no end marker when checkpoint is missing"
        );
    }

    #[test]
    fn content_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = crate::team::checkpoint::Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 77,
            task_title: "Checkpoint round-trip".to_string(),
            task_description: "Verify checkpoint content survives the round-trip.".to_string(),
            branch: Some("eng-1-1/77".to_string()),
            last_commit: Some("deadbeef checkpoint test".to_string()),
            test_summary: Some("test result: ok. 3 passed".to_string()),
            timestamp: "2026-03-22T14:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(tmp.path(), &cp).unwrap();

        let read_back = crate::team::checkpoint::read_checkpoint(tmp.path(), "eng-1-1").unwrap();
        let section = super::format_checkpoint_section(&read_back);

        let start_marker = "[RESUMING FROM CHECKPOINT]\n";
        let end_marker = "\n[END CHECKPOINT]";
        let start = section.find(start_marker).expect("missing start marker") + start_marker.len();
        let end = section.find(end_marker).expect("missing end marker");
        let extracted = &section[start..end];

        assert_eq!(
            extracted, read_back,
            "content between markers must match the checkpoint file"
        );
        assert!(extracted.contains("**Task:** #77"));
        assert!(extracted.contains("eng-1-1/77"));
        assert!(extracted.contains("deadbeef checkpoint test"));
        assert!(extracted.contains("3 passed"));
    }
}
