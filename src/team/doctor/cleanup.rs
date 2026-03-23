use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use super::super::config::TeamConfig;
use super::super::git_cmd;
use super::util::{
    display_cleanup_path, is_task_branch, launch_state_path, load_team_config,
};
use super::{CleanupPlan, CleanupSummary, OrphanStatus};
use crate::task::load_tasks_from_dir;

use super::checks::active_task_targets;

pub(super) fn detect_orphans(project_root: &Path) -> Result<OrphanStatus> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(OrphanStatus {
            branches: Vec::new(),
            worktrees: Vec::new(),
        });
    }

    let tasks = load_tasks_from_dir(&tasks_dir)?;
    let active_tasks: Vec<_> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "review"))
        .collect();
    let active_targets = active_task_targets(project_root, &active_tasks);

    let mut branches: Vec<_> = list_task_branches(project_root)?
        .into_iter()
        .filter(|branch| !active_targets.branches.contains(branch))
        .collect();
    branches.sort();

    let mut worktrees: Vec<_> = list_worktree_dirs(project_root)?
        .into_iter()
        .filter(|path| !active_targets.worktrees.contains(path))
        .collect();
    worktrees.sort();

    Ok(OrphanStatus {
        branches,
        worktrees,
    })
}

pub(super) fn detect_cleanup_plan(project_root: &Path) -> Result<CleanupPlan> {
    let orphan_status = detect_orphans(project_root)?;
    let team_config = load_team_config(project_root)?;
    let mut stale_state = detect_stale_state(project_root, team_config.as_ref());
    stale_state.sort();
    let orphan_test_sessions = crate::tmux::list_sessions_with_prefix("batty-test-");

    Ok(CleanupPlan {
        orphan_status,
        stale_state,
        orphan_test_sessions,
    })
}

