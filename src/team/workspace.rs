use std::path::{Path, PathBuf};

use anyhow::Result;

use super::task_loop::{
    prepare_multi_repo_assignment_worktree_from_trunk, setup_multi_repo_worktree_from_trunk,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRepoTarget {
    pub label: Option<String>,
    pub path: PathBuf,
}

pub fn engineer_workspace_dir(project_root: &Path, engineer: &str) -> PathBuf {
    project_root.join(".batty").join("worktrees").join(engineer)
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
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<PathBuf> {
    setup_multi_repo_worktree_from_trunk(
        project_root,
        worktree_dir,
        branch_name,
        team_config_dir,
        sub_repo_names,
        trunk_branch,
    )?;
    Ok(worktree_dir.to_path_buf())
}

pub struct WorkspaceAssignmentWorktree<'a> {
    pub project_root: &'a Path,
    pub worktree_dir: &'a Path,
    pub engineer_name: &'a str,
    pub task_branch: &'a str,
    pub team_config_dir: &'a Path,
    pub sub_repo_names: &'a [String],
    pub trunk_branch: &'a str,
}

pub fn prepare_workspace_assignment_worktree(
    assignment: WorkspaceAssignmentWorktree<'_>,
) -> Result<PathBuf> {
    prepare_multi_repo_assignment_worktree_from_trunk(
        assignment.project_root,
        assignment.worktree_dir,
        assignment.engineer_name,
        assignment.task_branch,
        assignment.team_config_dir,
        assignment.sub_repo_names,
        assignment.trunk_branch,
    )?;
    Ok(assignment.worktree_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engineer_workspace_dir_preserves_existing_layout() {
        let project_root = Path::new("/repo");

        assert_eq!(
            engineer_workspace_dir(project_root, "eng-1"),
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
