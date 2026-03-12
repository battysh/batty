//! Task-loop helpers extracted from the team daemon.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

pub(crate) struct MergeLock {
    path: PathBuf,
}

impl MergeLock {
    pub fn acquire(project_root: &Path) -> Result<Self> {
        let path = project_root.join(".batty").join("merge.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = std::time::Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > std::time::Duration::from_secs(60) {
                        bail!("merge lock timeout after 60s: {}", path.display());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => bail!("failed to acquire merge lock: {e}"),
            }
        }
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub(crate) enum MergeOutcome {
    Success,
    RebaseConflict(String),
}

fn priority_rank(p: &str) -> u32 {
    match p {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

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

pub(crate) fn run_tests_in_worktree(worktree_dir: &Path) -> Result<(bool, String)> {
    let output = std::process::Command::new("cargo")
        .arg("test")
        .current_dir(worktree_dir)
        .output()
        .context("failed to run cargo test in worktree")?;

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

    Ok((output.status.success(), trimmed))
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
        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                branch_name,
                &worktree_dir.to_string_lossy(),
                "main",
            ])
            .current_dir(project_root)
            .output()
            .context("failed to create git worktree")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("already exists") {
                let output = std::process::Command::new("git")
                    .args([
                        "worktree",
                        "add",
                        &worktree_dir.to_string_lossy(),
                        branch_name,
                    ])
                    .current_dir(project_root)
                    .output()
                    .context("failed to create git worktree")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    bail!("git worktree add failed: {stderr}");
                }
            } else {
                bail!("git worktree add failed: {stderr}");
            }
        }

        info!(worktree = %worktree_dir.display(), branch = branch_name, "created engineer worktree");
    }

    ensure_engineer_worktree_links(worktree_dir, team_config_dir)?;

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
    setup_engineer_worktree(project_root, worktree_dir, &base_branch, team_config_dir)?;
    maybe_migrate_legacy_engineer_worktree(
        project_root,
        worktree_dir,
        engineer_name,
        &base_branch,
    )?;
    ensure_task_branch_namespace_available(project_root, engineer_name)?;

    if worktree_has_user_changes(worktree_dir)? {
        bail!(
            "engineer worktree '{}' at {} has uncommitted changes",
            engineer_name,
            worktree_dir.display()
        );
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

#[allow(dead_code)] // Retained for existing tests and as a lower-level helper.
pub(crate) fn refresh_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }

    if worktree_has_user_changes(worktree_dir)? {
        warn!(
            worktree = %worktree_dir.display(),
            branch = branch_name,
            "skipping worktree refresh because worktree is dirty"
        );
        return Ok(());
    }

    let up_to_date = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", "main", branch_name])
        .current_dir(project_root)
        .output()
        .context("failed to compare worktree branch with main")?;
    if up_to_date.status.success() {
        return Ok(());
    }

    let rebase = std::process::Command::new("git")
        .args(["rebase", "main"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to rebase engineer worktree")?;
    if rebase.status.success() {
        info!(
            worktree = %worktree_dir.display(),
            branch = branch_name,
            "refreshed engineer worktree"
        );
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
    let _ = std::process::Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(worktree_dir)
        .output();

    let remove = std::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_dir.to_string_lossy(),
        ])
        .current_dir(project_root)
        .output()
        .context("failed to remove conflicted worktree")?;
    if !remove.status.success() {
        let remove_stderr = String::from_utf8_lossy(&remove.stderr);
        bail!("git worktree remove --force failed after rebase error '{stderr}': {remove_stderr}");
    }

    let delete = std::process::Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(project_root)
        .output()
        .context("failed to delete conflicted worktree branch")?;
    if !delete.status.success() {
        let delete_stderr = String::from_utf8_lossy(&delete.stderr);
        bail!("git branch -D failed after rebase error '{stderr}': {delete_stderr}");
    }

    warn!(
        worktree = %worktree_dir.display(),
        branch = branch_name,
        rebase_error = %stderr,
        "recreating engineer worktree after rebase conflict"
    );
    setup_engineer_worktree(project_root, worktree_dir, branch_name, team_config_dir)?;
    Ok(())
}

