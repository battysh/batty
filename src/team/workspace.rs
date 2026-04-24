use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tracing::warn;

use super::config::WorkspaceType;
use super::task_loop::{prepare_multi_repo_assignment_worktree, setup_multi_repo_worktree};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRepoTarget {
    pub label: Option<String>,
    pub path: PathBuf,
}

pub fn engineer_workspace_dir(
    project_root: &Path,
    workspace_type: WorkspaceType,
    engineer: &str,
) -> PathBuf {
    match workspace_type {
        WorkspaceType::Generic => project_root.join(".batty").join("worktrees").join(engineer),
        WorkspaceType::Brazil => brazil_workspace_src_dir(project_root, engineer),
    }
}

pub fn brazil_workspace_root(project_root: &Path, engineer: &str) -> PathBuf {
    project_root
        .parent()
        .unwrap_or(project_root)
        .join(".batty-brazil")
        .join(engineer)
}

pub fn brazil_workspace_src_dir(project_root: &Path, engineer: &str) -> PathBuf {
    brazil_workspace_root(project_root, engineer).join("src")
}

pub fn remove_empty_brazil_workspace_root(project_root: &Path, engineer: &str) -> Result<()> {
    let root = brazil_workspace_root(project_root, engineer);
    let src = root.join("src");
    if src.exists() && src.read_dir()?.next().is_none() {
        std::fs::remove_dir(&src)
            .with_context(|| format!("failed to remove empty {}", src.display()))?;
    }
    if root.exists() && root.read_dir()?.next().is_none() {
        std::fs::remove_dir(&root)
            .with_context(|| format!("failed to remove empty {}", root.display()))?;
    }
    Ok(())
}

pub fn workspace_repo_targets(
    worktree_path: &Path,
    is_multi_repo: bool,
    sub_repo_names: &[String],
) -> Vec<WorkspaceRepoTarget> {
    if !is_multi_repo {
        return vec![WorkspaceRepoTarget {
            label: None,
            path: worktree_path.to_path_buf(),
        }];
    }

    sub_repo_names
        .iter()
        .filter_map(|name| {
            let path = worktree_path.join(name);
            path.is_dir().then(|| WorkspaceRepoTarget {
                label: Some(name.clone()),
                path,
            })
        })
        .collect()
}

pub fn setup_workspace_worktree(
    project_root: &Path,
    workspace_type: WorkspaceType,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
    sub_repo_names: &[String],
) -> Result<PathBuf> {
    setup_multi_repo_worktree(
        project_root,
        worktree_dir,
        branch_name,
        team_config_dir,
        sub_repo_names,
    )?;
    register_brazil_workspace_if_needed(workspace_type, worktree_dir, sub_repo_names)?;
    Ok(worktree_dir.to_path_buf())
}

pub fn prepare_workspace_assignment_worktree(
    project_root: &Path,
    workspace_type: WorkspaceType,
    worktree_dir: &Path,
    engineer_name: &str,
    task_branch: &str,
    team_config_dir: &Path,
    sub_repo_names: &[String],
) -> Result<PathBuf> {
    prepare_multi_repo_assignment_worktree(
        project_root,
        worktree_dir,
        engineer_name,
        task_branch,
        team_config_dir,
        sub_repo_names,
    )?;
    register_brazil_workspace_if_needed(workspace_type, worktree_dir, sub_repo_names)?;
    Ok(worktree_dir.to_path_buf())
}

fn register_brazil_workspace_if_needed(
    workspace_type: WorkspaceType,
    workspace_src_dir: &Path,
    sub_repo_names: &[String],
) -> Result<()> {
    if !workspace_type.is_brazil() {
        return Ok(());
    }

    if !brazil_available() {
        warn!(
            workspace = %workspace_src_dir.display(),
            "Brazil workspace registration skipped because `brazil` is unavailable"
        );
        return Ok(());
    }

    for repo_name in sub_repo_names {
        let output = Command::new("brazil")
            .args(["ws", "use", &format!("./{repo_name}")])
            .current_dir(workspace_src_dir)
            .output()
            .with_context(|| {
                format!(
                    "failed to register Brazil package '{repo_name}' in {}",
                    workspace_src_dir.display()
                )
            })?;
        if !output.status.success() {
            anyhow::bail!(
                "Brazil registration failed for package '{}' in {}: {}",
                repo_name,
                workspace_src_dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    Ok(())
}

fn brazil_available() -> bool {
    Command::new("brazil")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brazil_workspace_dir_is_sibling_workspace_src() {
        let project_root = Path::new("/ws/src");

        assert_eq!(
            engineer_workspace_dir(project_root, WorkspaceType::Brazil, "eng-1"),
            PathBuf::from("/ws/.batty-brazil/eng-1/src")
        );
    }

    #[test]
    fn generic_workspace_dir_preserves_existing_layout() {
        let project_root = Path::new("/repo");

        assert_eq!(
            engineer_workspace_dir(project_root, WorkspaceType::Generic, "eng-1"),
            PathBuf::from("/repo/.batty/worktrees/eng-1")
        );
    }

    #[test]
    fn repo_targets_resolve_nested_subrepos() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("pkg-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("pkg-b")).unwrap();

        let targets = workspace_repo_targets(
            tmp.path(),
            true,
            &[
                "pkg-a".to_string(),
                "pkg-b".to_string(),
                "missing".to_string(),
            ],
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].label.as_deref(), Some("pkg-a"));
        assert_eq!(targets[1].label.as_deref(), Some("pkg-b"));
    }
}
