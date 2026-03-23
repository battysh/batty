//! Free helper functions used by the daemon module.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::team::board_cmd;
use crate::team::config::RoleType;
use crate::tmux;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MemberWorktreeContext {
    pub path: PathBuf,
    pub branch: Option<String>,
}

pub(super) fn describe_command_failure(
    command: &str,
    args: &[&str],
    output: &std::process::Output,
) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("process exited with status {}", output.status)
    };

    format!("`{command} {}` failed: {details}", args.join(" "))
}

pub(super) fn default_prompt_file_for_role(role_type: RoleType) -> &'static str {
    match role_type {
        RoleType::Architect => "architect.md",
        RoleType::Manager => "manager.md",
        RoleType::Engineer => "engineer.md",
        RoleType::User => "architect.md",
    }
}

pub(super) fn role_prompt_path(
    team_config_dir: &Path,
    prompt_override: Option<&str>,
    role_type: RoleType,
) -> PathBuf {
    team_config_dir.join(prompt_override.unwrap_or(default_prompt_file_for_role(role_type)))
}

/// Extract the `## Nudge` section from a prompt .md file.
///
/// Returns the text after `## Nudge` up to the next `## ` heading or EOF.
/// Returns `None` if no `## Nudge` section is found.
pub(super) fn extract_nudge_section(prompt_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(prompt_path).ok()?;
    let mut in_nudge = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge {
            // Stop at next heading
            if line.starts_with("## ") {
                break;
            }
            lines.push(line);
        }
    }

    if lines.is_empty() {
        return None;
    }

    let text = lines.join("\n").trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

pub(super) fn format_stuck_duration(stuck_age_secs: u64) -> String {
    if stuck_age_secs >= 3600 {
        let hours = stuck_age_secs / 3600;
        let mins = (stuck_age_secs % 3600) / 60;
        format!("{hours}h {mins}m")
    } else if stuck_age_secs >= 60 {
        let mins = stuck_age_secs / 60;
        let secs = stuck_age_secs % 60;
        format!("{mins}m {secs}s")
    } else {
        format!("{stuck_age_secs}s")
    }
}

pub(super) fn ensure_tmux_session_ready(session: &str) -> Result<()> {
    if tmux::session_exists(session) {
        Ok(())
    } else {
        bail!("daemon startup pre-flight failed: tmux session '{session}' is missing")
    }
}

pub(super) fn ensure_kanban_available() -> Result<()> {
    let output = std::process::Command::new("kanban-md")
        .arg("--help")
        .output()
        .context(
            "daemon startup pre-flight failed while verifying board tooling: could not execute `kanban-md --help`",
        )?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        "unknown error".to_string()
    } else {
        stderr
    };
    bail!("daemon startup pre-flight failed: `kanban-md --help` failed: {detail}");
}

pub(super) fn board_dir(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("board")
}

pub(super) fn ensure_board_initialized(project_root: &Path) -> Result<bool> {
    let board_dir = board_dir(project_root);
    if board_dir.join("tasks").is_dir() {
        return Ok(false);
    }

    board_cmd::init(&board_dir).map_err(|error| {
        anyhow::anyhow!(
            "daemon startup pre-flight failed: unable to initialize board at '{}': {error}",
            board_dir.display()
        )
    })?;
    Ok(true)
}

use anyhow::Context;