/// Merge an engineer's worktree branch into main.
pub fn merge_engineer_branch(project_root: &Path, engineer_name: &str) -> Result<MergeOutcome> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        bail!(
            "no worktree found for '{}' at {}",
            engineer_name,
            worktree_dir.display()
        );
    }

    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&worktree_dir)
        .output()
        .context("failed to get worktree branch")?;

    if !output.status.success() {
        bail!("failed to determine worktree branch");
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!(engineer = engineer_name, branch = %branch, "merging worktree branch");

    let rebase = std::process::Command::new("git")
        .args(["rebase", "main"])
        .current_dir(&worktree_dir)
        .output()
        .context("failed to rebase engineer branch onto main")?;

    if !rebase.status.success() {
        let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
        let _ = std::process::Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(&worktree_dir)
            .output();
        warn!(engineer = engineer_name, branch = %branch, "rebase conflict during merge");
        return Ok(MergeOutcome::RebaseConflict(stderr));
    }

    let output = std::process::Command::new("git")
        .args(["merge", &branch, "--no-edit"])
        .current_dir(project_root)
        .output()
        .context("git merge failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("merge failed: {stderr}");
    }

    println!("Merged branch '{branch}' from {engineer_name}");

    // Reset worktree to main tip so it's ready for the next task.
    if let Err(e) = reset_engineer_worktree(project_root, engineer_name) {
        warn!(
            engineer = engineer_name,
            error = %e,
            "worktree reset failed after merge"
        );
    }

    Ok(MergeOutcome::Success)
}

pub(crate) fn reset_engineer_worktree(project_root: &Path, engineer_name: &str) -> Result<()> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        return Ok(());
    }

    let previous_branch = current_worktree_branch(&worktree_dir)?;
    let base_branch = engineer_base_branch_name(engineer_name);
    if let Err(error) = checkout_worktree_branch_from_main(&worktree_dir, &base_branch) {
        warn!(
            engineer = engineer_name,
            error = %error,
            "failed to reset worktree after merge"
        );
        return Ok(());
    }

    if previous_branch != base_branch
        && (previous_branch == engineer_name
            || previous_branch.starts_with(&format!("{engineer_name}/")))
        && branch_is_merged_into(project_root, &previous_branch, "main")?
        && let Err(error) = delete_branch(project_root, &previous_branch)
    {
        warn!(
            engineer = engineer_name,
            branch = %previous_branch,
            error = %error,
            "failed to delete merged engineer task branch"
        );
    }

    info!(
        engineer = engineer_name,
        branch = %base_branch,
        worktree = %worktree_dir.display(),
        "reset worktree to main after merge"
    );
    Ok(())
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

fn worktree_has_user_changes(worktree_dir: &Path) -> Result<bool> {
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to inspect worktree status")?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        bail!("git status --porcelain failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&status.stdout)
        .lines()
        .any(|line| !line.starts_with("?? .batty/")))
}

fn current_worktree_branch(worktree_dir: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to determine worktree branch")?;
    if !output.status.success() {
        bail!("failed to determine worktree branch");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn checkout_worktree_branch_from_main(worktree_dir: &Path, branch_name: &str) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(["checkout", "-B", branch_name, "main"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| format!("failed to switch worktree to branch '{branch_name}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git checkout -B {branch_name} main failed: {stderr}");
    }
    Ok(())
}

fn branch_exists(project_root: &Path, branch_name: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch_name}"),
        ])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to check whether branch '{branch_name}' exists"))?;
    Ok(output.status.success())
}

fn branch_is_checked_out_in_any_worktree(project_root: &Path, branch_name: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .context("failed to list git worktrees")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree list --porcelain failed: {stderr}");
    }

    let target = format!("branch refs/heads/{branch_name}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == target))
}

fn branch_is_merged_into(
    project_root: &Path,
    branch_name: &str,
    base_branch: &str,
) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", branch_name, base_branch])
        .current_dir(project_root)
        .output()
        .with_context(|| {
            format!("failed to compare branch '{branch_name}' with '{base_branch}'")
        })?;
    Ok(output.status.success())
}

fn delete_branch(project_root: &Path, branch_name: &str) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to delete branch '{branch_name}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git branch -D {branch_name} failed: {stderr}");
    }
    Ok(())
}

fn archived_legacy_branch_name(project_root: &Path, engineer_name: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", engineer_name])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to resolve legacy branch '{engineer_name}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse --short {engineer_name} failed: {stderr}");
    }

    let short_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut candidate = format!("legacy/{engineer_name}-{short_sha}");
    let mut counter = 1usize;
    while branch_exists(project_root, &candidate)? {
        counter += 1;
        candidate = format!("legacy/{engineer_name}-{short_sha}-{counter}");
    }
    Ok(candidate)
}

