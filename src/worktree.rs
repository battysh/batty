//! Git worktree lifecycle for isolated phase runs.
//!
//! Each `batty work <phase>` run gets a dedicated branch/worktree:
//! `<phase-slug>-run-<NNN>`.
//! The executor runs in that worktree. Cleanup is merge-aware:
//! - merged runs are removed (worktree + branch)
//! - rejected/failed/unmerged runs are retained for inspection

use std::ffi::OsStr;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct PhaseWorktree {
    pub repo_root: PathBuf,
    pub base_branch: String,
    pub start_commit: String,
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AgentWorktree {
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Failed,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupDecision {
    Cleaned,
    KeptForReview,
    KeptForFailure,
}

impl PhaseWorktree {
    pub fn finalize(&self, outcome: RunOutcome) -> Result<CleanupDecision> {
        match outcome {
            RunOutcome::Failed => Ok(CleanupDecision::KeptForFailure),
            RunOutcome::DryRun => {
                remove_worktree(&self.repo_root, &self.path)?;
                delete_branch(&self.repo_root, &self.branch)?;
                Ok(CleanupDecision::Cleaned)
            }
            RunOutcome::Completed => {
                let branch_tip = current_commit(&self.repo_root, &self.branch)?;
                if branch_tip == self.start_commit {
                    return Ok(CleanupDecision::KeptForReview);
                }

                if is_merged_into_base(&self.repo_root, &self.branch, &self.base_branch)? {
                    remove_worktree(&self.repo_root, &self.path)?;
                    delete_branch(&self.repo_root, &self.branch)?;
                    Ok(CleanupDecision::Cleaned)
                } else {
                    Ok(CleanupDecision::KeptForReview)
                }
            }
        }
    }
}

/// Create an isolated git worktree for a phase run.
pub fn prepare_phase_worktree(project_root: &Path, phase: &str) -> Result<PhaseWorktree> {
    let repo_root = resolve_repo_root(project_root)?;
    let base_branch = current_branch(&repo_root)?;
    let start_commit = current_commit(&repo_root, "HEAD")?;
    let worktrees_root = repo_root.join(".batty").join("worktrees");

    std::fs::create_dir_all(&worktrees_root).with_context(|| {
        format!(
            "failed to create worktrees directory {}",
            worktrees_root.display()
        )
    })?;

    let phase_slug = sanitize_phase_for_branch(phase);
    let prefix = format!("{phase_slug}-run-");
    let mut run_number = next_run_number(&repo_root, &worktrees_root, &prefix)?;

    loop {
        let branch = format!("{prefix}{run_number:03}");
        let path = worktrees_root.join(&branch);

        if path.exists() || branch_exists(&repo_root, &branch)? {
            run_number += 1;
            continue;
        }

        let path_s = path.to_string_lossy().to_string();
        let add_output = run_git(
            &repo_root,
            [
                "worktree",
                "add",
                "-b",
                branch.as_str(),
                path_s.as_str(),
                base_branch.as_str(),
            ],
        )?;
        if !add_output.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&add_output.stderr).trim()
            );
        }

        return Ok(PhaseWorktree {
            repo_root,
            base_branch,
            start_commit,
            branch,
            path,
        });
    }
}

/// Resolve the phase worktree for a run.
///
/// Behavior:
/// - If `force_new` is false, resume the latest existing `<phase>-run-###` worktree if found.
/// - Otherwise (or if none exists), create a new worktree.
///
/// Returns `(worktree, resumed_existing)`.
pub fn resolve_phase_worktree(
    project_root: &Path,
    phase: &str,
    force_new: bool,
) -> Result<(PhaseWorktree, bool)> {
    if !force_new && let Some(existing) = latest_phase_worktree(project_root, phase)? {
        return Ok((existing, true));
    }

    Ok((prepare_phase_worktree(project_root, phase)?, false))
}

