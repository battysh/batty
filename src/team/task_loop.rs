//! Task-loop helpers extracted from the team daemon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use super::git_cmd;
use super::retry::{RetryConfig, retry_sync};
use super::test_results::{self, TestRunOutput};

const SHARED_CARGO_CONFIG_MARKER: &str = "# Managed by Batty: shared cargo target";
const WORKTREE_EXCLUDE_MARKER: &str = "# Managed by Batty worktree ignores";
const REVIEW_READY_SCOPE_FENCE: &[&str] = &[
    "src/team/task_loop.rs",
    "src/team/completion.rs",
    "src/team/review.rs",
];
const MIN_REVIEW_READY_PRODUCTION_ADDITIONS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorktreeRefreshAction {
    Unchanged,
    SkippedDirty,
    Rebased,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeRefreshOutcome {
    pub(crate) action: WorktreeRefreshAction,
    pub(crate) behind_main: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffStatEntry {
    pub(crate) path: String,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitValidationGate {
    pub(crate) blockers: Vec<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn priority_rank(p: &str) -> u32 {
    match p {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn next_unclaimed_task(board_dir: &Path) -> Result<Option<crate::task::Task>> {
    let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
    let task_status_by_id: HashMap<u32, String> = tasks
        .iter()
        .map(|task| (task.id, task.status.clone()))
        .collect();

    let mut available: Vec<crate::task::Task> = tasks
        .into_iter()
        .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
        .filter(|task| task.claimed_by.is_none())
        .filter(|task| task.blocked.is_none())
        .filter(|task| task.blocked_on.is_none())
        .filter(|task| {
            task.depends_on.iter().all(|dep_id| {
                task_status_by_id
                    .get(dep_id)
                    .is_none_or(|status| status == "done")
            })
        })
        .collect();

    available.sort_by_key(|task| (priority_rank(&task.priority), task.id));
    Ok(available.into_iter().next())
}

pub(crate) fn run_tests_in_worktree(
    worktree_dir: &Path,
    test_command: Option<&str>,
) -> Result<TestRunOutput> {
    let command_text = test_command.unwrap_or("cargo test");
    let mut command = std::process::Command::new("sh");
    let cargo_home = engineer_worktree_project_root(worktree_dir)
        .map(|project_root| project_root.join(".batty").join("cargo-home"))
        .unwrap_or_else(|| worktree_dir.join(".batty").join("cargo-home"));
    std::fs::create_dir_all(&cargo_home)
        .with_context(|| format!("failed to create {}", cargo_home.display()))?;
    command
        .arg("-lc")
        .arg(command_text)
        .current_dir(worktree_dir);
    command.env("CARGO_HOME", &cargo_home);
    if let Some(project_root) = engineer_worktree_project_root(worktree_dir) {
        let wt_name = worktree_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".to_string());
        command.env(
            "CARGO_TARGET_DIR",
            shared_cargo_target_dir(&project_root).join(&wt_name),
        );
    }
    let output = command.output().with_context(|| {
        format!(
            "failed while running `{command_text}` in engineer worktree {}",
            worktree_dir.display(),
        )
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut combined = String::new();
    combined.push_str(&stdout);
    if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&stderr);

    let lines: Vec<&str> = combined.lines().collect();
    let trimmed = if lines.len() > 50 {
        lines[lines.len() - 50..].join("\n")
    } else {
        combined
    };

    let passed = output.status.success();
    Ok(TestRunOutput {
        passed,
        results: test_results::parse(command_text, &trimmed, passed),
        output: trimmed,
    })
}

pub(crate) fn shared_cargo_target_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("shared-target")
}

pub(crate) fn validate_review_ready_worktree(worktree_dir: &Path) -> Result<Vec<String>> {
    let diff = map_git_error(
        retry_git(|| git_cmd::run_git(worktree_dir, &["diff", "--stat", "main..HEAD"])),
        "failed to inspect engineer branch diff",
    )?;
    Ok(validate_review_ready_diff_stat(&diff.stdout).blockers)
}

pub(crate) fn validate_review_ready_diff_stat(diff_stat: &str) -> CommitValidationGate {
    let entries = parse_diff_stat_entries(diff_stat);
    let mut blockers = Vec::new();

    if entries.is_empty() {
        blockers.push("engineer branch has no diff against main".to_string());
        return CommitValidationGate { blockers };
    }

    let out_of_scope = entries
        .iter()
        .filter(|entry| !REVIEW_READY_SCOPE_FENCE.contains(&entry.path.as_str()))
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    if !out_of_scope.is_empty() {
        blockers.push(format!(
            "changes outside task scope fence: {}",
            out_of_scope.join(", ")
        ));
    }

    let production_entries = entries
        .iter()
        .filter(|entry| {
            REVIEW_READY_SCOPE_FENCE.contains(&entry.path.as_str()) && entry.path.ends_with(".rs")
        })
        .collect::<Vec<_>>();
    let production_additions: usize = production_entries.iter().map(|entry| entry.additions).sum();
    let production_deletions: usize = production_entries.iter().map(|entry| entry.deletions).sum();

    if production_additions < MIN_REVIEW_READY_PRODUCTION_ADDITIONS {
        blockers.push(format!(
            "need at least {MIN_REVIEW_READY_PRODUCTION_ADDITIONS} lines of production Rust added; found {production_additions}"
        ));
    }
    if production_deletions > production_additions {
        blockers.push(format!(
            "production Rust diff is net-destructive ({production_additions} additions, {production_deletions} deletions)"
        ));
    }

    CommitValidationGate { blockers }
}

fn parse_diff_stat_entries(diff_stat: &str) -> Vec<DiffStatEntry> {
    diff_stat
        .lines()
        .filter_map(|line| {
            let (path, summary) = line.split_once('|')?;
            let path = path.trim();
            if path.is_empty() {
                return None;
            }

            let additions = summary.chars().filter(|ch| *ch == '+').count();
            let deletions = summary.chars().filter(|ch| *ch == '-').count();
            Some(DiffStatEntry {
                path: path.to_string(),
                additions,
                deletions,
            })
        })
        .collect()
}

fn retry_git<T, F>(operation: F) -> std::result::Result<T, git_cmd::GitError>
where
    F: Fn() -> std::result::Result<T, git_cmd::GitError>,
{
    retry_sync(&RetryConfig::fast(), operation)
}

fn map_git_error<T>(result: std::result::Result<T, git_cmd::GitError>, action: &str) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("{action}: {error}"))
}

pub(crate) fn read_task_title(board_dir: &Path, task_id: u32) -> String {
    let tasks_dir = board_dir.join("tasks");
    let prefix = format!("{task_id:03}-");
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix)
                && name.ends_with(".md")
                && let Ok(content) = std::fs::read_to_string(entry.path())
            {
                for line in content.lines() {
                    if line.starts_with("title:") {
                        return line
                            .trim_start_matches("title:")
                            .trim()
                            .trim_matches(|c| c == '"' || c == '\'')
                            .to_string();
                    }
                }
            }
        }
    }
    format!("Task #{task_id}")
}

/// Set up a git worktree for an engineer with symlinked shared config.
pub(crate) fn setup_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
) -> Result<PathBuf> {
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if !worktree_dir.exists() {
        let path = worktree_dir.to_string_lossy().to_string();
        match retry_git(|| git_cmd::worktree_add(project_root, worktree_dir, branch_name, "main")) {
            Ok(_) => {}
            Err(git_cmd::GitError::Permanent { stderr, .. })
                if stderr.contains("already exists") =>
            {
                map_git_error(
                    retry_git(|| {
                        git_cmd::run_git(project_root, &["worktree", "add", &path, branch_name])
                    }),
                    "failed to create git worktree",
                )?;
            }
            Err(error) => {
                return Err(anyhow::anyhow!("failed to create git worktree: {error}"));
            }
        }

        info!(worktree = %worktree_dir.display(), branch = branch_name, "created engineer worktree");
    }

    ensure_engineer_worktree_links(worktree_dir, team_config_dir)?;
    ensure_shared_cargo_target_config(project_root, worktree_dir)?;
    ensure_engineer_worktree_excludes(worktree_dir)?;

    Ok(worktree_dir.to_path_buf())
}

pub(crate) fn prepare_engineer_assignment_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
    task_branch: &str,
    team_config_dir: &Path,
) -> Result<PathBuf> {
    let base_branch = engineer_base_branch_name(engineer_name);
    ensure_engineer_worktree_health(project_root, worktree_dir, &base_branch)?;
    setup_engineer_worktree(project_root, worktree_dir, &base_branch, team_config_dir)?;
    maybe_migrate_legacy_engineer_worktree(
        project_root,
        worktree_dir,
        engineer_name,
        &base_branch,
    )?;
    ensure_task_branch_namespace_available(project_root, engineer_name)?;

    if worktree_has_user_changes(worktree_dir)? {
        auto_clean_worktree(worktree_dir)?;
    }

    let previous_branch = current_worktree_branch(worktree_dir)?;
    if previous_branch != base_branch
        && previous_branch != engineer_name
        && previous_branch != task_branch
        && !branch_is_merged_into(project_root, &previous_branch, "main")?
    {
        bail!(
            "engineer worktree '{}' is on unmerged branch '{}'",
            engineer_name,
            previous_branch
        );
    }

    checkout_worktree_branch_from_main(worktree_dir, &base_branch)?;

    checkout_worktree_branch_from_main(worktree_dir, task_branch)?;
    ensure_engineer_worktree_links(worktree_dir, team_config_dir)?;

    if previous_branch != base_branch
        && previous_branch != task_branch
        && (previous_branch == engineer_name
            || previous_branch.starts_with(&format!("{engineer_name}/")))
        && branch_is_merged_into(project_root, &previous_branch, "main")?
    {
        delete_branch(project_root, &previous_branch)?;
    }

    Ok(worktree_dir.to_path_buf())
}

