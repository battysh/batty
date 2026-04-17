//! Utilities for proactively tracking context pressure and preserving restart state.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::task::Task;
use crate::team::checkpoint::{self, Checkpoint, RestartContext};

const DEFAULT_THRESHOLD_PCT: u8 = 80;
const DEFAULT_CONTEXT_LIMIT_TOKENS: usize = 128_000;
const STATUS_LINE_LIMIT: usize = 20;
const TEST_OUTPUT_LINE_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    GracefulHandoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPressure {
    pub message_count: usize,
    pub estimated_tokens: usize,
    pub threshold_pct: u8,
}

impl Default for ContextPressure {
    fn default() -> Self {
        Self {
            message_count: 0,
            estimated_tokens: 0,
            threshold_pct: DEFAULT_THRESHOLD_PCT,
        }
    }
}

impl ContextPressure {
    pub fn new(message_count: usize, estimated_tokens: usize) -> Self {
        Self {
            message_count,
            estimated_tokens,
            ..Self::default()
        }
    }

    fn usage_pct(&self) -> usize {
        self.estimated_tokens.saturating_mul(100) / DEFAULT_CONTEXT_LIMIT_TOKENS.max(1)
    }
}

pub fn estimate_token_usage(output_bytes: usize) -> usize {
    output_bytes.div_ceil(4)
}

pub fn check_context_pressure(pressure: &ContextPressure) -> Option<ContextAction> {
    (pressure.usage_pct() >= pressure.threshold_pct as usize)
        .then_some(ContextAction::GracefulHandoff)
}

pub fn create_checkpoint(worktree: &Path, task_id: u32) -> Result<Checkpoint> {
    let role = worktree_role(worktree)?;
    let project_root = project_root_from_worktree(worktree)?;
    let checkpoint = Checkpoint {
        role,
        task_id,
        task_title: format!("Task #{task_id}"),
        task_description: build_state_summary(worktree, task_id),
        branch: git_output(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        last_commit: git_output(worktree, &["log", "-1", "--oneline"]),
        test_summary: last_test_output(worktree),
        timestamp: timestamp_now(),
    };
    checkpoint::write_checkpoint(&project_root, &checkpoint)?;
    Ok(checkpoint)
}

pub fn stage_restart_context(
    worktree: &Path,
    role: &str,
    task: &Task,
    reason: &str,
    restart_count: u32,
    output_bytes: Option<u64>,
) -> Result<RestartContext> {
    let context = RestartContext {
        role: role.to_string(),
        task_id: task.id,
        task_title: task.title.clone(),
        task_description: task.description.clone(),
        branch: task
            .branch
            .clone()
            .or_else(|| git_output(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])),
        worktree_path: Some(worktree.display().to_string()),
        restart_count,
        reason: reason.to_string(),
        output_bytes,
        last_commit: git_output(worktree, &["rev-parse", "HEAD"]),
        created_at_epoch_secs: Some(epoch_now_secs()),
        handoff_consumed: false,
    };
    checkpoint::write_restart_context(worktree, &context)?;
    Ok(context)
}

pub fn consume_restart_context(worktree: &Path) -> Result<Option<RestartContext>> {
    let Some(mut context) = checkpoint::read_restart_context(worktree) else {
        return Ok(None);
    };
    context.handoff_consumed = true;
    checkpoint::write_restart_context(worktree, &context)?;
    Ok(Some(context))
}

pub fn clear_restart_context(worktree: &Path) {
    checkpoint::remove_restart_context(worktree);
}

pub fn clear_proactive_restart_context_if_stale(
    worktree: &Path,
    output_bytes: u64,
    cooldown: Duration,
) -> bool {
    let Some(context) = checkpoint::read_restart_context(worktree) else {
        return false;
    };
    if context.reason != "context_pressure" {
        return false;
    }

    let commit_changed = git_output(worktree, &["rev-parse", "HEAD"]) != context.last_commit;
    let cooldown_elapsed = context
        .created_at_epoch_secs
        .map(|started| epoch_now_secs().saturating_sub(started) >= cooldown.as_secs())
        .unwrap_or(true);
    if output_bytes > 0 || commit_changed || cooldown_elapsed {
        checkpoint::remove_restart_context(worktree);
        return true;
    }
    false
}

