//! Serialized merge queue for parallel agent branches.
//!
//! Completed tasks enqueue branch merge requests. The queue processes one item
//! at a time to avoid concurrent writes to the target branch.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    pub task_id: u32,
    pub agent: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    pub task_id: u32,
    pub agent: String,
    pub branch: String,
}

pub struct MergeQueue {
    repo_root: PathBuf,
    target_branch: String,
    verify_command: String,
    rebase_retries: u32,
    queue: VecDeque<MergeRequest>,
}

impl MergeQueue {
    pub fn new(
        repo_root: PathBuf,
        target_branch: String,
        verify_command: String,
        rebase_retries: u32,
    ) -> Self {
        Self {
            repo_root,
            target_branch,
            verify_command,
            rebase_retries,
            queue: VecDeque::new(),
        }
    }

    pub fn enqueue(&mut self, request: MergeRequest) {
        self.queue.push_back(request);
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn process_next(&mut self) -> Result<Option<MergeResult>> {
        let Some(request) = self.queue.pop_front() else {
            return Ok(None);
        };

        rebase_branch_onto_target(
            &self.repo_root,
            &request.branch,
            &self.target_branch,
            self.rebase_retries,
        )?;

        // Test gate after rebase, before merge.
        let verify = run_shell_in_repo(&self.repo_root, &self.verify_command)?;
        if !verify.status.success() {
            bail!(
                "merge queue test gate failed for branch '{}': {}",
                request.branch,
                String::from_utf8_lossy(&verify.stderr).trim()
            );
        }

        switch_branch(&self.repo_root, &self.target_branch)?;
        let merge = run_git(
            &self.repo_root,
            ["merge", "--ff-only", request.branch.as_str()],
        )?;
        if !merge.status.success() {
            bail!(
                "ff-only merge failed for branch '{}': {}",
                request.branch,
                String::from_utf8_lossy(&merge.stderr).trim()
            );
        }

        Ok(Some(MergeResult {
            task_id: request.task_id,
            agent: request.agent,
            branch: request.branch,
        }))
    }
}

fn switch_branch(repo_root: &Path, branch: &str) -> Result<()> {
    let output = run_git(repo_root, ["switch", branch])?;
    if !output.status.success() {
        bail!(
            "failed to switch to branch '{}': {}",
            branch,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn rebase_branch_onto_target(
    repo_root: &Path,
    branch: &str,
    target: &str,
    retries: u32,
) -> Result<()> {
    for attempt in 0..=retries {
        switch_branch(repo_root, branch)?;
        let rebase = run_git(repo_root, ["rebase", target])?;
        if rebase.status.success() {
            switch_branch(repo_root, target)?;
            return Ok(());
        }

        let _ = run_git(repo_root, ["rebase", "--abort"]);
        switch_branch(repo_root, target)?;

        if attempt == retries {
            bail!(
                "rebase failed for branch '{}' onto '{}': {}",
                branch,
                target,
                String::from_utf8_lossy(&rebase.stderr).trim()
            );
        }

        // Refresh target branch before retrying. This is best-effort because
        // not all repos have an upstream configured.
        let _ = run_git(repo_root, ["pull", "--rebase"]);
    }

    unreachable!("retry loop should have returned or failed")
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

fn run_shell_in_repo(repo_root: &Path, command: &str) -> Result<Output> {
    Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(repo_root)
        .output()
        .with_context(|| {
            format!(
                "failed to run shell command '{}' in {}",
                command,
                repo_root.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(repo: &Path, args: &[&str]) -> Output {
        Command::new("git").current_dir(repo).args(args).output().unwrap()
    }

    fn init_repo() -> Option<(tempfile::TempDir, String)> {
        let version = Command::new("git").arg("--version").output().ok()?;
        if !version.status.success() {
            return None;
        }

        let tmp = tempfile::tempdir().ok()?;
        if !git(tmp.path(), &["init", "-q"]).status.success() {
            return None;
        }
        let _ = git(
            tmp.path(),
            &["config", "user.email", "batty-merge-queue@example.com"],
        );
        let _ = git(tmp.path(), &["config", "user.name", "Batty Merge Queue"]);
        fs::write(tmp.path().join("README.md"), "base\n").ok()?;
        let _ = git(tmp.path(), &["add", "README.md"]);
        let _ = git(tmp.path(), &["commit", "-q", "-m", "init"]);

        let branch = String::from_utf8_lossy(&git(tmp.path(), &["branch", "--show-current"]).stdout)
            .trim()
            .to_string();
        if branch.is_empty() {
            return None;
        }
        Some((tmp, branch))
    }

    #[test]
    fn processes_queue_in_fifo_order() {
        let Some((tmp, base)) = init_repo() else {
            return;
        };

        assert!(git(tmp.path(), &["switch", "-c", "agent-a"]).status.success());
        fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
        assert!(git(tmp.path(), &["add", "a.txt"]).status.success());
        assert!(git(tmp.path(), &["commit", "-q", "-m", "a"]).status.success());

        assert!(git(tmp.path(), &["switch", &base]).status.success());
        assert!(git(tmp.path(), &["switch", "-c", "agent-b"]).status.success());
        fs::write(tmp.path().join("b.txt"), "b\n").unwrap();
        assert!(git(tmp.path(), &["add", "b.txt"]).status.success());
        assert!(git(tmp.path(), &["commit", "-q", "-m", "b"]).status.success());
        assert!(git(tmp.path(), &["switch", &base]).status.success());

        let mut queue = MergeQueue::new(
            tmp.path().to_path_buf(),
            base.clone(),
            "true".to_string(),
            1,
        );
        queue.enqueue(MergeRequest {
            task_id: 1,
            agent: "agent-a".to_string(),
            branch: "agent-a".to_string(),
        });
        queue.enqueue(MergeRequest {
            task_id: 2,
            agent: "agent-b".to_string(),
            branch: "agent-b".to_string(),
        });

        let first = queue.process_next().unwrap().unwrap();
        let second = queue.process_next().unwrap().unwrap();
        assert_eq!(first.agent, "agent-a");
        assert_eq!(second.agent, "agent-b");
        assert!(queue.is_empty());
    }

    #[test]
    fn test_gate_failure_blocks_merge() {
        let Some((tmp, base)) = init_repo() else {
            return;
        };

        assert!(git(tmp.path(), &["switch", "-c", "agent-a"]).status.success());
        fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
        assert!(git(tmp.path(), &["add", "a.txt"]).status.success());
        assert!(git(tmp.path(), &["commit", "-q", "-m", "a"]).status.success());
        assert!(git(tmp.path(), &["switch", &base]).status.success());

        let mut queue = MergeQueue::new(
            tmp.path().to_path_buf(),
            base.clone(),
            "false".to_string(),
            1,
        );
        queue.enqueue(MergeRequest {
            task_id: 1,
            agent: "agent-a".to_string(),
            branch: "agent-a".to_string(),
        });

        let err = queue.process_next().unwrap_err().to_string();
        assert!(err.contains("test gate failed"));
    }

    #[test]
    fn unresolved_conflict_fails_after_retry() {
        let Some((tmp, base)) = init_repo() else {
            return;
        };

        fs::write(tmp.path().join("conflict.txt"), "base\n").unwrap();
        assert!(git(tmp.path(), &["add", "conflict.txt"]).status.success());
        assert!(
            git(tmp.path(), &["commit", "-q", "-m", "base conflict"])
                .status
                .success()
        );

        assert!(git(tmp.path(), &["switch", "-c", "agent-a"]).status.success());
        fs::write(tmp.path().join("conflict.txt"), "agent\n").unwrap();
        assert!(git(tmp.path(), &["add", "conflict.txt"]).status.success());
        assert!(git(tmp.path(), &["commit", "-q", "-m", "agent edit"]).status.success());

        assert!(git(tmp.path(), &["switch", &base]).status.success());
        fs::write(tmp.path().join("conflict.txt"), "target\n").unwrap();
        assert!(git(tmp.path(), &["add", "conflict.txt"]).status.success());
        assert!(
            git(tmp.path(), &["commit", "-q", "-m", "target edit"])
                .status
                .success()
        );

        let mut queue = MergeQueue::new(
            tmp.path().to_path_buf(),
            base.clone(),
            "true".to_string(),
            1,
        );
        queue.enqueue(MergeRequest {
            task_id: 9,
            agent: "agent-a".to_string(),
            branch: "agent-a".to_string(),
        });

        let err = queue.process_next().unwrap_err().to_string();
        assert!(err.contains("rebase failed"));
    }
}