/// Set up worktrees for a multi-repo project. Creates one git worktree per
/// sub-repo inside `worktree_dir`, mirroring the original directory layout.
pub(crate) fn setup_multi_repo_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
    sub_repo_names: &[String],
) -> Result<PathBuf> {
    std::fs::create_dir_all(worktree_dir)
        .with_context(|| format!("failed to create {}", worktree_dir.display()))?;

    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        let sub_wt = worktree_dir.join(repo_name);
        setup_engineer_worktree(&repo_root, &sub_wt, branch_name, team_config_dir)?;
    }

    ensure_engineer_worktree_links(worktree_dir, team_config_dir)?;
    Ok(worktree_dir.to_path_buf())
}

/// Prepare worktrees for a multi-repo task assignment. Creates task branches
/// in every sub-repo so the engineer can work across all of them.
pub(crate) fn prepare_multi_repo_assignment_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
    task_branch: &str,
    team_config_dir: &Path,
    sub_repo_names: &[String],
) -> Result<PathBuf> {
    std::fs::create_dir_all(worktree_dir)
        .with_context(|| format!("failed to create {}", worktree_dir.display()))?;

    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        let sub_wt = worktree_dir.join(repo_name);
        prepare_engineer_assignment_worktree(
            &repo_root,
            &sub_wt,
            engineer_name,
            task_branch,
            team_config_dir,
        )?;
    }

    ensure_engineer_worktree_links(worktree_dir, team_config_dir)?;
    Ok(worktree_dir.to_path_buf())
}

pub(crate) fn worktree_commits_behind_main(worktree_dir: &Path) -> Result<u32> {
    map_git_error(
        retry_git(|| git_cmd::rev_list_count(worktree_dir, "HEAD..main")),
        "failed to measure worktree staleness against main",
    )
}

pub(crate) fn refresh_engineer_worktree_if_stale(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
    stale_threshold: u32,
) -> Result<WorktreeRefreshOutcome> {
    if !worktree_dir.exists() {
        return Ok(WorktreeRefreshOutcome {
            action: WorktreeRefreshAction::Unchanged,
            behind_main: None,
        });
    }

    let behind_main = Some(worktree_commits_behind_main(worktree_dir)?);
    if behind_main.is_none_or(|count| count <= stale_threshold) {
        return Ok(WorktreeRefreshOutcome {
            action: WorktreeRefreshAction::Unchanged,
            behind_main,
        });
    }

    let action =
        refresh_engineer_worktree(project_root, worktree_dir, branch_name, team_config_dir)?;
    Ok(WorktreeRefreshOutcome {
        action,
        behind_main,
    })
}

fn ensure_engineer_worktree_health(
    project_root: &Path,
    worktree_dir: &Path,
    _base_branch: &str,
) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }

    if !worktree_registered(project_root, worktree_dir)? {
        bail!(
            "engineer worktree path exists but is not registered in git worktree list: {}",
            worktree_dir.display()
        );
    }

    Ok(())
}

#[allow(dead_code)] // Retained for existing tests and as a lower-level helper.
pub(crate) fn refresh_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
) -> Result<WorktreeRefreshAction> {
    if !worktree_dir.exists() {
        return Ok(WorktreeRefreshAction::Unchanged);
    }

    if worktree_has_user_changes(worktree_dir)? {
        warn!(
            worktree = %worktree_dir.display(),
            branch = branch_name,
            "skipping worktree refresh because worktree is dirty"
        );
        return Ok(WorktreeRefreshAction::SkippedDirty);
    }

    if map_git_error(
        retry_git(|| git_cmd::merge_base_is_ancestor(project_root, "main", branch_name)),
        "failed to compare worktree branch with main",
    )? {
        return Ok(WorktreeRefreshAction::Unchanged);
    }

    let rebase_result = retry_git(|| git_cmd::rebase(worktree_dir, "main"));
    if rebase_result.is_ok() {
        info!(
            worktree = %worktree_dir.display(),
            branch = branch_name,
            "refreshed engineer worktree"
        );
        return Ok(WorktreeRefreshAction::Rebased);
    }

    let stderr = match rebase_result {
        Ok(_) => unreachable!("successful rebase returned early"),
        Err(git_cmd::GitError::Transient { stderr, .. })
        | Err(git_cmd::GitError::Permanent { stderr, .. })
        | Err(git_cmd::GitError::RebaseFailed { stderr, .. })
        | Err(git_cmd::GitError::MergeFailed { stderr, .. }) => stderr.trim().to_string(),
        Err(git_cmd::GitError::RevParseFailed { stderr, .. }) => stderr.trim().to_string(),
        Err(git_cmd::GitError::InvalidRevListCount { output, .. }) => output.trim().to_string(),
        Err(git_cmd::GitError::Exec { source, .. }) => source.to_string(),
    };
    let _ = retry_git(|| git_cmd::rebase_abort(worktree_dir));

    if !is_worktree_safe_to_mutate(worktree_dir)? {
        bail!(
            "worktree at {} has uncommitted changes on a task branch after failed rebase — refusing to destroy. Commit or stash first.",
            worktree_dir.display()
        );
    }

    map_git_error(
        retry_git(|| git_cmd::worktree_remove(project_root, worktree_dir, true)),
        &format!("failed to remove conflicted worktree after rebase error '{stderr}'"),
    )?;

    map_git_error(
        retry_git(|| git_cmd::branch_delete(project_root, branch_name)),
        &format!("failed to delete conflicted worktree branch after rebase error '{stderr}'"),
    )?;

    warn!(
        worktree = %worktree_dir.display(),
        branch = branch_name,
        rebase_error = %stderr,
        "recreating engineer worktree after rebase conflict"
    );
    setup_engineer_worktree(project_root, worktree_dir, branch_name, team_config_dir)?;
    Ok(WorktreeRefreshAction::Reset)
}

pub(crate) fn engineer_base_branch_name(engineer_name: &str) -> String {
    format!("eng-main/{engineer_name}")
}

fn maybe_migrate_legacy_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
    base_branch: &str,
) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }

    let current_branch = current_worktree_branch(worktree_dir)?;
    if current_branch != engineer_name {
        return Ok(());
    }

    if worktree_has_user_changes(worktree_dir)? {
        bail!(
            "legacy engineer branch '{}' is still checked out in {} with uncommitted changes; resolve it before assigning a new task branch",
            engineer_name,
            worktree_dir.display()
        );
    }

    checkout_worktree_branch_from_main(worktree_dir, base_branch)?;
    if branch_is_merged_into(project_root, engineer_name, "main")? {
        delete_branch(project_root, engineer_name)?;
        info!(
            branch = engineer_name,
            base_branch,
            worktree = %worktree_dir.display(),
            "auto-migrated legacy engineer worktree to base branch"
        );
        return Ok(());
    }

    let archive_branch = archived_legacy_branch_name(project_root, engineer_name)?;
    rename_branch(project_root, engineer_name, &archive_branch)?;
    warn!(
        old_branch = engineer_name,
        new_branch = %archive_branch,
        base_branch,
        worktree = %worktree_dir.display(),
        "auto-migrated unmerged legacy engineer worktree to base branch"
    );
    Ok(())
}

fn ensure_task_branch_namespace_available(project_root: &Path, engineer_name: &str) -> Result<()> {
    if !branch_exists(project_root, engineer_name)? {
        return Ok(());
    }

    if branch_is_checked_out_in_any_worktree(project_root, engineer_name)? {
        bail!(
            "legacy engineer branch '{}' is still checked out in a worktree; resolve it before assigning a new task branch",
            engineer_name
        );
    }

    if branch_is_merged_into(project_root, engineer_name, "main")? {
        delete_branch(project_root, engineer_name)?;
        info!(
            branch = engineer_name,
            "deleted merged legacy engineer branch to free task namespace"
        );
        return Ok(());
    }

    let archive_branch = archived_legacy_branch_name(project_root, engineer_name)?;
    rename_branch(project_root, engineer_name, &archive_branch)?;
    warn!(
        old_branch = engineer_name,
        new_branch = %archive_branch,
        "archived legacy engineer branch to free task namespace"
    );
    Ok(())
}

fn ensure_engineer_worktree_links(worktree_dir: &Path, team_config_dir: &Path) -> Result<()> {
    let wt_batty_dir = worktree_dir.join(".batty");
    std::fs::create_dir_all(&wt_batty_dir).ok();
    let wt_config_link = wt_batty_dir.join("team_config");

    if !wt_config_link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(team_config_dir, &wt_config_link).with_context(|| {
            format!(
                "failed to symlink {} -> {}",
                wt_config_link.display(),
                team_config_dir.display()
            )
        })?;

        #[cfg(not(unix))]
        {
            warn!("symlinks not supported on this platform, copying config instead");
            let _ = std::fs::create_dir_all(&wt_config_link);
        }

        debug!(
            link = %wt_config_link.display(),
            target = %team_config_dir.display(),
            "symlinked team config into worktree"
        );
    }

    Ok(())
}