fn rename_branch(project_root: &Path, old_branch: &str, new_branch: &str) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(["branch", "-m", old_branch, new_branch])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to rename branch '{old_branch}' to '{new_branch}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git branch -m {old_branch} {new_branch} failed: {stderr}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Output};

    fn git(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {:?} failed to run: {e}", args))
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
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

    #[test]
    fn merge_rejects_missing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let err = merge_engineer_branch(tmp.path(), "eng-1-1").unwrap_err();
        assert!(err.to_string().contains("no worktree found"));
    }

    #[test]
    fn test_merge_lock_acquire_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let lock_path = tmp.path().join(".batty").join("merge.lock");

        {
            let lock = MergeLock::acquire(tmp.path()).unwrap();
            assert!(lock_path.exists());
            drop(lock);
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_merge_with_rebase_picks_up_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        std::fs::write(repo.join("other.txt"), "main work\n").unwrap();
        git_ok(&repo, &["add", "other.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));

        assert!(repo.join("feature.txt").exists());
        assert!(repo.join("other.txt").exists());
    }

    #[test]
    fn test_reset_worktree_after_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));

        let main_head = git_stdout(&repo, &["rev-parse", "HEAD"]);
        let wt_head = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        assert_eq!(main_head, wt_head);
    }

    #[test]
    fn test_reset_worktree_restores_engineer_base_branch_after_task_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            "eng-1/task-42",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-1")
        );

        let branch_check = git(&repo, &["rev-parse", "--verify", "eng-1/task-42"]);
        assert!(
            !branch_check.status.success(),
            "merged task branch should be deleted"
        );
    }

    #[test]
    fn test_reset_worktree_leaves_clean_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("new.txt"), "content\n").unwrap();
        git_ok(&worktree_dir, &["add", "new.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add file"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));

        let status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        let tracked_changes: Vec<&str> = status
            .lines()
            .filter(|line| !line.starts_with("?? .batty/"))
            .collect();
        assert!(
            tracked_changes.is_empty(),
            "worktree has tracked changes: {:?}",
            tracked_changes
        );
    }

    #[test]
    fn test_merge_rebase_conflict_returns_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-2");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("conflict.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "add conflict file"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("conflict.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "conflict.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("conflict.txt"), "main version\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        let result = merge_engineer_branch(&repo, "eng-2").unwrap();
        assert!(matches!(result, MergeOutcome::RebaseConflict(_)));

        let status = git(&worktree_dir, &["status", "--porcelain"]);
        assert!(status.status.success());
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
            "eng-5/task-123",
            &team_config_dir,
        )
        .unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-5/task-123"
        );
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );
        assert!(worktree_dir.join(".batty").join("team_config").exists());
    }

    #[test]
    fn test_prepare_assignment_worktree_rejects_dirty_worktree() {
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

        let err = prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-6",
            "eng-6/task-7",
            &team_config_dir,
        )
        .unwrap_err();
        assert!(err.to_string().contains("uncommitted changes"));
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
            "eng-6b/task-17",
            &team_config_dir,
        )
        .unwrap();

        let legacy_check = git(&repo, &["rev-parse", "--verify", "eng-6b"]);
        assert!(!legacy_check.status.success());
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-6b/task-17"
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
            "eng-7/task-99",
            &team_config_dir,
        )
        .unwrap();

        let legacy_check = git(&repo, &["rev-parse", "--verify", "eng-7"]);
        assert!(!legacy_check.status.success());
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-7/task-99"
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
            "eng-8/task-100",
            &team_config_dir,
        )
        .unwrap();

        let legacy_check = git(&repo, &["rev-parse", "--verify", "eng-8"]);
        assert!(!legacy_check.status.success());
        assert!(!git_stdout(&repo, &["branch", "--list", "legacy/eng-8-*"]).is_empty());
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-8/task-100"
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
        let (passed, output) = run_tests_in_worktree(worktree).unwrap();
        assert!(passed);
        assert!(output.contains("test result: ok"));

        std::fs::write(
            worktree.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn fails() {\n        assert_eq!(2 + 2, 5);\n    }\n}\n",
        )
        .unwrap();
        let (passed, output) = run_tests_in_worktree(worktree).unwrap();
        assert!(!passed);
        assert!(output.contains("FAILED"));
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
}
