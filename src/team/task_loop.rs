//! Task-loop helpers extracted from the team daemon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

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
                "HEAD",
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

    Ok(worktree_dir.to_path_buf())
}

pub(crate) fn refresh_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }

    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to inspect worktree status")?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        bail!("git status --porcelain failed: {stderr}");
    }

    let dirty = String::from_utf8_lossy(&status.stdout)
        .lines()
        .any(|line| !line.starts_with("?? .batty/"));
    if dirty {
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
pub fn merge_engineer_branch(project_root: &Path, engineer_name: &str) -> Result<()> {
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