fn ensure_shared_cargo_target_config(project_root: &Path, worktree_dir: &Path) -> Result<()> {
    let cargo_dir = worktree_dir.join(".cargo");
    std::fs::create_dir_all(&cargo_dir)
        .with_context(|| format!("failed to create {}", cargo_dir.display()))?;
    let config_path = cargo_dir.join("config.toml");
    // Each worktree gets its own target subdirectory so parallel builds
    // don't contend on the same Cargo lock. The shared parent is kept for
    // disk-pressure cleanup scans.
    let worktree_name = worktree_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let target_dir = shared_cargo_target_dir(project_root).join(&worktree_name);
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;

    let managed = format!(
        "{SHARED_CARGO_CONFIG_MARKER}\n[build]\ntarget-dir = {:?}\n",
        target_dir
    );

    match std::fs::read_to_string(&config_path) {
        Ok(existing) if existing == managed => return Ok(()),
        Ok(existing) if !existing.is_empty() && !existing.contains(SHARED_CARGO_CONFIG_MARKER) => {
            warn!(
                config = %config_path.display(),
                "leaving existing cargo config unchanged; shared target must be configured manually"
            );
            return Ok(());
        }
        Ok(_) | Err(_) => {}
    }

    std::fs::write(&config_path, managed)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok(())
}

fn ensure_engineer_worktree_excludes(worktree_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| format!("failed to resolve git dir for {}", worktree_dir.display()))?;
    if !output.status.success() {
        bail!(
            "failed to resolve git dir for {}: {}",
            worktree_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let git_dir_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let git_dir = if Path::new(&git_dir_text).is_absolute() {
        PathBuf::from(git_dir_text)
    } else {
        worktree_dir.join(git_dir_text)
    };
    let exclude_path = git_dir.join("info").join("exclude");
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut content = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if !content.contains(WORKTREE_EXCLUDE_MARKER) {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(WORKTREE_EXCLUDE_MARKER);
        content.push('\n');
    }

    for rule in [".cargo/", ".batty/team_config"] {
        if !content.lines().any(|line| line.trim() == rule) {
            content.push_str(rule);
            content.push('\n');
        }
    }

    std::fs::write(&exclude_path, content)
        .with_context(|| format!("failed to write {}", exclude_path.display()))?;
    Ok(())
}

fn engineer_worktree_project_root(worktree_dir: &Path) -> Option<PathBuf> {
    for ancestor in worktree_dir.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == "worktrees")
            && ancestor
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".batty")
        {
            return ancestor
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf);
        }
    }
    None
}

pub(crate) fn worktree_has_user_changes(worktree_dir: &Path) -> Result<bool> {
    Ok(map_git_error(
        retry_git(|| git_cmd::status_porcelain(worktree_dir)),
        "failed to inspect worktree status",
    )?
    .lines()
    .any(|line| !line.starts_with("?? .batty/") && !line.starts_with("?? .cargo/")))
}

pub(crate) fn git_has_unresolved_conflicts(repo_dir: &Path) -> Result<bool> {
    let status = map_git_error(
        retry_git(|| git_cmd::status_porcelain(repo_dir)),
        "failed to inspect git conflict state",
    )?;
    Ok(status.lines().any(line_has_unresolved_conflict))
}

fn line_has_unresolved_conflict(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= 2
        && matches!(
            (bytes[0], bytes[1]),
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D')
        )
}

/// Returns `false` if the worktree has uncommitted changes on a task branch
/// (i.e. not an `eng-main/*` base branch). This gate should be checked before
/// any operation that would destroy worktree state (reset, clean, checkout).
pub(crate) fn is_worktree_safe_to_mutate(worktree_dir: &Path) -> Result<bool> {
    if !worktree_dir.exists() {
        return Ok(true);
    }

    let has_changes = worktree_has_user_changes(worktree_dir)?;
    if !has_changes {
        return Ok(true);
    }

    let branch = match map_git_error(
        retry_git(|| git_cmd::rev_parse_branch(worktree_dir)),
        "failed to determine worktree branch for safety check",
    ) {
        Ok(b) => b,
        Err(_) => return Ok(true), // Can't determine branch — allow mutation
    };

    // eng-main/* branches are base branches with no user work worth preserving.
    if branch.starts_with("eng-main/") {
        return Ok(true);
    }

    // Task branch with uncommitted changes — NOT safe to mutate.
    warn!(
        worktree = %worktree_dir.display(),
        branch = %branch,
        "worktree has uncommitted changes on task branch, refusing to mutate"
    );
    Ok(false)
}

fn run_git_with_timeout(worktree_dir: &Path, args: &[&str], timeout: Duration) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(worktree_dir).args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to launch `git {}` in {}",
            args.join(" "),
            worktree_dir.display()
        )
    })?;

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!(
                "`git {}` failed in {} with status {}",
                args.join(" "),
                worktree_dir.display(),
                status
            );
        }

        if Instant::now() >= deadline {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            bail!(
                "`git {}` timed out after {}s in {}",
                args.join(" "),
                timeout.as_secs(),
                worktree_dir.display()
            );
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut std::process::Child) {
    let _ = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
}

#[cfg(not(unix))]
fn terminate_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
}

pub(crate) fn preserve_worktree_with_commit(
    worktree_dir: &Path,
    commit_message: &str,
    timeout: Duration,
) -> Result<bool> {
    if !worktree_has_user_changes(worktree_dir)? {
        return Ok(false);
    }

    run_git_with_timeout(worktree_dir, &["add", "-A"], timeout)?;
    run_git_with_timeout(worktree_dir, &["commit", "-m", commit_message], timeout)?;
    Ok(true)
}

fn auto_clean_worktree(worktree_dir: &Path) -> Result<()> {
    // Try commit first — preserves work in git history (no stash accumulation).
    if auto_commit_before_reset(worktree_dir) {
        return Ok(());
    }

    // Commit failed — fall back to force clean.
    warn!(
        worktree = %worktree_dir.display(),
        "force-cleaning engineer worktree"
    );
    let _ = retry_git(|| git_cmd::run_git(worktree_dir, &["checkout", "--", "."]));
    let _ = retry_git(|| git_cmd::run_git(worktree_dir, &["clean", "-fd", "--exclude=.batty/"]));

    if worktree_has_user_changes(worktree_dir)? {
        bail!(
            "engineer worktree at {} still dirty after auto-clean",
            worktree_dir.display()
        );
    }
    Ok(())
}

/// Auto-commit uncommitted changes before a worktree reset to avoid stash
/// accumulation. Returns `true` if changes were successfully committed or
/// there was nothing to commit.
pub(crate) fn auto_commit_before_reset(worktree_dir: &Path) -> bool {
    let branch = retry_git(|| git_cmd::rev_parse_branch(worktree_dir)).unwrap_or_default();
    let msg = format!("wip: auto-save before worktree reset [{}]", branch);
    match preserve_worktree_with_commit(worktree_dir, &msg, Duration::from_secs(5)) {
        Ok(true) => {
            info!(
                worktree = %worktree_dir.display(),
                branch = %branch,
                "auto-committed uncommitted changes before worktree reset"
            );
            true
        }
        Ok(false) => true,
        Err(e) => {
            warn!(
                worktree = %worktree_dir.display(),
                error = %e,
                "auto-commit failed"
            );
            false
        }
    }
}

pub(crate) fn current_worktree_branch(worktree_dir: &Path) -> Result<String> {
    map_git_error(
        retry_git(|| git_cmd::rev_parse_branch(worktree_dir)),
        "failed to determine worktree branch",
    )
}

pub(crate) fn checkout_worktree_branch_from_main(
    worktree_dir: &Path,
    branch_name: &str,
) -> Result<()> {
    map_git_error(
        retry_git(|| git_cmd::checkout_new_branch(worktree_dir, branch_name, "main")),
        &format!("failed to switch worktree to branch '{branch_name}'"),
    )
}

fn branch_exists(project_root: &Path, branch_name: &str) -> Result<bool> {
    map_git_error(
        retry_git(|| git_cmd::show_ref_exists(project_root, branch_name)),
        &format!("failed to check whether branch '{branch_name}' exists"),
    )
}

fn worktree_registered(project_root: &Path, worktree_dir: &Path) -> Result<bool> {
    let output = map_git_error(
        retry_git(|| git_cmd::worktree_list(project_root)),
        "failed to list git worktrees",
    )?;
    let target = worktree_dir
        .canonicalize()
        .unwrap_or_else(|_| worktree_dir.to_path_buf());

    for line in output.lines() {
        let Some(candidate) = line.strip_prefix("worktree ") else {
            continue;
        };
        let candidate = PathBuf::from(candidate.trim());
        let candidate = candidate.canonicalize().unwrap_or(candidate);
        if candidate == target {
            return Ok(true);
        }
    }

    Ok(false)
}