/// Prepare (or reuse) one worktree per parallel agent slot for a phase.
///
/// Layout:
/// - path: `.batty/worktrees/<phase>/<agent>/`
/// - branch: `batty/<phase-slug>/<agent-slug>`
pub fn prepare_agent_worktrees(
    project_root: &Path,
    phase: &str,
    agent_names: &[String],
    force_new: bool,
) -> Result<Vec<AgentWorktree>> {
    if agent_names.is_empty() {
        bail!("parallel agent worktree preparation requires at least one agent");
    }

    let repo_root = resolve_repo_root(project_root)?;
    let base_branch = current_branch(&repo_root)?;
    let phase_slug = sanitize_phase_for_branch(phase);
    let phase_dir = repo_root.join(".batty").join("worktrees").join(phase);
    std::fs::create_dir_all(&phase_dir).with_context(|| {
        format!(
            "failed to create agent worktree phase directory {}",
            phase_dir.display()
        )
    })?;

    let mut seen_agent_slugs = HashSet::new();
    for agent in agent_names {
        let slug = sanitize_phase_for_branch(agent);
        if !seen_agent_slugs.insert(slug.clone()) {
            bail!(
                "agent names contain duplicate sanitized slug '{}'; use unique agent names",
                slug
            );
        }
    }

    let mut worktrees = Vec::with_capacity(agent_names.len());
    for agent in agent_names {
        let agent_slug = sanitize_phase_for_branch(agent);
        let branch = format!("batty/{phase_slug}/{agent_slug}");
        let path = phase_dir.join(&agent_slug);

        if force_new {
            let _ = remove_worktree(&repo_root, &path);
            let _ = delete_branch(&repo_root, &branch);
        }

        if path.exists() {
            if !branch_exists(&repo_root, &branch)? {
                bail!(
                    "agent worktree path exists but branch is missing: {} ({})",
                    path.display(),
                    branch
                );
            }
            if !worktree_registered(&repo_root, &path)? {
                bail!(
                    "agent worktree path exists but is not registered in git worktree list: {}",
                    path.display()
                );
            }
        } else {
            let path_s = path.to_string_lossy().to_string();
            let add_output = if branch_exists(&repo_root, &branch)? {
                run_git(&repo_root, ["worktree", "add", path_s.as_str(), branch.as_str()])?
            } else {
                run_git(
                    &repo_root,
                    [
                        "worktree",
                        "add",
                        "-b",
                        branch.as_str(),
                        path_s.as_str(),
                        base_branch.as_str(),
                    ],
                )?
            };
            if !add_output.status.success() {
                bail!(
                    "git worktree add failed for agent '{}': {}",
                    agent,
                    String::from_utf8_lossy(&add_output.stderr).trim()
                );
            }
        }

        worktrees.push(AgentWorktree {
            branch,
            path,
        });
    }

    Ok(worktrees)
}

fn latest_phase_worktree(project_root: &Path, phase: &str) -> Result<Option<PhaseWorktree>> {
    let repo_root = resolve_repo_root(project_root)?;
    let base_branch = current_branch(&repo_root)?;
    let worktrees_root = repo_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(None);
    }

    let phase_slug = sanitize_phase_for_branch(phase);
    let prefix = format!("{phase_slug}-run-");
    let mut best: Option<(u32, String, PathBuf)> = None;

    for entry in std::fs::read_dir(&worktrees_root)
        .with_context(|| format!("failed to read {}", worktrees_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let branch = entry.file_name().to_string_lossy().to_string();
        let Some(run) = parse_run_number(&branch, &prefix) else {
            continue;
        };

        if !branch_exists(&repo_root, &branch)? {
            warn!(
                branch = %branch,
                path = %path.display(),
                "skipping stale phase worktree directory without branch"
            );
            continue;
        }

        match &best {
            Some((best_run, _, _)) if run <= *best_run => {}
            _ => best = Some((run, branch, path)),
        }
    }

    let Some((_, branch, path)) = best else {
        return Ok(None);
    };

    let start_commit = current_commit(&repo_root, &branch)?;
    Ok(Some(PhaseWorktree {
        repo_root,
        base_branch,
        start_commit,
        branch,
        path,
    }))
}