fn detect_stale_state(project_root: &Path, team_config: Option<&TeamConfig>) -> Vec<PathBuf> {
    let Some(team_config) = team_config else {
        return Vec::new();
    };

    let session = format!("batty-{}", team_config.name);
    if crate::tmux::session_exists(&session) {
        return Vec::new();
    }

    stale_state_candidates(project_root)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

fn stale_state_candidates(project_root: &Path) -> Vec<PathBuf> {
    vec![
        launch_state_path(project_root),
        super::super::daemon_state_path(project_root),
        project_root.join(".batty").join("merge.lock"),
    ]
}

fn list_task_branches(project_root: &Path) -> Result<Vec<String>> {
    Ok(git_cmd::for_each_ref_branches(project_root)?
        .into_iter()
        .filter(|branch| is_task_branch(branch))
        .collect())
}

pub(super) fn list_worktree_dirs(project_root: &Path) -> Result<Vec<PathBuf>> {
    let worktrees_root = project_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(&worktrees_root)
        .with_context(|| format!("failed to read {}", worktrees_root.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn cleanup_orphans(project_root: &Path, orphan_status: &OrphanStatus) -> Result<CleanupSummary> {
    let mut worktrees_removed = 0usize;
    let mut actions = Vec::new();
    for worktree in &orphan_status.worktrees {
        git_cmd::worktree_remove(project_root, worktree, true).map_err(|error| {
            anyhow::anyhow!(
                "failed to remove worktree '{}': {error}",
                worktree.display()
            )
        })?;
        info!(path = %worktree.display(), "doctor removed orphan worktree");
        actions.push(format!(
            "removed orphan worktree '{}'",
            display_cleanup_path(project_root, worktree)
        ));
        worktrees_removed += 1;
    }

    let mut branches_removed = 0usize;
    for branch in &orphan_status.branches {
        git_cmd::branch_delete(project_root, branch)
            .map_err(|error| anyhow::anyhow!("failed to delete branch '{branch}': {error}"))?;
        info!(branch, "doctor deleted orphan branch");
        actions.push(format!("deleted orphan branch '{branch}'"));
        branches_removed += 1;
    }

    Ok(CleanupSummary {
        branches_removed,
        worktrees_removed,
        stale_state_removed: 0,
        test_sessions_removed: 0,
        actions,
    })
}

fn cleanup_stale_state(project_root: &Path, stale_state: &[PathBuf]) -> Result<CleanupSummary> {
    let mut stale_state_removed = 0usize;
    let mut actions = Vec::new();

    for path in stale_state {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove stale state '{}'", path.display()))?;
        info!(path = %path.display(), "doctor removed stale state file");
        actions.push(format!(
            "removed stale state '{}'",
            display_cleanup_path(project_root, path)
        ));
        stale_state_removed += 1;
    }

    Ok(CleanupSummary {
        branches_removed: 0,
        worktrees_removed: 0,
        stale_state_removed,
        test_sessions_removed: 0,
        actions,
    })
}

fn cleanup_test_sessions(sessions: &[String]) -> CleanupSummary {
    let mut removed = 0usize;
    let mut actions = Vec::new();
    for session in sessions {
        if crate::tmux::kill_session(session).is_ok() {
            info!(session, "doctor killed orphaned test session");
            actions.push(format!("killed orphaned test session '{session}'"));
            removed += 1;
        }
    }
    CleanupSummary {
        branches_removed: 0,
        worktrees_removed: 0,
        stale_state_removed: 0,
        test_sessions_removed: removed,
        actions,
    }
}

pub(super) fn apply_cleanup_plan(
    project_root: &Path,
    cleanup_plan: &CleanupPlan,
) -> Result<CleanupSummary> {
    let orphan_summary = cleanup_orphans(project_root, &cleanup_plan.orphan_status)?;
    let stale_state_summary = cleanup_stale_state(project_root, &cleanup_plan.stale_state)?;
    let test_session_summary = cleanup_test_sessions(&cleanup_plan.orphan_test_sessions);

    let mut actions = orphan_summary.actions;
    actions.extend(stale_state_summary.actions);
    actions.extend(test_session_summary.actions);

    Ok(CleanupSummary {
        branches_removed: orphan_summary.branches_removed + stale_state_summary.branches_removed,
        worktrees_removed: orphan_summary.worktrees_removed + stale_state_summary.worktrees_removed,
        stale_state_removed: orphan_summary.stale_state_removed
            + stale_state_summary.stale_state_removed,
        test_sessions_removed: test_session_summary.test_sessions_removed,
        actions,
    })
}

pub(super) fn render_cleanup_plan(project_root: &Path, cleanup_plan: &CleanupPlan) -> String {
    let mut out = String::new();
    out.push_str("== Cleanup Plan ==\n");
    if cleanup_plan.is_empty() {
        out.push_str("No orphan branches, worktrees, stale state, or test sessions to clean up.\n");
        return out;
    }

    for worktree in &cleanup_plan.orphan_status.worktrees {
        out.push_str(&format!(
            "remove_worktree: {}\n",
            display_cleanup_path(project_root, worktree)
        ));
    }
    for branch in &cleanup_plan.orphan_status.branches {
        out.push_str(&format!("delete_branch: {branch}\n"));
    }
    for path in &cleanup_plan.stale_state {
        out.push_str(&format!(
            "remove_stale_state: {}\n",
            display_cleanup_path(project_root, path)
        ));
    }
    for session in &cleanup_plan.orphan_test_sessions {
        out.push_str(&format!("kill_test_session: {session}\n"));
    }

    out
}

pub(super) fn render_cleanup_summary(summary: &CleanupSummary) -> String {
    let mut out = String::new();
    out.push_str("== Cleanup ==\n");
    out.push_str(&format!(
        "removed_branches: {}\nremoved_worktrees: {}\nremoved_stale_state: {}\nremoved_test_sessions: {}\n",
        summary.branches_removed, summary.worktrees_removed, summary.stale_state_removed,
        summary.test_sessions_removed,
    ));
    for action in &summary.actions {
        out.push_str(&format!("action: {action}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::super::util::launch_state_path;
    use super::*;

    fn write_named_team_config(root: &Path, name: &str) {
        let team_dir = root.join(".batty").join("team_config");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("team.yaml"),
            format!(
                r#"
name: {name}
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: codex
  - name: engineer
    role_type: engineer
    agent: codex
    use_worktrees: true
"#
            ),
        )
        .unwrap();
        fs::write(
            team_dir.join("architect.md"),
            "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
        )
        .unwrap();
        fs::write(team_dir.join("manager.md"), "Manager prompt").unwrap();
        fs::write(team_dir.join("engineer.md"), "Engineer prompt").unwrap();
    }

    fn write_stale_state_files(root: &Path) -> Vec<PathBuf> {
        let batty_dir = root.join(".batty");
        fs::create_dir_all(&batty_dir).unwrap();

        let paths = vec![
            launch_state_path(root),
            super::super::super::daemon_state_path(root),
            batty_dir.join("merge.lock"),
        ];

        fs::write(&paths[0], "{}").unwrap();
        fs::write(
            &paths[1],
            r#"{"clean_shutdown":true,"saved_at":1,"states":{},"active_tasks":{}}"#,
        )
        .unwrap();
        fs::write(&paths[2], "locked").unwrap();
        paths
    }

    fn init_git_repo(root: &Path) {
        git_ok(root, &["init", "-b", "main"]);
        git_ok(root, &["config", "user.email", "batty-test@example.com"]);
        git_ok(root, &["config", "user.name", "Batty Test"]);
        fs::write(root.join("README.md"), "initial\n").unwrap();
        git_ok(root, &["add", "README.md"]);
        git_ok(root, &["commit", "-m", "initial"]);
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("git {:?} failed to run: {error}", args));
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_claimed_task(root: &Path, id: u32, status: &str, claimed_by: &str) {
        let tasks_dir = root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let content = format!(
            "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: medium\nclaimed_by: {claimed_by}\nclass: standard\n---\n\nTask body.\n"
        );
        fs::write(tasks_dir.join(format!("{id:03}-task-{id}.md")), content).unwrap();
    }

    #[test]
    fn detect_orphans_ignores_active_claimed_task_targets() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_claimed_task(tmp.path(), 72, "in-progress", "eng-1");

        git_ok(tmp.path(), &["branch", "eng-1/72"]);
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("eng-1")).unwrap();

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("eng-9");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-9",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );

        let orphans = detect_orphans(tmp.path()).unwrap();
        assert_eq!(orphans.branches, vec!["eng-9/task-9".to_string()]);
        assert_eq!(orphans.worktrees, vec![orphan_worktree]);
    }

    #[test]
    fn cleanup_orphans_removes_detected_branch_and_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_claimed_task(tmp.path(), 72, "review", "eng-1");

        git_ok(tmp.path(), &["branch", "eng-1/72"]);
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("eng-1")).unwrap();

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("eng-9");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-9",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );

        let summary = cleanup_orphans(tmp.path(), &detect_orphans(tmp.path()).unwrap()).unwrap();

        assert_eq!(summary.branches_removed, 1);
        assert_eq!(summary.worktrees_removed, 1);
        assert_eq!(summary.stale_state_removed, 0);
        assert!(
            summary
                .actions
                .contains(&"deleted orphan branch 'eng-9/task-9'".to_string())
        );
        assert!(!orphan_worktree.exists());
        assert_eq!(
            list_task_branches(tmp.path()).unwrap(),
            vec!["eng-1/72".to_string()]
        );
    }

    #[test]
    fn detect_cleanup_plan_includes_stale_state_when_session_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let team_name = format!("doctor-cleanup-{}", std::process::id());
        write_named_team_config(tmp.path(), &team_name);
        let mut stale_paths = write_stale_state_files(tmp.path());
        stale_paths.sort();

        let cleanup_plan = detect_cleanup_plan(tmp.path()).unwrap();

        assert!(cleanup_plan.orphan_status.branches.is_empty());
        assert!(cleanup_plan.orphan_status.worktrees.is_empty());
        assert_eq!(cleanup_plan.stale_state, stale_paths);
    }

    #[test]
    fn detect_stale_state_returns_empty_without_team_config() {
        let tmp = tempfile::tempdir().unwrap();
        write_stale_state_files(tmp.path());

        let stale = detect_stale_state(tmp.path(), None);

        assert!(stale.is_empty());
    }

    #[test]
    fn stale_state_candidates_match_expected_paths() {
        let tmp = tempfile::tempdir().unwrap();

        let candidates = stale_state_candidates(tmp.path());

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], launch_state_path(tmp.path()));
        assert_eq!(
            candidates[1],
            super::super::super::daemon_state_path(tmp.path())
        );
        assert_eq!(candidates[2], tmp.path().join(".batty").join("merge.lock"));
    }

    #[test]
    fn cleanup_stale_state_removes_files_and_reports_actions() {
        let tmp = tempfile::tempdir().unwrap();
        let stale_paths = write_stale_state_files(tmp.path());

        let summary = cleanup_stale_state(tmp.path(), &stale_paths).unwrap();

        assert_eq!(summary.branches_removed, 0);
        assert_eq!(summary.worktrees_removed, 0);
        assert_eq!(summary.stale_state_removed, 3);
        assert!(
            summary
                .actions
                .iter()
                .any(|action| action.contains(".batty/launch-state.json"))
        );
        for path in stale_paths {
            assert!(!path.exists(), "{} should be removed", path.display());
        }
    }

    #[test]
    fn render_cleanup_plan_reports_empty_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = CleanupPlan {
            orphan_status: OrphanStatus {
                branches: Vec::new(),
                worktrees: Vec::new(),
            },
            stale_state: Vec::new(),
            orphan_test_sessions: Vec::new(),
        };

        let rendered = render_cleanup_plan(tmp.path(), &plan);

        assert!(rendered.contains("== Cleanup Plan =="));
        assert!(
            rendered.contains(
                "No orphan branches, worktrees, stale state, or test sessions to clean up."
            )
        );
    }

    #[test]
    fn render_cleanup_summary_lists_counts_and_actions() {
        let summary = CleanupSummary {
            branches_removed: 1,
            worktrees_removed: 2,
            stale_state_removed: 3,
            test_sessions_removed: 0,
            actions: vec!["deleted orphan branch 'eng-1/task-1'".to_string()],
        };

        let rendered = render_cleanup_summary(&summary);

        assert!(rendered.contains("removed_branches: 1"));
        assert!(rendered.contains("removed_worktrees: 2"));
        assert!(rendered.contains("removed_stale_state: 3"));
        assert!(rendered.contains("action: deleted orphan branch 'eng-1/task-1'"));
    }

    #[test]
    fn render_cleanup_plan_includes_test_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = CleanupPlan {
            orphan_status: OrphanStatus {
                branches: Vec::new(),
                worktrees: Vec::new(),
            },
            stale_state: Vec::new(),
            orphan_test_sessions: vec!["batty-test-stale-1".to_string()],
        };

        let rendered = render_cleanup_plan(tmp.path(), &plan);

        assert!(rendered.contains("kill_test_session: batty-test-stale-1"));
    }

    #[test]
    fn render_cleanup_summary_includes_test_session_count() {
        let summary = CleanupSummary {
            branches_removed: 0,
            worktrees_removed: 0,
            stale_state_removed: 0,
            test_sessions_removed: 3,
            actions: vec!["killed orphaned test session 'batty-test-x'".to_string()],
        };

        let rendered = render_cleanup_summary(&summary);

        assert!(rendered.contains("removed_test_sessions: 3"));
        assert!(rendered.contains("killed orphaned test session 'batty-test-x'"));
    }

    #[test]
    fn run_fix_yes_cleans_orphans_and_stale_state() {
        let tmp = tempfile::tempdir().unwrap();
        let team_name = format!("doctor-run-fix-{}", std::process::id());
        init_git_repo(tmp.path());
        write_named_team_config(tmp.path(), &team_name);
        write_claimed_task(tmp.path(), 72, "review", "eng-1");

        git_ok(tmp.path(), &["branch", "eng-1/72"]);
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("eng-1")).unwrap();

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("eng-9");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-9",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );
        let stale_paths = write_stale_state_files(tmp.path());

        let output = super::super::run(tmp.path(), true, true).unwrap();

        assert!(output.contains("== Cleanup Plan =="));
        assert!(output.contains("delete_branch: eng-9/task-9"));
        assert!(output.contains("remove_worktree: .batty/worktrees/eng-9"));
        assert!(output.contains("remove_stale_state: .batty/launch-state.json"));
        assert!(output.contains("removed_branches: 1"));
        assert!(output.contains("removed_worktrees: 1"));
        assert!(output.contains("removed_stale_state: 3"));
        assert!(output.contains("action: deleted orphan branch 'eng-9/task-9'"));
        assert!(!orphan_worktree.exists());
        for path in stale_paths {
            assert!(!path.exists(), "{} should be removed", path.display());
        }
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn doctor_detects_orphaned_test_sessions() {
        let session1 = format!("batty-test-doctor-orphan-{}-a", std::process::id());
        let session2 = format!("batty-test-doctor-orphan-{}-b", std::process::id());
        let _ = crate::tmux::kill_session(&session1);
        let _ = crate::tmux::kill_session(&session2);

        crate::tmux::create_session(&session1, "sleep", &["30".to_string()], "/tmp").unwrap();
        crate::tmux::create_session(&session2, "sleep", &["30".to_string()], "/tmp").unwrap();

        let sessions = crate::tmux::list_sessions_with_prefix("batty-test-");
        assert!(
            sessions.contains(&session1),
            "should detect orphaned test session 1"
        );
        assert!(
            sessions.contains(&session2),
            "should detect orphaned test session 2"
        );

        // Verify cleanup_test_sessions kills them
        let summary = cleanup_test_sessions(&[session1.clone(), session2.clone()]);
        assert_eq!(summary.test_sessions_removed, 2);
        assert!(!crate::tmux::session_exists(&session1));
        assert!(!crate::tmux::session_exists(&session2));
    }
}