fn branch_is_checked_out_in_any_worktree(project_root: &Path, branch_name: &str) -> Result<bool> {
    let output = map_git_error(
        retry_git(|| git_cmd::worktree_list(project_root)),
        "failed to list git worktrees",
    )?;
    let target = format!("branch refs/heads/{branch_name}");
    Ok(output.lines().any(|line| line.trim() == target))
}

pub(crate) fn branch_is_merged_into(
    project_root: &Path,
    branch_name: &str,
    base_branch: &str,
) -> Result<bool> {
    map_git_error(
        retry_git(|| git_cmd::merge_base_is_ancestor(project_root, branch_name, base_branch)),
        &format!("failed to compare branch '{branch_name}' with '{base_branch}'"),
    )
}

pub(crate) fn engineer_worktree_ready_for_dispatch(
    project_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }

    if !worktree_registered(project_root, worktree_dir)? {
        bail!(
            "engineer worktree path exists but is not registered in git worktree list: {}",
            worktree_dir.display()
        );
    }

    let base_branch = engineer_base_branch_name(engineer_name);
    let current_branch = current_worktree_branch(worktree_dir)?;
    if current_branch != base_branch {
        bail!(
            "engineer worktree '{}' is checked out on '{}' instead of '{}'",
            engineer_name,
            current_branch,
            base_branch
        );
    }

    if worktree_has_user_changes(worktree_dir)? {
        bail!(
            "engineer worktree '{}' has uncommitted changes",
            engineer_name
        );
    }

    let ahead_of_main = map_git_error(
        retry_git(|| git_cmd::rev_list_count(worktree_dir, "main..HEAD")),
        "failed to compare worktree against main",
    )?;
    let behind_main = map_git_error(
        retry_git(|| git_cmd::rev_list_count(worktree_dir, "HEAD..main")),
        "failed to compare worktree against main",
    )?;
    if ahead_of_main != 0 || behind_main != 0 {
        bail!(
            "engineer worktree '{}' is not based on current main (ahead {}, behind {})",
            engineer_name,
            ahead_of_main,
            behind_main
        );
    }

    Ok(())
}

pub(crate) fn delete_branch(project_root: &Path, branch_name: &str) -> Result<()> {
    map_git_error(
        retry_git(|| git_cmd::branch_delete(project_root, branch_name)),
        &format!("failed to delete branch '{branch_name}'"),
    )
}

fn archived_legacy_branch_name(project_root: &Path, engineer_name: &str) -> Result<String> {
    let short_sha = map_git_error(
        retry_git(|| git_cmd::run_git(project_root, &["rev-parse", "--short", engineer_name])),
        &format!("failed to resolve legacy branch '{engineer_name}'"),
    )?
    .stdout
    .trim()
    .to_string();
    let mut candidate = format!("legacy/{engineer_name}-{short_sha}");
    let mut counter = 1usize;
    while branch_exists(project_root, &candidate)? {
        counter += 1;
        candidate = format!("legacy/{engineer_name}-{short_sha}-{counter}");
    }
    Ok(candidate)
}

fn rename_branch(project_root: &Path, old_branch: &str, new_branch: &str) -> Result<()> {
    map_git_error(
        retry_git(|| git_cmd::branch_rename(project_root, old_branch, new_branch)),
        &format!("failed to rename branch '{old_branch}' to '{new_branch}'"),
    )
}