pub fn proactive_restart_is_suppressed(
    worktree: &Path,
    output_bytes: u64,
    cooldown: Duration,
) -> bool {
    let Some(context) = checkpoint::read_restart_context(worktree) else {
        return false;
    };
    if context.reason != "context_pressure" || !context.handoff_consumed {
        return false;
    }

    let commit_changed = git_output(worktree, &["rev-parse", "HEAD"]) != context.last_commit;
    let cooldown_elapsed = context
        .created_at_epoch_secs
        .map(|started| epoch_now_secs().saturating_sub(started) >= cooldown.as_secs())
        .unwrap_or(true);
    output_bytes == 0 && !commit_changed && !cooldown_elapsed
}

fn worktree_role(worktree: &Path) -> Result<String> {
    worktree
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .context("worktree path must end with the member role")
}

fn project_root_from_worktree(worktree: &Path) -> Result<PathBuf> {
    let worktrees_dir = worktree
        .parent()
        .context("worktree path must be inside .batty/worktrees/<role>")?;
    if worktrees_dir.file_name().and_then(|name| name.to_str()) != Some("worktrees") {
        bail!("worktree path must be inside .batty/worktrees/<role>");
    }

    let batty_dir = worktrees_dir
        .parent()
        .context("worktree path must be inside .batty/worktrees/<role>")?;
    if batty_dir.file_name().and_then(|name| name.to_str()) != Some(".batty") {
        bail!("worktree path must be inside .batty/worktrees/<role>");
    }

    batty_dir
        .parent()
        .map(Path::to_path_buf)
        .context("could not locate project root from worktree path")
}

fn build_state_summary(worktree: &Path, task_id: u32) -> String {
    let mut sections = vec![format!(
        "Resume task #{task_id} from the current worktree state at {}.",
        worktree.display()
    )];

    if let Some(status) = git_status_summary(worktree) {
        sections.push(format!("## Git Status\n\n{status}"));
    }

    if let Some(test_summary) = last_test_output(worktree) {
        sections.push(format!("## Recent Test Output\n\n{test_summary}"));
    }

    sections.join("\n\n")
}

fn git_status_summary(worktree: &Path) -> Option<String> {
    let status = git_output(worktree, &["status", "--short"])?;
    let lines: Vec<&str> = status.lines().take(STATUS_LINE_LIMIT).collect();
    if lines.is_empty() {
        Some(String::from("clean working tree"))
    } else {
        Some(lines.join("\n"))
    }
}

fn git_output(worktree: &Path, args: &[&str]) -> Option<String> {
    if !worktree.exists() {
        return None;
    }
    let output = Command::new("git")
        .args(args)
        .current_dir(worktree)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "HEAD" {
        None
    } else {
        Some(value)
    }
}

