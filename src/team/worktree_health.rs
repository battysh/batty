use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::config::RoleType;
use super::hierarchy;

fn git_program() -> &'static str {
    for program in ["git", "/usr/bin/git", "/opt/homebrew/bin/git"] {
        if Command::new(program).arg("--version").output().is_ok() {
            return program;
        }
    }
    "git"
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeHealth {
    member: String,
    path: PathBuf,
    exists: bool,
    registered: bool,
    branch: Option<String>,
    dirty: bool,
    conflicts: bool,
    missing_git_link: bool,
    lock_files: Vec<String>,
    behind_main: Option<u64>,
}

pub fn run(project_root: &Path) -> Result<String> {
    let statuses = collect_worktree_health(project_root)?;
    Ok(render_report(project_root, &statuses))
}

fn collect_worktree_health(project_root: &Path) -> Result<Vec<WorktreeHealth>> {
    let team_config = super::config::TeamConfig::load(&super::team_config_path(project_root))?;
    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let registered_worktrees = registered_worktree_paths(project_root)?;

    let mut statuses = members
        .into_iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .map(|member| {
            let path = if member.use_worktrees {
                project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(&member.name)
            } else {
                project_root.to_path_buf()
            };
            inspect_worktree(
                project_root,
                &member.name,
                path,
                member.use_worktrees,
                &registered_worktrees,
            )
        })
        .collect::<Vec<_>>();
    statuses.sort_by(|a, b| a.member.cmp(&b.member));
    Ok(statuses)
}

fn inspect_worktree(
    project_root: &Path,
    member_name: &str,
    path: PathBuf,
    use_worktrees: bool,
    registered_worktrees: &HashSet<PathBuf>,
) -> WorktreeHealth {
    let exists = path.exists();
    let registered = !use_worktrees || registered_worktrees.contains(&path);
    let missing_git_link = use_worktrees && !git_link_present(&path);
    let branch = git_output(&path, &["branch", "--show-current"]);
    let dirty = git_output(&path, &["status", "--porcelain"])
        .is_some_and(|output| !output.trim().is_empty());
    let conflicts = git_output(&path, &["status", "--porcelain"])
        .is_some_and(|output| has_merge_conflicts(&output));
    let lock_files = find_lock_files(&path);
    let behind_main = branch
        .as_ref()
        .and_then(|_| behind_main_count(project_root, &path));

    WorktreeHealth {
        member: member_name.to_string(),
        path,
        exists,
        registered,
        branch,
        dirty,
        conflicts,
        missing_git_link,
        lock_files,
        behind_main,
    }
}

fn render_report(project_root: &Path, statuses: &[WorktreeHealth]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Batty worktree health for {}\n\n",
        project_root.display()
    ));
    out.push_str("MEMBER               STATUS  BRANCH                BEHIND  NOTES\n");
    out.push_str("--------------------------------------------------------------------------\n");

    for status in statuses {
        let state = overall_state(status);
        let behind = status
            .behind_main
            .map(|count| count.to_string())
            .unwrap_or_else(|| "-".to_string());
        let branch = status.branch.as_deref().unwrap_or("-");
        let notes = status_notes(status);
        out.push_str(&format!(
            "{:<20} {:<7} {:<21} {:<7} {}\n",
            status.member, state, branch, behind, notes
        ));
    }

    out
}

fn overall_state(status: &WorktreeHealth) -> &'static str {
    if !status.exists || !status.registered || status.missing_git_link || status.conflicts {
        "fail"
    } else if !status.lock_files.is_empty() || status.behind_main.unwrap_or(0) > 0 || status.dirty {
        "warn"
    } else {
        "ok"
    }
}

fn status_notes(status: &WorktreeHealth) -> String {
    let mut notes = Vec::new();
    if !status.exists {
        notes.push(format!("missing path {}", status.path.display()));
    }
    if !status.registered {
        notes.push("orphaned from git worktree list".to_string());
    }
    if status.missing_git_link {
        notes.push("missing .git link".to_string());
    }
    if status.conflicts {
        notes.push("merge conflicts detected".to_string());
    }
    if !status.lock_files.is_empty() {
        notes.push(format!("lock files: {}", status.lock_files.join(", ")));
    }
    if status.dirty {
        notes.push("dirty".to_string());
    }
    if status.behind_main.unwrap_or(0) > 0 {
        notes.push(format!(
            "{} commits behind main",
            status.behind_main.unwrap_or(0)
        ));
    }
    if notes.is_empty() {
        "healthy".to_string()
    } else {
        notes.join("; ")
    }
}