fn resolve_repo_root(project_root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(project_root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to run git in {}", project_root.display()))?;
    if !output.status.success() {
        bail!(
            "not a git repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("git rev-parse returned empty repository root");
    }
    Ok(PathBuf::from(root))
}

fn current_branch(repo_root: &Path) -> Result<String> {
    let output = run_git(repo_root, ["branch", "--show-current"])?;
    if !output.status.success() {
        bail!(
            "failed to determine current branch: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        bail!("detached HEAD is not supported for phase worktree runs; checkout a branch first");
    }
    Ok(branch)
}

fn next_run_number(repo_root: &Path, worktrees_root: &Path, prefix: &str) -> Result<u32> {
    let mut max_run = 0;

    let refs = run_git(
        repo_root,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?;
    if !refs.status.success() {
        bail!(
            "failed to list branches: {}",
            String::from_utf8_lossy(&refs.stderr).trim()
        );
    }

    for branch in String::from_utf8_lossy(&refs.stdout).lines() {
        if let Some(run) = parse_run_number(branch, prefix) {
            max_run = max_run.max(run);
        }
    }

    if worktrees_root.is_dir() {
        for entry in std::fs::read_dir(worktrees_root)
            .with_context(|| format!("failed to read {}", worktrees_root.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(run) = parse_run_number(name.as_ref(), prefix) {
                max_run = max_run.max(run);
            }
        }
    }

    Ok(max_run + 1)
}

fn parse_run_number(name: &str, prefix: &str) -> Option<u32> {
    let suffix = name.strip_prefix(prefix)?;
    if suffix.len() < 3 || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn sanitize_phase_for_branch(phase: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for c in phase.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "phase".to_string()
    } else {
        slug
    }
}

fn run_git<I, S>(repo_root: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", repo_root.display()))
}

fn branch_exists(repo_root: &Path, branch: &str) -> Result<bool> {
    let ref_name = format!("refs/heads/{branch}");
    let output = run_git(
        repo_root,
        ["show-ref", "--verify", "--quiet", ref_name.as_str()],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to check branch '{}': {}",
            branch,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn worktree_registered(repo_root: &Path, path: &Path) -> Result<bool> {
    let output = run_git(repo_root, ["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        bail!(
            "failed to list worktrees: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let target = path.to_string_lossy().to_string();
    let listed = String::from_utf8_lossy(&output.stdout);
    for line in listed.lines() {
        if let Some(candidate) = line.strip_prefix("worktree ")
            && candidate.trim() == target
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_merged_into_base(repo_root: &Path, branch: &str, base_branch: &str) -> Result<bool> {
    let output = run_git(
        repo_root,
        ["merge-base", "--is-ancestor", branch, base_branch],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to check merge status for '{}' into '{}': {}",
            branch,
            base_branch,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn current_commit(repo_root: &Path, rev: &str) -> Result<String> {
    let output = run_git(repo_root, ["rev-parse", rev])?;
    if !output.status.success() {
        bail!(
            "failed to resolve revision '{}': {}",
            rev,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.is_empty() {
        bail!("git rev-parse returned empty commit for '{rev}'");
    }
    Ok(commit)
}

fn remove_worktree(repo_root: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let path_s = path.to_string_lossy().to_string();
    let output = run_git(
        repo_root,
        ["worktree", "remove", "--force", path_s.as_str()],
    )?;
    if !output.status.success() {
        bail!(
            "failed to remove worktree '{}': {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn delete_branch(repo_root: &Path, branch: &str) -> Result<()> {
    if !branch_exists(repo_root, branch)? {
        return Ok(());
    }

    let output = run_git(repo_root, ["branch", "-D", branch])?;
    if !output.status.success() {
        bail!(
            "failed to delete branch '{}': {}",
            branch,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Sync the phase board from the source tree into the worktree.
///
/// Worktrees are created from committed state, so any uncommitted kanban
/// changes (new tasks, reworked boards, etc.) would be lost. This copies
/// the phase directory from `source_kanban_root/<phase>/` into
/// `worktree_kanban_root/<phase>/`, overwriting whatever git checked out.
///
/// Only syncs when the source directory exists and differs from the
/// worktree (i.e., the source tree has uncommitted kanban changes).
pub fn sync_phase_board_to_worktree(
    project_root: &Path,
    worktree_root: &Path,
    phase: &str,
) -> Result<()> {
    let source_phase_dir = crate::paths::resolve_kanban_root(project_root).join(phase);
    if !source_phase_dir.is_dir() {
        return Ok(());
    }

    let dest_kanban_root = crate::paths::resolve_kanban_root(worktree_root);
    let dest_phase_dir = dest_kanban_root.join(phase);

    // Remove stale destination and copy fresh.
    if dest_phase_dir.exists() {
        std::fs::remove_dir_all(&dest_phase_dir).with_context(|| {
            format!(
                "failed to remove stale phase board at {}",
                dest_phase_dir.display()
            )
        })?;
    }

    copy_dir_recursive(&source_phase_dir, &dest_phase_dir).with_context(|| {
        format!(
            "failed to sync phase board from {} to {}",
            source_phase_dir.display(),
            dest_phase_dir.display()
        )
    })?;

    info!(
        phase = phase,
        source = %source_phase_dir.display(),
        dest = %dest_phase_dir.display(),
        "synced phase board into worktree"
    );
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> Option<tempfile::TempDir> {
        if !git_available() {
            return None;
        }

        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(
            tmp.path(),
            &["config", "user.email", "batty-test@example.com"],
        );
        git(tmp.path(), &["config", "user.name", "Batty Test"]);

        fs::write(tmp.path().join("README.md"), "init\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "init"]);

        Some(tmp)
    }

    fn cleanup_worktree(repo_root: &Path, worktree: &PhaseWorktree) {
        let _ = remove_worktree(repo_root, &worktree.path);
        let _ = delete_branch(repo_root, &worktree.branch);
    }

    fn cleanup_agent_worktrees(repo_root: &Path, worktrees: &[AgentWorktree]) {
        for wt in worktrees {
            let _ = remove_worktree(repo_root, &wt.path);
            let _ = delete_branch(repo_root, &wt.branch);
        }
    }

    #[test]
    fn sanitize_phase_for_branch_normalizes_phase() {
        assert_eq!(sanitize_phase_for_branch("phase-2.5"), "phase-2-5");
        assert_eq!(sanitize_phase_for_branch("Phase 7"), "phase-7");
        assert_eq!(sanitize_phase_for_branch("///"), "phase");
    }

    #[test]
    fn parse_run_number_extracts_suffix() {
        assert_eq!(parse_run_number("phase-2-run-001", "phase-2-run-"), Some(1));
        assert_eq!(
            parse_run_number("phase-2-run-1234", "phase-2-run-"),
            Some(1234)
        );
        assert_eq!(parse_run_number("phase-2-run-aa1", "phase-2-run-"), None);
        assert_eq!(parse_run_number("other-001", "phase-2-run-"), None);
    }

    #[test]
    fn prepare_phase_worktree_increments_run_number() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let first = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let second = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();

        assert!(
            first.branch.ends_with("001"),
            "first branch: {}",
            first.branch
        );
        assert!(
            second.branch.ends_with("002"),
            "second branch: {}",
            second.branch
        );
        assert!(first.path.is_dir());
        assert!(second.path.is_dir());

        cleanup_worktree(tmp.path(), &first);
        cleanup_worktree(tmp.path(), &second);
    }

    #[test]
    fn finalize_keeps_unmerged_completed_worktree() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let decision = worktree.finalize(RunOutcome::Completed).unwrap();

        assert_eq!(decision, CleanupDecision::KeptForReview);
        assert!(worktree.path.exists());
        assert!(branch_exists(tmp.path(), &worktree.branch).unwrap());

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn finalize_keeps_failed_worktree() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let decision = worktree.finalize(RunOutcome::Failed).unwrap();

        assert_eq!(decision, CleanupDecision::KeptForFailure);
        assert!(worktree.path.exists());
        assert!(branch_exists(tmp.path(), &worktree.branch).unwrap());

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn finalize_cleans_when_merged() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();

        fs::write(worktree.path.join("work.txt"), "done\n").unwrap();
        git(&worktree.path, &["add", "work.txt"]);
        git(&worktree.path, &["commit", "-q", "-m", "worktree change"]);

        git(
            tmp.path(),
            &["merge", "--no-ff", "--no-edit", worktree.branch.as_str()],
        );

        let decision = worktree.finalize(RunOutcome::Completed).unwrap();
        assert_eq!(decision, CleanupDecision::Cleaned);
        assert!(!worktree.path.exists());
        assert!(!branch_exists(tmp.path(), &worktree.branch).unwrap());
    }

    #[test]
    fn resolve_phase_worktree_resumes_latest_existing_by_default() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let first = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let second = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();

        let (resolved, resumed) = resolve_phase_worktree(tmp.path(), "phase-2.5", false).unwrap();
        assert!(
            resumed,
            "expected default behavior to resume existing worktree"
        );
        assert_eq!(
            resolved.branch, second.branch,
            "should resume latest run branch"
        );
        assert_eq!(resolved.path, second.path, "should resume latest run path");

        cleanup_worktree(tmp.path(), &first);
        cleanup_worktree(tmp.path(), &second);
    }

    #[test]
    fn resolve_phase_worktree_force_new_creates_next_run() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let first = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let (resolved, resumed) = resolve_phase_worktree(tmp.path(), "phase-2.5", true).unwrap();

        assert!(!resumed, "force-new should never resume prior worktree");
        assert_ne!(resolved.branch, first.branch);
        assert!(
            resolved.branch.ends_with("002"),
            "branch: {}",
            resolved.branch
        );

        cleanup_worktree(tmp.path(), &first);
        cleanup_worktree(tmp.path(), &resolved);
    }

    #[test]
    fn resolve_phase_worktree_without_existing_creates_new() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let (resolved, resumed) = resolve_phase_worktree(tmp.path(), "phase-2.5", false).unwrap();
        assert!(!resumed);
        assert!(
            resolved.branch.ends_with("001"),
            "branch: {}",
            resolved.branch
        );

        cleanup_worktree(tmp.path(), &resolved);
    }

    #[test]
    fn prepare_agent_worktrees_creates_layout_and_branches() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent-1".to_string(), "agent-2".to_string()];
        let worktrees = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();

        assert_eq!(worktrees.len(), 2);
        assert_eq!(
            worktrees[0].path,
            tmp.path()
                .join(".batty")
                .join("worktrees")
                .join("phase-4")
                .join("agent-1")
        );
        assert_eq!(worktrees[0].branch, "batty/phase-4/agent-1");
        assert!(branch_exists(tmp.path(), "batty/phase-4/agent-1").unwrap());
        assert!(branch_exists(tmp.path(), "batty/phase-4/agent-2").unwrap());

        cleanup_agent_worktrees(tmp.path(), &worktrees);
    }

    #[test]
    fn prepare_agent_worktrees_reuses_existing_agent_paths() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent-1".to_string(), "agent-2".to_string()];
        let first = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();
        let second = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();

        assert_eq!(first[0].path, second[0].path);
        assert_eq!(first[1].path, second[1].path);
        assert_eq!(first[0].branch, second[0].branch);
        assert_eq!(first[1].branch, second[1].branch);

        cleanup_agent_worktrees(tmp.path(), &first);
    }

    #[test]
    fn prepare_agent_worktrees_rejects_duplicate_sanitized_names() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent 1".to_string(), "agent-1".to_string()];
        let err = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate sanitized slug"));
    }

    #[test]
    fn prepare_agent_worktrees_force_new_recreates_worktrees() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent-1".to_string()];
        let first = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();

        fs::write(first[0].path.join("agent.txt"), "agent-1\n").unwrap();
        git(&first[0].path, &["add", "agent.txt"]);
        git(&first[0].path, &["commit", "-q", "-m", "agent work"]);

        let second = prepare_agent_worktrees(tmp.path(), "phase-4", &names, true).unwrap();
        let listing = run_git(tmp.path(), ["branch", "--list", "batty/phase-4/agent-1"]).unwrap();
        assert!(listing.status.success());
        assert!(second[0].path.exists());

        cleanup_agent_worktrees(tmp.path(), &second);
    }

    #[test]
    fn sync_phase_board_copies_uncommitted_tasks_into_worktree() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a committed phase board with one task.
        let kanban = tmp.path().join(".batty").join("kanban");
        let phase_dir = kanban.join("my-phase").join("tasks");
        fs::create_dir_all(&phase_dir).unwrap();
        fs::write(phase_dir.join("001-old.md"), "old task\n").unwrap();
        fs::write(
            kanban.join("my-phase").join("config.yml"),
            "version: 10\nnext_id: 2\n",
        )
        .unwrap();
        git(tmp.path(), &["add", ".batty"]);
        git(tmp.path(), &["commit", "-q", "-m", "add phase board"]);

        // Create a worktree — it will have the committed (old) board.
        let worktree = prepare_phase_worktree(tmp.path(), "my-phase").unwrap();
        let wt_task = worktree
            .path
            .join(".batty")
            .join("kanban")
            .join("my-phase")
            .join("tasks")
            .join("001-old.md");
        assert!(wt_task.exists(), "worktree should have committed task");

        // Now add a new task to the source tree (uncommitted).
        fs::write(phase_dir.join("002-new.md"), "new task\n").unwrap();

        // Sync.
        sync_phase_board_to_worktree(tmp.path(), &worktree.path, "my-phase").unwrap();

        // Worktree should now have both tasks.
        let wt_tasks_dir = worktree
            .path
            .join(".batty")
            .join("kanban")
            .join("my-phase")
            .join("tasks");
        assert!(wt_tasks_dir.join("001-old.md").exists());
        assert!(
            wt_tasks_dir.join("002-new.md").exists(),
            "uncommitted task should be synced into worktree"
        );

        // The new file content should match.
        let content = fs::read_to_string(wt_tasks_dir.join("002-new.md")).unwrap();
        assert_eq!(content, "new task\n");

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn sync_phase_board_overwrites_stale_worktree_board() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a committed phase board.
        let kanban = tmp.path().join(".batty").join("kanban");
        let phase_dir = kanban.join("my-phase").join("tasks");
        fs::create_dir_all(&phase_dir).unwrap();
        fs::write(phase_dir.join("001-old.md"), "original\n").unwrap();
        fs::write(
            kanban.join("my-phase").join("config.yml"),
            "version: 10\nnext_id: 2\n",
        )
        .unwrap();
        git(tmp.path(), &["add", ".batty"]);
        git(tmp.path(), &["commit", "-q", "-m", "add phase board"]);

        let worktree = prepare_phase_worktree(tmp.path(), "my-phase").unwrap();

        // Rewrite the source task (uncommitted change).
        fs::write(phase_dir.join("001-old.md"), "rewritten\n").unwrap();

        sync_phase_board_to_worktree(tmp.path(), &worktree.path, "my-phase").unwrap();

        let wt_content = fs::read_to_string(
            worktree
                .path
                .join(".batty")
                .join("kanban")
                .join("my-phase")
                .join("tasks")
                .join("001-old.md"),
        )
        .unwrap();
        assert_eq!(
            wt_content, "rewritten\n",
            "worktree board should reflect source tree changes"
        );

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn sync_phase_board_noop_when_source_missing() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "nonexistent").unwrap();

        // Should not error when source phase dir doesn't exist.
        sync_phase_board_to_worktree(tmp.path(), &worktree.path, "nonexistent").unwrap();

        cleanup_worktree(tmp.path(), &worktree);
    }
}
