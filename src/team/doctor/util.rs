use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use super::super::config::TeamConfig;
use super::super::hierarchy::MemberInstance;
use super::super::prompt_compose::{render_member_prompt, resolve_prompt_context};
use super::{CheckLevel, CheckLine, DoctorDaemonState, LaunchIdentityRecord};

pub(super) fn check_line(level: CheckLevel, message: impl Into<String>) -> CheckLine {
    CheckLine {
        level,
        message: message.into(),
    }
}

pub(super) fn resolve_task_worktree(project_root: &Path, worktree_path: &str) -> PathBuf {
    let path = PathBuf::from(worktree_path);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

pub(super) fn is_task_branch(branch: &str) -> bool {
    branch.starts_with("eng-")
        && branch
            .split_once('/')
            .is_some_and(|(_, suffix)| suffix.starts_with("task-") || suffix.parse::<u32>().is_ok())
}

pub(super) fn is_engineer_name(name: &str) -> bool {
    name.starts_with("eng-")
}

pub(super) fn prompt_yes_no(msg: &str, default_yes: bool) -> Result<bool> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default_yes);
    }
    Ok(trimmed.starts_with('y') || trimmed.starts_with('Y'))
}

pub(super) fn current_prompt(member: &MemberInstance, config_dir: &Path) -> String {
    strip_nudge_section(&render_member_prompt(
        member,
        config_dir,
        &resolve_prompt_context(member),
    ))
}

pub(super) fn strip_nudge_section(prompt: &str) -> String {
    let mut lines = Vec::new();
    let mut in_nudge = false;

    for line in prompt.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge && line.starts_with("## ") {
            in_nudge = false;
        }
        if !in_nudge {
            lines.push(line);
        }
    }

    lines.join("\n").trim_end().to_string()
}

pub(super) fn short_prompt_hash(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

pub(super) fn canonical_agent_name(agent_name: &str) -> String {
    match agent_name {
        "claude" | "claude-code" => "claude-code".to_string(),
        "codex" | "codex-cli" => "codex-cli".to_string(),
        "kiro" | "kiro-cli" => "kiro-cli".to_string(),
        _ => agent_name.to_string(),
    }
}

pub(super) fn git_output(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

pub(super) fn load_team_config(project_root: &Path) -> Result<Option<TeamConfig>> {
    let path = super::super::team_config_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(TeamConfig::load(&path)?))
}

pub(super) fn load_launch_state(
    path: &Path,
) -> Result<Option<HashMap<String, LaunchIdentityRecord>>> {
    load_json_file(path)
}

pub(super) fn load_daemon_state(path: &Path) -> Result<Option<DoctorDaemonState>> {
    load_json_file(path)
}

pub(super) fn load_json_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(parsed))
}

pub(super) fn launch_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("launch-state.json")
}

pub(super) fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

pub(super) fn default_claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
        .join("projects")
}

pub(super) fn claude_session_id_exists(session_id: &str) -> bool {
    let session_file = format!("{session_id}.jsonl");
    let Ok(entries) = fs::read_dir(default_claude_projects_root()) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir() && path.join(&session_file).exists()
    })
}

pub(super) fn display_cleanup_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_json_file_returns_none_for_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.json");

        let parsed: Option<DoctorDaemonState> = load_json_file(&path).unwrap();

        assert_eq!(parsed, None);
    }

    #[test]
    fn load_json_file_reports_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.json");
        fs::write(&path, "{not json").unwrap();

        let error = load_json_file::<DoctorDaemonState>(&path).unwrap_err();

        assert!(error.to_string().contains("failed to parse"));
    }

    #[test]
    fn resolve_task_worktree_handles_relative_and_absolute_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let relative = resolve_task_worktree(tmp.path(), ".batty/worktrees/eng-1");
        let absolute = resolve_task_worktree(tmp.path(), "/tmp/eng-1");

        assert_eq!(
            relative,
            tmp.path().join(".batty").join("worktrees").join("eng-1")
        );
        assert_eq!(absolute, PathBuf::from("/tmp/eng-1"));
    }

    #[test]
    fn branch_and_engineer_name_helpers_match_expected_patterns() {
        assert!(is_task_branch("eng-1/12"));
        assert!(is_task_branch("eng-1/task-12"));
        assert!(!is_task_branch("main"));
        assert!(!is_task_branch("eng-1/feature"));
        assert!(is_engineer_name("eng-1"));
        assert!(!is_engineer_name("manager"));
    }

    #[test]
    fn display_cleanup_path_prefers_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join(".batty").join("worktrees").join("eng-1");

        let display = display_cleanup_path(tmp.path(), &nested);

        assert_eq!(display, ".batty/worktrees/eng-1");
    }
}