fn registered_worktree_paths(project_root: &Path) -> Result<HashSet<PathBuf>> {
    let output = Command::new(git_program())
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .with_context(|| {
            format!(
                "failed to query git worktree list in {}",
                project_root.display()
            )
        })?;
    if !output.status.success() {
        return Ok(HashSet::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = HashSet::new();
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            paths.insert(PathBuf::from(path));
        }
    }
    Ok(paths)
}

fn git_output(path: &Path, args: &[&str]) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let output = Command::new(git_program())
        .args(args)
        .current_dir(path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn git_link_present(path: &Path) -> bool {
    let git_path = path.join(".git");
    if git_path.is_dir() {
        return true;
    }
    if !git_path.is_file() {
        return false;
    }
    let Ok(contents) = std::fs::read_to_string(&git_path) else {
        return false;
    };
    let Some(gitdir) = contents.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let resolved = path.join(gitdir.trim());
    resolved.exists()
}

fn has_merge_conflicts(status_output: &str) -> bool {
    status_output.lines().any(|line| {
        let bytes = line.as_bytes();
        bytes.len() >= 2
            && matches!(
                (bytes[0], bytes[1]),
                (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D')
            )
    })
}

fn find_lock_files(path: &Path) -> Vec<String> {
    let mut locks = Vec::new();
    let Some(git_dir_text) = git_output(path, &["rev-parse", "--git-dir"]) else {
        return locks;
    };
    let git_dir = if Path::new(&git_dir_text).is_absolute() {
        PathBuf::from(git_dir_text)
    } else {
        path.join(git_dir_text)
    };

    let candidates = [
        ("index.lock", git_dir.join("index.lock")),
        ("refs/lock", git_dir.join("refs").join("lock")),
    ];
    for (label, candidate) in candidates {
        if candidate.exists() {
            locks.push(label.to_string());
        }
    }
    locks
}

fn behind_main_count(project_root: &Path, path: &Path) -> Option<u64> {
    let output = Command::new(git_program())
        .args(["rev-list", "--left-right", "--count", "HEAD...main"])
        .current_dir(path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.split_whitespace();
    let _ahead = parts.next()?;
    let behind = parts.next()?.parse::<u64>().ok()?;

    // Only surface staleness for actual worktrees inside this repo.
    if path.starts_with(project_root) {
        Some(behind)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            Command::new(git_program())
                .args(["init", "-b", "main"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(git_program())
                .args(["config", "user.email", "batty@example.com"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(git_program())
                .args(["config", "user.name", "Batty Tests"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(tmp.path().join("README.md"), "hello\n").unwrap();
        assert!(
            Command::new(git_program())
                .args(["add", "README.md"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(git_program())
                .args(["commit", "-m", "init"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        tmp
    }

    #[test]
    fn git_link_present_detects_missing_link() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        assert!(!git_link_present(tmp.path()));
    }

    #[test]
    fn find_lock_files_reports_index_lock() {
        let repo = init_repo();
        let git_dir = repo.path().join(".git");
        std::fs::write(git_dir.join("index.lock"), "").unwrap();
        assert_eq!(find_lock_files(repo.path()), vec!["index.lock".to_string()]);
    }

    #[test]
    fn behind_main_count_reports_stale_branch() {
        let repo = init_repo();
        assert!(
            Command::new(git_program())
                .args(["checkout", "-b", "feature"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(git_program())
                .args(["checkout", "main"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.path().join("README.md"), "hello\nworld\n").unwrap();
        assert!(
            Command::new(git_program())
                .args(["commit", "-am", "advance main"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(git_program())
                .args(["checkout", "feature"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(behind_main_count(repo.path(), repo.path()), Some(1));
    }
}