fn last_test_output(worktree: &Path) -> Option<String> {
    let output_path = worktree.join(".batty_test_output");
    let content = std::fs::read_to_string(output_path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(TEST_OUTPUT_LINE_LIMIT);
    let summary = lines[start..].join("\n");
    (!summary.is_empty()).then_some(summary)
}

fn timestamp_now() -> String {
    let secs = epoch_now_secs();
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    let seconds = secs % 60;
    let days_since_epoch = secs / 86400;
    let (year, month, day) = epoch_days_to_date(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn epoch_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let status = Command::new("git")
            .args(["init"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .args(["config", "user.name", "Batty Test"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .args(["config", "user.email", "batty@example.com"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn commit_file(repo: &Path, rel_path: &str, content: &str, message: &str) {
        let file_path = repo.join(rel_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file_path, content).unwrap();
        let status = Command::new("git")
            .args(["add", rel_path])
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn estimate_token_usage_uses_four_chars_per_token() {
        assert_eq!(estimate_token_usage(0), 0);
        assert_eq!(estimate_token_usage(4), 1);
        assert_eq!(estimate_token_usage(5), 2);
    }

    #[test]
    fn threshold_detection_returns_graceful_handoff_at_default_threshold() {
        let pressure = ContextPressure::new(24, 102_400);
        assert_eq!(
            check_context_pressure(&pressure),
            Some(ContextAction::GracefulHandoff)
        );
    }

    #[test]
    fn threshold_detection_stays_idle_below_limit() {
        let pressure = ContextPressure::new(8, 90_000);
        assert_eq!(check_context_pressure(&pressure), None);
    }

    #[test]
    fn create_checkpoint_persists_restart_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let worktree = project_root
            .join(".batty")
            .join("worktrees")
            .join("eng-1-2");
        init_git_repo(&worktree);
        commit_file(&worktree, "src/lib.rs", "pub fn ready() {}\n", "initial");
        std::fs::write(
            worktree.join(".batty_test_output"),
            "test a ... ok\ntest b ... ok\n",
        )
        .unwrap();
        std::fs::write(worktree.join("notes.txt"), "pending change\n").unwrap();

        let checkpoint = create_checkpoint(&worktree, 453).unwrap();

        assert_eq!(checkpoint.role, "eng-1-2");
        assert_eq!(checkpoint.task_id, 453);
        assert!(
            matches!(checkpoint.branch.as_deref(), Some("master") | Some("main")),
            "unexpected branch: {:?}",
            checkpoint.branch
        );
        assert!(
            checkpoint
                .last_commit
                .as_deref()
                .unwrap()
                .contains("initial")
        );
        assert!(
            checkpoint
                .task_description
                .contains("Resume task #453 from the current worktree state")
        );
        assert!(checkpoint.task_description.contains("notes.txt"));

        let stored = checkpoint::read_checkpoint(project_root, "eng-1-2").unwrap();
        assert!(stored.contains("Task #453"));
        assert!(stored.contains("Recent Test Output"));
    }

    #[test]
    fn create_checkpoint_requires_project_root_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let err = create_checkpoint(tmp.path(), 1).unwrap_err();
        assert!(err.to_string().contains(".batty/worktrees"));
    }

    fn make_task(id: u32) -> Task {
        Task {
            id,
            title: format!("Task #{id}"),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("eng-1-2".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: vec![],
            depends_on: vec![],
            review_owner: None,
            blocked_on: None,
            worktree_path: Some("/tmp/worktree".to_string()),
            branch: Some(format!("eng-1-2/{id}")),
            commit: None,
            artifacts: vec![],
            next_action: Some("Finish the implementation.".to_string()),
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: "Continue from the saved state.".to_string(),
            batty_config: None,
            source_path: PathBuf::from("/tmp/task.md"),
        }
    }

    #[test]
    fn staged_restart_context_round_trips_and_marks_consumed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("eng-1-2");
        init_git_repo(&repo);
        commit_file(&repo, "src/lib.rs", "pub fn ready() {}\n", "initial");
        let task = make_task(77);

        let staged =
            stage_restart_context(&repo, "eng-1-2", &task, "context_pressure", 2, Some(256))
                .unwrap();
        assert_eq!(staged.reason, "context_pressure");
        assert_eq!(staged.restart_count, 2);
        assert_eq!(staged.output_bytes, Some(256));
        assert!(!staged.handoff_consumed);

        let consumed = consume_restart_context(&repo).unwrap().unwrap();
        assert!(consumed.handoff_consumed);
        assert_eq!(consumed.last_commit, staged.last_commit);
    }

    #[test]
    fn proactive_restart_is_suppressed_until_progress_or_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("eng-1-2");
        init_git_repo(&repo);
        commit_file(&repo, "src/lib.rs", "pub fn ready() {}\n", "initial");
        let task = make_task(88);

        stage_restart_context(&repo, "eng-1-2", &task, "context_pressure", 1, Some(0)).unwrap();
        let consumed = consume_restart_context(&repo).unwrap().unwrap();
        assert!(consumed.handoff_consumed);

        assert!(proactive_restart_is_suppressed(
            &repo,
            0,
            Duration::from_secs(30)
        ));
        assert!(!clear_proactive_restart_context_if_stale(
            &repo,
            0,
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn proactive_restart_guard_clears_after_new_output() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("eng-1-2");
        init_git_repo(&repo);
        commit_file(&repo, "src/lib.rs", "pub fn ready() {}\n", "initial");
        let task = make_task(89);

        stage_restart_context(&repo, "eng-1-2", &task, "context_pressure", 1, Some(0)).unwrap();
        consume_restart_context(&repo).unwrap();

        assert!(clear_proactive_restart_context_if_stale(
            &repo,
            12,
            Duration::from_secs(30)
        ));
        assert!(checkpoint::read_restart_context(&repo).is_none());
    }

    #[test]
    fn proactive_restart_guard_clears_after_new_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("eng-1-2");
        init_git_repo(&repo);
        commit_file(&repo, "src/lib.rs", "pub fn ready() {}\n", "initial");
        let task = make_task(90);

        stage_restart_context(&repo, "eng-1-2", &task, "context_pressure", 1, Some(0)).unwrap();
        consume_restart_context(&repo).unwrap();
        commit_file(
            &repo,
            "src/lib.rs",
            "pub fn ready() { println!(\"ok\"); }\n",
            "follow-up",
        );

        assert!(clear_proactive_restart_context_if_stale(
            &repo,
            0,
            Duration::from_secs(30)
        ));
        assert!(!proactive_restart_is_suppressed(
            &repo,
            0,
            Duration::from_secs(30)
        ));
    }
}