/// Recycle done cron tasks back to todo when their next occurrence is due.
///
/// Returns a list of (task_id, cron_expression) for each recycled task.
pub(crate) fn recycle_cron_tasks(board_dir: &Path) -> Result<Vec<(u32, String)>> {
    use chrono::Utc;
    use cron::Schedule;
    use serde_yaml::Value;
    use std::str::FromStr;

    use super::task_cmd::{find_task_path, set_optional_string, update_task_frontmatter, yaml_key};

    let tasks_dir = board_dir.join("tasks");
    let tasks = crate::task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;

    let now = Utc::now();
    let mut recycled = Vec::new();

    for task in &tasks {
        // Skip non-done tasks
        if task.status != "done" {
            continue;
        }

        // Skip tasks without a cron schedule
        let cron_expr = match &task.cron_schedule {
            Some(expr) => expr.clone(),
            None => continue,
        };

        // Skip archived tasks
        if task.tags.iter().any(|t| t == "archived") {
            continue;
        }

        // Parse the cron expression
        let schedule = match Schedule::from_str(&cron_expr) {
            Ok(s) => s,
            Err(err) => {
                warn!(task_id = task.id, cron = %cron_expr, error = %err, "invalid cron expression, skipping");
                continue;
            }
        };

        // Determine the reference point: cron_last_run or now - 1 day
        let reference = task
            .cron_last_run
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|| now - chrono::Duration::days(1));

        // Find next occurrence after reference
        let next = match schedule.after(&reference).next() {
            Some(dt) => dt,
            None => continue,
        };

        // If next occurrence is in the future, skip
        if next > now {
            continue;
        }

        // Compute next FUTURE occurrence for scheduled_for
        let next_future = schedule.after(&now).next().map(|dt| dt.to_rfc3339());

        let now_str = now.to_rfc3339();
        let task_id = task.id;
        let task_path = find_task_path(board_dir, task_id)?;

        update_task_frontmatter(&task_path, |mapping| {
            // Set status to todo
            mapping.insert(yaml_key("status"), Value::String("todo".to_string()));

            // Update scheduled_for to next future occurrence
            set_optional_string(mapping, "scheduled_for", next_future.as_deref());

            // Update cron_last_run to now
            set_optional_string(mapping, "cron_last_run", Some(&now_str));

            // Clear transient fields
            mapping.remove(yaml_key("claimed_by"));
            mapping.remove(yaml_key("branch"));
            mapping.remove(yaml_key("commit"));
            mapping.remove(yaml_key("artifacts"));
            mapping.remove(yaml_key("next_action"));
            mapping.remove(yaml_key("review_owner"));
            mapping.remove(yaml_key("blocked_on"));
            mapping.remove(yaml_key("worktree_path"));
        })?;

        info!(task_id, cron = %cron_expr, "recycled cron task back to todo");
        recycled.push((task_id, cron_expr));
    }

    Ok(recycled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{git, git_ok, git_stdout};

    fn production_unwrap_expect_count(path: &Path) -> usize {
        let content = std::fs::read_to_string(path).unwrap();
        let test_split = content.split("\n#[cfg(test)]").next().unwrap_or(&content);
        test_split
            .lines()
            .filter(|line| line.contains(".unwrap(") || line.contains(".expect("))
            .count()
    }

    fn init_git_repo(tmp: &tempfile::TempDir) -> PathBuf {
        let repo = tmp.path();
        git_ok(repo, &["init", "-b", "main"]);
        git_ok(repo, &["config", "user.email", "batty-test@example.com"]);
        git_ok(repo, &["config", "user.name", "Batty Test"]);
        std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        git_ok(repo, &["add", "README.md", ".batty/team_config"]);
        git_ok(repo, &["commit", "-m", "initial"]);
        repo.to_path_buf()
    }

    fn write_task_file(
        dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        priority: &str,
        claimed_by: Option<&str>,
        depends_on: &[u32],
    ) {
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: {priority}\n");
        if let Some(cb) = claimed_by {
            content.push_str(&format!("claimed_by: {cb}\n"));
        }
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep in depends_on {
                content.push_str(&format!("    - {dep}\n"));
            }
        }
        content.push_str("class: standard\n---\n\nTask description.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    fn write_task_file_with_workflow_frontmatter(
        dir: &Path,
        id: u32,
        title: &str,
        extra_frontmatter: &str,
    ) {
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: todo\npriority: critical\n{extra_frontmatter}class: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_refresh_worktree_rebases_behind_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(repo.join("main.txt"), "new main content\n").unwrap();
        git_ok(&repo, &["add", "main.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        assert!(worktree_dir.join("main.txt").exists());
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn test_refresh_worktree_recreates_on_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-2");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("file.txt"), "A\n").unwrap();
        git_ok(&repo, &["add", "file.txt"]);
        git_ok(&repo, &["commit", "-m", "add file"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("file.txt"), "B\n").unwrap();
        git_ok(&worktree_dir, &["add", "file.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("file.txt"), "C\n").unwrap();
        git_ok(&repo, &["add", "file.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        assert!(worktree_dir.exists());
        assert_eq!(
            std::fs::read_to_string(worktree_dir.join("file.txt")).unwrap(),
            "C\n"
        );
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn test_refresh_worktree_skips_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-3");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("scratch.txt"), "uncommitted\n").unwrap();

        std::fs::write(repo.join("main.txt"), "new main content\n").unwrap();
        git_ok(&repo, &["add", "main.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        assert!(!worktree_dir.join("main.txt").exists());
        assert_eq!(
            std::fs::read_to_string(worktree_dir.join("scratch.txt")).unwrap(),
            "uncommitted\n"
        );
    }

    #[test]
    fn test_refresh_worktree_noop_when_current() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-4");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-4", &team_config_dir).unwrap();
        let before = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-4", &team_config_dir).unwrap();

        let after = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        assert_eq!(before, after);
        assert!(worktree_dir.exists());
    }

    #[test]
    fn test_prepare_assignment_worktree_checks_out_task_branch_from_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-5");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-5",
            "eng-5/123",
            &team_config_dir,
        )
        .unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-5/123"
        );
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );
        assert!(worktree_dir.join(".batty").join("team_config").exists());
    }

    #[test]
    fn test_prepare_assignment_worktree_recreates_stale_task_branch_from_current_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-5b");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-5b",
            "eng-5b/123",
            &team_config_dir,
        )
        .unwrap();
        let stale_commit = git_stdout(&repo, &["rev-parse", "eng-5b/123"]);

        git_ok(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("fresh.txt"), "fresh main content\n").unwrap();
        git_ok(&repo, &["add", "fresh.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);
        let current_main = git_stdout(&repo, &["rev-parse", "main"]);

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-5b",
            "eng-5b/123",
            &team_config_dir,
        )
        .unwrap();

        assert_ne!(stale_commit, current_main);
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "eng-5b/123"]),
            current_main
        );
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"]),
            current_main
        );
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-5b/123"
        );
    }

    #[test]
    fn test_setup_engineer_worktree_writes_shared_cargo_target_config() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-shared");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-shared", &team_config_dir).unwrap();

        let config =
            std::fs::read_to_string(worktree_dir.join(".cargo").join("config.toml")).unwrap();
        assert!(config.contains(SHARED_CARGO_CONFIG_MARKER));
        assert!(config.contains(shared_cargo_target_dir(&repo).to_string_lossy().as_ref()));
    }

    #[test]
    fn test_setup_engineer_worktree_preserves_existing_cargo_config() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-preserve");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-preserve", &team_config_dir).unwrap();
        let config_path = worktree_dir.join(".cargo").join("config.toml");
        std::fs::write(&config_path, "[term]\nverbose = true\n").unwrap();

        setup_engineer_worktree(&repo, &worktree_dir, "eng-preserve", &team_config_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(config_path).unwrap(),
            "[term]\nverbose = true\n"
        );
    }

    #[test]
    fn test_prepare_assignment_worktree_auto_cleans_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-6");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &engineer_base_branch_name("eng-6"),
            &team_config_dir,
        )
        .unwrap();
        std::fs::write(worktree_dir.join("scratch.txt"), "uncommitted\n").unwrap();

        // Should succeed — auto-clean commits the dirty file.
        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-6",
            "eng-6/7",
            &team_config_dir,
        )
        .unwrap();

        // Worktree should be clean now.
        assert!(!worktree_has_user_changes(&worktree_dir).unwrap());

        // No stash should be created (commit-before-reset discipline).
        let stash_list = git_stdout(&worktree_dir, &["stash", "list"]);
        assert!(
            stash_list.trim().is_empty(),
            "no stash should be created, changes should be auto-committed"
        );
    }

    #[test]
    fn test_prepare_assignment_worktree_auto_migrates_clean_legacy_worktree_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-6b");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-6b", &team_config_dir).unwrap();

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-6b",
            "eng-6b/17",
            &team_config_dir,
        )
        .unwrap();

        let legacy_check = git(&repo, &["rev-parse", "--verify", "eng-6b"]);
        assert!(!legacy_check.status.success());
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-6b/17"
        );
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--verify", "eng-main/eng-6b"]),
            git_stdout(&repo, &["rev-parse", "--verify", "main"])
        );
    }

    #[test]
    fn test_prepare_assignment_worktree_deletes_merged_legacy_branch_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-7");
        let team_config_dir = repo.join(".batty").join("team_config");

        git_ok(&repo, &["branch", "eng-7"]);

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-7",
            "eng-7/99",
            &team_config_dir,
        )
        .unwrap();

        let legacy_check = git(&repo, &["rev-parse", "--verify", "eng-7"]);
        assert!(!legacy_check.status.success());
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-7/99"
        );
    }

    #[test]
    fn test_prepare_assignment_worktree_archives_unmerged_legacy_branch_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-8");
        let team_config_dir = repo.join(".batty").join("team_config");

        git_ok(&repo, &["checkout", "-b", "eng-8"]);
        std::fs::write(repo.join("legacy.txt"), "legacy branch work\n").unwrap();
        git_ok(&repo, &["add", "legacy.txt"]);
        git_ok(&repo, &["commit", "-m", "legacy work"]);
        git_ok(&repo, &["checkout", "main"]);

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-8",
            "eng-8/100",
            &team_config_dir,
        )
        .unwrap();

        let legacy_check = git(&repo, &["rev-parse", "--verify", "eng-8"]);
        assert!(!legacy_check.status.success());
        assert!(!git_stdout(&repo, &["branch", "--list", "legacy/eng-8-*"]).is_empty());
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-8/100"
        );
    }

    #[test]
    fn test_prepare_assignment_worktree_rejects_unregistered_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-9");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::create_dir_all(&worktree_dir).unwrap();

        let err = prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-9",
            "eng-9/1",
            &team_config_dir,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("not registered in git worktree list")
        );
    }

    #[test]
    fn test_next_unclaimed_task_picks_highest_priority() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 1, "low-task", "todo", "low", None, &[]);
        write_task_file(tmp.path(), 2, "high-task", "todo", "high", None, &[]);
        write_task_file(
            tmp.path(),
            3,
            "critical-task",
            "todo",
            "critical",
            None,
            &[],
        );

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 3);
        assert_eq!(task.title, "critical-task");
    }

    #[test]
    fn test_next_unclaimed_task_skips_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(
            tmp.path(),
            1,
            "claimed-task",
            "todo",
            "critical",
            Some("eng-1-1"),
            &[],
        );
        write_task_file(tmp.path(), 2, "open-task", "todo", "low", None, &[]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 2);
        assert_eq!(task.title, "open-task");
    }

    #[test]
    fn test_next_unclaimed_task_skips_blocked_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 1, "first-task", "backlog", "medium", None, &[]);
        write_task_file(tmp.path(), 2, "second-task", "todo", "critical", None, &[1]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 1);
        assert_eq!(task.title, "first-task");
    }

    #[test]
    fn test_next_unclaimed_task_skips_blocked_on_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file_with_workflow_frontmatter(
            tmp.path(),
            1,
            "blocked-task",
            "blocked_on: waiting-for-review\n",
        );
        write_task_file(tmp.path(), 2, "open-task", "todo", "high", None, &[]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 2);
        assert_eq!(task.title, "open-task");
    }

    #[test]
    fn test_next_unclaimed_task_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks")).unwrap();

        let task = next_unclaimed_task(tmp.path()).unwrap();
        assert!(task.is_none());
    }

    #[test]
    fn test_run_tests_in_worktree_returns_pass_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname = \"batty-testcrate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        std::fs::write(
            worktree.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn passes() {\n        assert_eq!(2 + 2, 4);\n    }\n}\n",
        )
        .unwrap();
        let run = run_tests_in_worktree(worktree, None).unwrap();
        assert!(run.passed);
        assert!(run.output.contains("test result: ok"));
        assert_eq!(run.results.framework, "cargo");

        std::fs::write(
            worktree.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn fails() {\n        assert_eq!(2 + 2, 5);\n    }\n}\n",
        )
        .unwrap();
        let run = run_tests_in_worktree(worktree, None).unwrap();
        assert!(!run.passed);
        assert!(run.output.contains("FAILED"));
        assert_eq!(run.results.failed, 1);
        assert_eq!(run.results.failures[0].test_name, "tests::fails");
    }

    #[test]
    fn test_run_tests_in_worktree_uses_configured_command() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path();
        std::fs::write(
            worktree.join("check.sh"),
            "#!/bin/sh\necho CONFIG_TEST_OK\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                worktree.join("check.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let run = run_tests_in_worktree(worktree, Some("./check.sh")).unwrap();
        assert!(run.passed);
        assert!(run.output.contains("CONFIG_TEST_OK"));
    }

    #[test]
    fn test_run_tests_in_worktree_sets_shared_target_dir_for_engineer_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-target");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-target", &team_config_dir).unwrap();
        std::fs::write(
            worktree_dir.join("check.sh"),
            "#!/bin/sh\nprintf '%s\\n' \"$CARGO_TARGET_DIR\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                worktree_dir.join("check.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let run = run_tests_in_worktree(&worktree_dir, Some("./check.sh")).unwrap();
        assert!(run.passed);
        assert!(
            run.output
                .contains(shared_cargo_target_dir(&repo).to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_read_task_title_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("042-my-cool-task.md"),
            "---\ntitle: My Cool Task\nstatus: in-progress\npriority: high\n---\nBody here\n",
        )
        .unwrap();
        let title = read_task_title(tmp.path(), 42);
        assert_eq!(title, "My Cool Task");
    }

    #[test]
    fn test_read_task_title_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let title = read_task_title(tmp.path(), 99);
        assert_eq!(title, "Task #99");
    }

    #[test]
    fn review_ready_gate_accepts_valid_commit_diff() {
        let gate = validate_review_ready_diff_stat(
            " src/team/completion.rs | 12 ++++++++++++\n 1 file changed, 12 insertions(+)\n",
        );
        assert!(gate.blockers.is_empty());
    }

    #[test]
    fn review_ready_gate_rejects_zero_commit_diff() {
        let gate = validate_review_ready_diff_stat("");
        assert!(
            gate.blockers
                .contains(&"engineer branch has no diff against main".to_string())
        );
    }

    #[test]
    fn review_ready_gate_rejects_config_only_diff() {
        let gate = validate_review_ready_diff_stat(
            " Cargo.toml | 14 ++++++++++++++\n docs/notes.md | 6 ++++++\n 2 files changed, 20 insertions(+)\n",
        );
        assert!(
            gate.blockers
                .iter()
                .any(|blocker| blocker.contains("changes outside task scope fence"))
        );
        assert!(
            gate.blockers
                .iter()
                .any(|blocker| blocker.contains("need at least 10 lines of production Rust added"))
        );
    }

    #[test]
    fn review_ready_gate_rejects_destructive_net_deletion_diff() {
        let gate = validate_review_ready_diff_stat(
            " src/team/review.rs | 12 ++++--------\n 1 file changed, 4 insertions(+), 8 deletions(-)\n",
        );
        assert!(
            gate.blockers
                .iter()
                .any(|blocker| blocker.contains("net-destructive"))
        );
    }

    #[test]
    fn review_ready_gate_rejects_out_of_scope_diff() {
        let gate = validate_review_ready_diff_stat(
            " src/team/daemon.rs | 15 +++++++++++++++\n 1 file changed, 15 insertions(+)\n",
        );
        assert!(
            gate.blockers
                .iter()
                .any(|blocker| blocker.contains("changes outside task scope fence"))
        );
    }

    #[test]
    fn production_task_loop_has_no_unwrap_or_expect_calls() {
        let count = production_unwrap_expect_count(Path::new(file!()));
        assert_eq!(
            count, 0,
            "production task_loop.rs should avoid unwrap/expect"
        );
    }

    // -- Cron recycling tests --

    fn write_cron_task(board_dir: &Path, id: u32, status: &str, cron: &str, extra: &str) {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join(format!("{id:03}-cron-task.md"));
        let content = format!(
            "---\nid: {id}\ntitle: Cron Task {id}\nstatus: {status}\npriority: medium\ncron_schedule: \"{cron}\"\n{extra}---\n\nCron task body.\n"
        );
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn cron_recycle_resets_done_task_to_todo() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_cron_task(
            board_dir,
            1,
            "done",
            "0 * * * * *",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\n",
        );

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert_eq!(recycled.len(), 1);
        assert_eq!(recycled[0].0, 1);

        let task = crate::task::Task::from_file(&board_dir.join("tasks").join("001-cron-task.md"))
            .unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.cron_last_run.is_some(), "cron_last_run should be set");
        assert!(task.scheduled_for.is_some(), "scheduled_for should be set");
        assert!(task.claimed_by.is_none(), "claimed_by should be cleared");
    }

    #[test]
    fn cron_recycle_skips_archived_task() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_cron_task(
            board_dir,
            2,
            "done",
            "0 * * * * *",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\ntags:\n  - archived\n",
        );

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert!(recycled.is_empty(), "archived tasks should be skipped");
    }

    #[test]
    fn cron_recycle_skips_in_progress_task() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_cron_task(
            board_dir,
            3,
            "in-progress",
            "0 * * * * *",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\n",
        );

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert!(recycled.is_empty(), "in-progress tasks should be skipped");
    }

    #[test]
    fn cron_recycle_missed_trigger_skips_to_next_future() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_cron_task(
            board_dir,
            4,
            "done",
            "0 * * * * *",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\n",
        );

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert_eq!(recycled.len(), 1);

        let task = crate::task::Task::from_file(&board_dir.join("tasks").join("004-cron-task.md"))
            .unwrap();
        assert_eq!(task.status, "todo");

        let scheduled = task.scheduled_for.as_deref().unwrap();
        let scheduled_dt = chrono::DateTime::parse_from_rfc3339(scheduled).unwrap();
        assert!(
            scheduled_dt > chrono::Utc::now(),
            "scheduled_for should be in the future, got: {scheduled}"
        );
    }

    #[test]
    fn cron_recycle_clears_transient_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_cron_task(
            board_dir,
            5,
            "done",
            "0 * * * * *",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\nclaimed_by: eng-1-1\nbranch: eng-1-1/5\ncommit: abc123\nnext_action: review\nreview_owner: manager\nblocked_on: other\nworktree_path: /tmp/wt\n",
        );

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert_eq!(recycled.len(), 1);

        let task = crate::task::Task::from_file(&board_dir.join("tasks").join("005-cron-task.md"))
            .unwrap();
        assert!(task.claimed_by.is_none());
        assert!(task.branch.is_none());
        assert!(task.commit.is_none());
        assert!(task.next_action.is_none());
        assert!(task.review_owner.is_none());
        assert!(task.blocked_on.is_none());
        assert!(task.worktree_path.is_none());
    }

    #[test]
    fn cron_recycle_emits_event() {
        use crate::team::events::TeamEvent;

        let event = TeamEvent::task_recycled(42, "0 9 * * 1");
        assert_eq!(event.event, "task_recycled");
        assert_eq!(event.task.as_deref(), Some("#42"));
        assert_eq!(event.reason.as_deref(), Some("0 9 * * 1"));
    }

    #[test]
    fn task_recycled_event_format() {
        use crate::team::events::TeamEvent;

        let event = TeamEvent::task_recycled(7, "30 8 * * *");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_recycled\""));
        assert!(json.contains("\"task\":\"#7\""));
        assert!(json.contains("\"reason\":\"30 8 * * *\""));
    }

    // -- Integration tests --

    #[test]
    fn cron_recycler_integration_resets_done_task() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();

        // cron_last_run 2 minutes ago — next minutely trigger is already past
        let two_min_ago = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
        write_cron_task(
            board_dir,
            10,
            "done",
            "0 * * * * *",
            &format!(
                "cron_last_run: \"{two_min_ago}\"\nclaimed_by: eng-1-1\nbranch: eng-1-1/10\ncommit: deadbeef\nnext_action: review\nreview_owner: manager\nblocked_on: other\nworktree_path: /tmp/wt\n"
            ),
        );

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert_eq!(recycled.len(), 1, "done cron task should be recycled");
        assert_eq!(recycled[0].0, 10);

        let task = crate::task::Task::from_file(&board_dir.join("tasks").join("010-cron-task.md"))
            .unwrap();

        // Status reset to todo
        assert_eq!(task.status, "todo");

        // scheduled_for set to a future time
        let scheduled = task
            .scheduled_for
            .as_deref()
            .expect("scheduled_for should be set");
        let scheduled_dt = chrono::DateTime::parse_from_rfc3339(scheduled).unwrap();
        assert!(
            scheduled_dt > chrono::Utc::now(),
            "scheduled_for should be in the future, got: {scheduled}"
        );

        // cron_last_run updated (should be more recent than 2 min ago)
        let last_run = task
            .cron_last_run
            .as_deref()
            .expect("cron_last_run should be set");
        let last_run_dt = chrono::DateTime::parse_from_rfc3339(last_run).unwrap();
        let two_min_ago_dt = chrono::DateTime::parse_from_rfc3339(&two_min_ago).unwrap();
        assert!(
            last_run_dt > two_min_ago_dt,
            "cron_last_run should be updated to now, not the old value"
        );

        // Transient fields cleared
        assert!(task.claimed_by.is_none(), "claimed_by should be cleared");
        assert!(task.branch.is_none(), "branch should be cleared");
        assert!(task.commit.is_none(), "commit should be cleared");
        assert!(task.next_action.is_none(), "next_action should be cleared");
        assert!(
            task.review_owner.is_none(),
            "review_owner should be cleared"
        );
        assert!(task.blocked_on.is_none(), "blocked_on should be cleared");
        assert!(
            task.worktree_path.is_none(),
            "worktree_path should be cleared"
        );
    }

    #[test]
    fn cron_recycler_skips_non_cron_done_task() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();

        // Done task WITHOUT cron_schedule
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join("011-regular-task.md");
        std::fs::write(
            &path,
            "---\nid: 11\ntitle: Regular Task\nstatus: done\npriority: medium\n---\n\nNon-cron task.\n",
        )
        .unwrap();

        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert!(
            recycled.is_empty(),
            "non-cron done task should not be recycled"
        );

        // Verify task unchanged
        let task = crate::task::Task::from_file(&path).unwrap();
        assert_eq!(task.status, "done", "status should remain done");
    }

    #[test]
    fn e2e_done_cron_task_recycled() {
        use crate::team::resolver::{ResolutionStatus, resolve_board};
        use crate::team::test_support::{engineer_member, manager_member};

        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();

        // Create a done cron task with old cron_last_run
        write_cron_task(
            board_dir,
            10,
            "done",
            "0 * * * * *",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\n",
        );

        // Before recycling: task is done, so resolve_board excludes it
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let resolutions_before = resolve_board(board_dir, &members).unwrap();
        assert!(
            resolutions_before.is_empty(),
            "done task should not appear in resolve_board"
        );

        // Recycle the cron task
        let recycled = recycle_cron_tasks(board_dir).unwrap();
        assert_eq!(recycled.len(), 1, "one task should be recycled");
        assert_eq!(recycled[0].0, 10);

        // Verify task file was updated
        let task = crate::task::Task::from_file(&board_dir.join("tasks").join("010-cron-task.md"))
            .unwrap();
        assert_eq!(task.status, "todo", "status should be reset to todo");
        assert!(task.claimed_by.is_none(), "claimed_by should be cleared");
        assert!(
            task.cron_last_run.is_some(),
            "cron_last_run should be updated"
        );

        // scheduled_for should be set to a future time
        let scheduled = task.scheduled_for.as_deref().unwrap();
        let scheduled_dt = chrono::DateTime::parse_from_rfc3339(scheduled).unwrap();
        assert!(
            scheduled_dt > chrono::Utc::now(),
            "scheduled_for should be in the future, got: {scheduled}"
        );

        // After recycling: task is now todo with future scheduled_for → Blocked
        let resolutions_after = resolve_board(board_dir, &members).unwrap();
        assert_eq!(resolutions_after.len(), 1);
        assert_eq!(
            resolutions_after[0].status,
            ResolutionStatus::Blocked,
            "recycled cron task with future scheduled_for should be Blocked until its time"
        );
        assert!(
            resolutions_after[0]
                .blocking_reason
                .as_ref()
                .unwrap()
                .contains("scheduled for"),
            "blocking reason should mention 'scheduled for'"
        );
    }

    // --- is_worktree_safe_to_mutate tests ---

    #[test]
    fn safe_to_mutate_nonexistent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(is_worktree_safe_to_mutate(&missing).unwrap());
    }

    #[test]
    fn safe_to_mutate_clean_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-safe");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-safe",
            "eng-safe/99",
            &team_config_dir,
        )
        .unwrap();

        // No uncommitted changes — safe to mutate.
        assert!(is_worktree_safe_to_mutate(&wt_dir).unwrap());
    }

    #[test]
    fn unsafe_to_mutate_dirty_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-dirty");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-dirty",
            "eng-dirty/42",
            &team_config_dir,
        )
        .unwrap();

        // Create uncommitted changes.
        std::fs::write(wt_dir.join("wip.txt"), "work in progress\n").unwrap();
        git_ok(&wt_dir, &["add", "wip.txt"]);

        // Dirty task branch — NOT safe.
        assert!(!is_worktree_safe_to_mutate(&wt_dir).unwrap());
    }

    #[test]
    fn safe_to_mutate_dirty_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-base");
        let team_config_dir = repo.join(".batty").join("team_config");

        let base = engineer_base_branch_name("eng-base");
        setup_engineer_worktree(&repo, &wt_dir, &base, &team_config_dir).unwrap();

        std::fs::write(wt_dir.join("junk.txt"), "junk\n").unwrap();
        git_ok(&wt_dir, &["add", "junk.txt"]);

        // Dirty but on eng-main/* — safe to mutate.
        assert!(is_worktree_safe_to_mutate(&wt_dir).unwrap());
    }

    #[test]
    fn unsafe_to_mutate_dirty_untracked_files_on_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-ut");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-ut",
            "eng-ut/55",
            &team_config_dir,
        )
        .unwrap();

        // Untracked file (not in .batty/) counts as user changes.
        std::fs::write(wt_dir.join("new_file.rs"), "fn main() {}\n").unwrap();

        assert!(!is_worktree_safe_to_mutate(&wt_dir).unwrap());
    }

    #[test]
    fn safe_to_mutate_only_batty_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-bt");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-bt",
            "eng-bt/33",
            &team_config_dir,
        )
        .unwrap();

        // Only .batty/ untracked files — not user changes, safe.
        std::fs::create_dir_all(wt_dir.join(".batty").join("temp")).unwrap();
        std::fs::write(wt_dir.join(".batty").join("temp").join("log.txt"), "log\n").unwrap();

        assert!(is_worktree_safe_to_mutate(&wt_dir).unwrap());
    }

    // --- auto_commit_before_reset tests ---

    #[test]
    fn auto_commit_saves_uncommitted_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-ac");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-ac",
            "eng-ac/77",
            &team_config_dir,
        )
        .unwrap();

        // Create uncommitted changes.
        std::fs::write(wt_dir.join("work.rs"), "fn hello() {}\n").unwrap();
        git_ok(&wt_dir, &["add", "work.rs"]);

        assert!(auto_commit_before_reset(&wt_dir));

        // Worktree should now be clean.
        let status = git_stdout(&wt_dir, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "worktree should be clean after auto-commit"
        );

        // Verify the commit message contains the wip marker.
        let log = git_stdout(&wt_dir, &["log", "--oneline", "-1"]);
        assert!(
            log.contains("wip: auto-save"),
            "commit should have wip marker, got: {log}"
        );
    }

    #[test]
    fn auto_commit_noop_on_clean_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-cl");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-cl",
            "eng-cl/88",
            &team_config_dir,
        )
        .unwrap();

        let before = git_stdout(&wt_dir, &["rev-parse", "HEAD"]);

        // No changes — should succeed without creating a commit.
        assert!(auto_commit_before_reset(&wt_dir));

        let after = git_stdout(&wt_dir, &["rev-parse", "HEAD"]);
        assert_eq!(
            before, after,
            "no new commit should be created for clean worktree"
        );
    }

    #[test]
    fn auto_commit_saves_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-ut2");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-ut2",
            "eng-ut2/99",
            &team_config_dir,
        )
        .unwrap();

        // Create untracked file (not staged).
        std::fs::write(wt_dir.join("new_file.txt"), "new content\n").unwrap();

        assert!(auto_commit_before_reset(&wt_dir));

        // Worktree should be clean.
        let status = git_stdout(&wt_dir, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "worktree should be clean after auto-commit"
        );
    }

    #[test]
    fn auto_clean_worktree_uses_commit_not_stash() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-ns");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-ns",
            "eng-ns/66",
            &team_config_dir,
        )
        .unwrap();

        // Create uncommitted changes.
        std::fs::write(wt_dir.join("work.txt"), "some work\n").unwrap();

        auto_clean_worktree(&wt_dir).unwrap();

        // Should be clean.
        let status = git_stdout(&wt_dir, &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "worktree should be clean");

        // No stashes should have been created.
        let stash = git_stdout(&wt_dir, &["stash", "list"]);
        assert!(
            stash.trim().is_empty(),
            "no stash should be created, got: {stash}"
        );

        // A wip commit should exist.
        let log = git_stdout(&wt_dir, &["log", "--oneline", "-1"]);
        assert!(
            log.contains("wip: auto-save"),
            "should have wip commit, got: {log}"
        );
    }

    #[test]
    fn preserve_worktree_with_commit_returns_false_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo
            .join(".batty")
            .join("worktrees")
            .join("eng-clean-preserve");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-clean-preserve",
            "eng-clean-preserve/101",
            &team_config_dir,
        )
        .unwrap();

        let saved = preserve_worktree_with_commit(
            &wt_dir,
            "wip: auto-save before restart [batty]",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!saved);
    }

    #[test]
    fn preserve_worktree_with_commit_saves_dirty_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-preserve");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-preserve",
            "eng-preserve/103",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(wt_dir.join("preserved.txt"), "keep this work\n").unwrap();

        let saved = preserve_worktree_with_commit(
            &wt_dir,
            "wip: auto-save before restart [batty]",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(saved, "dirty worktree should be auto-committed");

        let status = git_stdout(&wt_dir, &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "worktree should be clean");

        let log = git_stdout(&wt_dir, &["log", "--oneline", "-1"]);
        assert!(
            log.contains("wip: auto-save before restart [batty]"),
            "expected restart preservation commit, got: {log}"
        );
    }

    #[test]
    fn preserve_worktree_with_commit_ignores_batty_untracked_only() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo
            .join(".batty")
            .join("worktrees")
            .join("eng-batty-clean");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-batty-clean",
            "eng-batty-clean/104",
            &team_config_dir,
        )
        .unwrap();

        std::fs::create_dir_all(wt_dir.join(".batty").join("scratch")).unwrap();
        std::fs::write(
            wt_dir.join(".batty").join("scratch").join("session.log"),
            "transient\n",
        )
        .unwrap();

        let head_before = git_stdout(&wt_dir, &["rev-parse", "HEAD"]);
        let saved = preserve_worktree_with_commit(
            &wt_dir,
            "wip: auto-save before restart [batty]",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(
            !saved,
            "only .batty untracked files should not trigger commit"
        );

        let head_after = git_stdout(&wt_dir, &["rev-parse", "HEAD"]);
        assert_eq!(head_before, head_after, "no commit should be created");
    }

    #[test]
    fn preserve_worktree_with_commit_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let wt_dir = repo.join(".batty").join("worktrees").join("eng-timeout");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &wt_dir,
            "eng-timeout",
            "eng-timeout/102",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(wt_dir.join("slow.txt"), "pending\n").unwrap();

        let hook_path = PathBuf::from(git_stdout(
            &wt_dir,
            &["rev-parse", "--git-path", "hooks/pre-commit"],
        ));
        let hooks_dir = hook_path.parent().unwrap();
        std::fs::create_dir_all(hooks_dir).unwrap();
        std::fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = preserve_worktree_with_commit(
            &wt_dir,
            "wip: auto-save before restart [batty]",
            Duration::from_millis(200),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out"), "unexpected error: {err}");
    }

    // --- priority_rank tests ---

    #[test]
    fn priority_rank_known_values() {
        assert_eq!(priority_rank("critical"), 0);
        assert_eq!(priority_rank("high"), 1);
        assert_eq!(priority_rank("medium"), 2);
        assert_eq!(priority_rank("low"), 3);
    }

    #[test]
    fn priority_rank_unknown_returns_lowest() {
        assert_eq!(priority_rank(""), 4);
        assert_eq!(priority_rank("urgent"), 4);
        assert_eq!(priority_rank("CRITICAL"), 4); // case-sensitive
    }

    // --- next_unclaimed_task edge cases ---

    #[test]
    fn next_unclaimed_task_all_done_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 1, "done-task", "done", "high", None, &[]);
        write_task_file(
            tmp.path(),
            2,
            "in-progress-task",
            "in-progress",
            "critical",
            None,
            &[],
        );

        let task = next_unclaimed_task(tmp.path()).unwrap();
        assert!(task.is_none());
    }

    #[test]
    fn next_unclaimed_task_respects_backlog_status() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(
            tmp.path(),
            1,
            "backlog-task",
            "backlog",
            "medium",
            None,
            &[],
        );

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 1);
    }

    #[test]
    fn next_unclaimed_task_tiebreaks_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 10, "task-ten", "todo", "high", None, &[]);
        write_task_file(tmp.path(), 5, "task-five", "todo", "high", None, &[]);
        write_task_file(tmp.path(), 20, "task-twenty", "todo", "high", None, &[]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 5, "should pick lowest id when priority is tied");
    }

    #[test]
    fn next_unclaimed_task_skips_blocked_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file_with_workflow_frontmatter(tmp.path(), 1, "blocked-task", "blocked: yes\n");
        write_task_file(tmp.path(), 2, "free-task", "todo", "low", None, &[]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 2);
    }

    #[test]
    fn next_unclaimed_task_allows_done_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 1, "done-dep", "done", "low", None, &[]);
        write_task_file(tmp.path(), 2, "depends-on-done", "todo", "high", None, &[1]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 2, "task with done dependency should be available");
    }

    #[test]
    fn next_unclaimed_task_blocks_on_undone_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(
            tmp.path(),
            1,
            "in-progress-dep",
            "in-progress",
            "low",
            None,
            &[],
        );
        write_task_file(
            tmp.path(),
            2,
            "blocked-by-dep",
            "todo",
            "critical",
            None,
            &[1],
        );

        // Task 2 depends on task 1 which is in-progress — should not be picked
        let task = next_unclaimed_task(tmp.path()).unwrap();
        assert!(
            task.is_none(),
            "task with in-progress dependency should not be available"
        );
    }

    #[test]
    fn next_unclaimed_task_nonexistent_dependency_treated_as_done() {
        let tmp = tempfile::tempdir().unwrap();
        // Task depends on id 999 which doesn't exist — treated as satisfied
        write_task_file(tmp.path(), 1, "orphan-dep", "todo", "high", None, &[999]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 1);
    }

    // --- read_task_title edge cases ---

    #[test]
    fn read_task_title_quoted_title() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("007-quoted.md"),
            "---\ntitle: 'My Quoted Task'\nstatus: todo\n---\nBody\n",
        )
        .unwrap();
        let title = read_task_title(tmp.path(), 7);
        assert_eq!(title, "My Quoted Task");
    }

    #[test]
    fn read_task_title_double_quoted() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("008-double.md"),
            "---\ntitle: \"Double Quoted\"\nstatus: todo\n---\nBody\n",
        )
        .unwrap();
        let title = read_task_title(tmp.path(), 8);
        assert_eq!(title, "Double Quoted");
    }

    #[test]
    fn read_task_title_no_title_line_returns_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("009-no-title.md"),
            "---\nstatus: todo\npriority: low\n---\nBody\n",
        )
        .unwrap();
        let title = read_task_title(tmp.path(), 9);
        assert_eq!(title, "Task #9");
    }

    #[test]
    fn read_task_title_three_digit_id_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("123-big-id.md"),
            "---\ntitle: Big ID Task\nstatus: todo\n---\n",
        )
        .unwrap();
        let title = read_task_title(tmp.path(), 123);
        assert_eq!(title, "Big ID Task");
    }

    // --- engineer_base_branch_name ---

    #[test]
    fn engineer_base_branch_name_format() {
        assert_eq!(engineer_base_branch_name("eng-1-1"), "eng-main/eng-1-1");
        assert_eq!(engineer_base_branch_name("eng-2"), "eng-main/eng-2");
    }

    // --- map_git_error ---

    #[test]
    fn map_git_error_ok_passes_through() {
        let result: std::result::Result<i32, super::git_cmd::GitError> = Ok(42);
        let mapped = map_git_error(result, "test action");
        assert_eq!(mapped.unwrap(), 42);
    }

    #[test]
    fn map_git_error_err_wraps_message() {
        let result: std::result::Result<i32, super::git_cmd::GitError> =
            Err(super::git_cmd::GitError::Permanent {
                message: "git status failed".to_string(),
                stderr: "fatal: something".to_string(),
            });
        let err = map_git_error(result, "checking status").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("checking status"), "got: {msg}");
    }

    // --- cron edge cases ---

    #[test]
    fn cron_recycle_invalid_expression_skips() {
        let tmp = tempfile::tempdir().unwrap();
        write_cron_task(
            tmp.path(),
            1,
            "done",
            "not a cron expression",
            "cron_last_run: \"2020-01-01T00:00:00+00:00\"\n",
        );

        let recycled = recycle_cron_tasks(tmp.path()).unwrap();
        assert!(
            recycled.is_empty(),
            "invalid cron expression should be skipped"
        );
    }

    #[test]
    fn cron_recycle_no_last_run_defaults_to_yesterday() {
        let tmp = tempfile::tempdir().unwrap();
        // Done cron task with no cron_last_run — should use now - 1 day as reference
        write_cron_task(tmp.path(), 1, "done", "0 * * * * *", "");

        let recycled = recycle_cron_tasks(tmp.path()).unwrap();
        assert_eq!(
            recycled.len(),
            1,
            "should recycle even without cron_last_run"
        );
    }

    #[test]
    fn cron_recycle_future_trigger_skips() {
        let tmp = tempfile::tempdir().unwrap();
        // Set last run to now so next trigger is in the future
        let now = chrono::Utc::now().to_rfc3339();
        write_cron_task(
            tmp.path(),
            1,
            "done",
            "0 0 1 1 * 2099",
            &format!("cron_last_run: \"{now}\"\n"),
        );

        let recycled = recycle_cron_tasks(tmp.path()).unwrap();
        assert!(recycled.is_empty(), "future trigger should be skipped");
    }

    // --- sentinel tests for error resilience (#311) ---

    /// Refresh on a stale/nonexistent worktree should return Ok, not panic.
    #[test]
    fn refresh_nonexistent_worktree_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_worktree = tmp.path().join("does-not-exist");
        let team_cfg = tmp.path().join("team_config");
        std::fs::create_dir_all(&team_cfg).unwrap();

        let result = refresh_engineer_worktree(tmp.path(), &fake_worktree, "no-branch", &team_cfg);
        // Non-existent worktree should be handled gracefully (early return Ok)
        assert!(
            result.is_ok(),
            "refresh on nonexistent worktree should not panic: {result:?}"
        );
    }

    /// run_tests_in_worktree should return a clean error when cargo is not
    /// found, and should surface an invalid worktree as a failed test run
    /// instead of panicking.
    #[test]
    fn test_gating_missing_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_dir = tmp.path().join("missing-worktree");
        assert!(!fake_dir.exists(), "test requires a nonexistent directory");
        let result = run_tests_in_worktree(&fake_dir, None);
        let output = result.expect("missing worktree should surface as a failed test run");
        assert!(
            !output.passed,
            "run_tests_in_worktree on missing dir should fail cleanly"
        );
        let err_msg = output.output;
        assert!(
            err_msg.contains("No such file")
                || err_msg.contains("failed")
                || err_msg.contains("could not find"),
            "error should describe the failed test operation, got: {err_msg}"
        );
    }

    /// checkout_worktree_branch_from_main should propagate an error cleanly
    /// when run against a non-git directory, not panic.
    #[test]
    fn checkout_branch_in_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        // tmp is not a git repo, so git operations should fail
        let result = checkout_worktree_branch_from_main(tmp.path(), "fake-branch");
        assert!(
            result.is_err(),
            "checkout on non-git dir should return Err, not panic"
        );
    }

    /// Verify the production code in this file has zero bare .unwrap() or
    /// .expect() calls (only safe fallback variants like unwrap_or_default).
    #[test]
    fn no_panicking_unwraps_in_production_code() {
        let count = production_unwrap_expect_count(Path::new("src/team/task_loop.rs"));
        assert_eq!(
            count, 0,
            "production code should have zero bare .unwrap()/.expect() calls, found {count}"
        );
    }

    #[test]
    fn git_has_unresolved_conflicts_detects_unmerged_status_entries() {
        assert!(line_has_unresolved_conflict("UU src/team/verification.rs"));
        assert!(line_has_unresolved_conflict("AA src/lib.rs"));
        assert!(line_has_unresolved_conflict("DU src/main.rs"));
        assert!(!line_has_unresolved_conflict(" M src/main.rs"));
        assert!(!line_has_unresolved_conflict("?? scratch.txt"));
    }
}
