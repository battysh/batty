use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config::{RoleType, TeamConfig};
use super::hierarchy::{self, MemberInstance};
use super::standup::MemberState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchIdentityRecord {
    agent: String,
    prompt: String,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DoctorDaemonState {
    clean_shutdown: bool,
    saved_at: u64,
    states: HashMap<String, MemberState>,
    active_tasks: HashMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeEligibility {
    member: String,
    eligible: bool,
    reason: String,
    stored_prompt_hash: Option<String>,
    current_prompt_hash: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeStatus {
    member: String,
    path: PathBuf,
    branch: Option<String>,
    dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSize {
    name: &'static str,
    bytes: Option<u64>,
}

pub fn build_report(project_root: &Path) -> Result<String> {
    let launch_state = load_launch_state(&launch_state_path(project_root))?;
    let daemon_state = load_daemon_state(&super::daemon_state_path(project_root))?;
    let team_config = load_team_config(project_root)?;
    let members = match &team_config {
        Some(config) => hierarchy::resolve_hierarchy(config)?,
        None => Vec::new(),
    };
    let resume =
        build_resume_eligibility(project_root, team_config.as_ref(), &members, &launch_state);
    let worktrees = build_worktree_statuses(project_root, &members);
    let log_sizes = vec![
        LogSize {
            name: "daemon.log",
            bytes: file_size(&project_root.join(".batty").join("daemon.log")),
        },
        LogSize {
            name: "orchestrator.log",
            bytes: file_size(&project_root.join(".batty").join("orchestrator.log")),
        },
    ];

    Ok(render_report(
        project_root,
        launch_state.as_ref(),
        daemon_state.as_ref(),
        &resume,
        &worktrees,
        &log_sizes,
    ))
}

fn render_report(
    project_root: &Path,
    launch_state: Option<&HashMap<String, LaunchIdentityRecord>>,
    daemon_state: Option<&DoctorDaemonState>,
    resume: &[ResumeEligibility],
    worktrees: &[WorktreeStatus],
    log_sizes: &[LogSize],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Batty doctor for {}\n\n", project_root.display()));

    out.push_str("== Launch State ==\n");
    match launch_state {
        Some(state) if !state.is_empty() => {
            let mut names: Vec<_> = state.keys().cloned().collect();
            names.sort();
            for name in names {
                let identity = &state[&name];
                out.push_str(&format!(
                    "{}: agent={}, prompt_hash={}, session_id={}\n",
                    name,
                    identity.agent,
                    short_prompt_hash(&identity.prompt),
                    identity.session_id.as_deref().unwrap_or("-"),
                ));
            }
        }
        _ => out.push_str("(missing)\n"),
    }
    out.push('\n');

    out.push_str("== Daemon State ==\n");
    match daemon_state {
        Some(state) => {
            out.push_str(&format!("clean_shutdown: {}\n", state.clean_shutdown));
            if state.states.is_empty() {
                out.push_str("member_states: (none)\n");
            } else {
                let mut names: Vec<_> = state.states.keys().cloned().collect();
                names.sort();
                out.push_str("member_states:\n");
                for name in names {
                    out.push_str(&format!("  {}: {:?}\n", name, state.states[&name]));
                }
            }
            if state.active_tasks.is_empty() {
                out.push_str("active_tasks: (none)\n");
            } else {
                let mut names: Vec<_> = state.active_tasks.keys().cloned().collect();
                names.sort();
                out.push_str("active_tasks:\n");
                for name in names {
                    out.push_str(&format!("  {}: #{}\n", name, state.active_tasks[&name]));
                }
            }
        }
        None => out.push_str("(missing)\n"),
    }
    out.push('\n');

    out.push_str("== Resume Eligibility ==\n");
    if resume.is_empty() {
        out.push_str("(no team config or members)\n");
    } else {
        for item in resume {
            out.push_str(&format!(
                "{}: eligible={} reason={} stored_hash={} current_hash={} session_id={}\n",
                item.member,
                item.eligible,
                item.reason,
                item.stored_prompt_hash.as_deref().unwrap_or("-"),
                item.current_prompt_hash.as_deref().unwrap_or("-"),
                item.session_id.as_deref().unwrap_or("-"),
            ));
        }
    }
    out.push('\n');

    out.push_str("== Worktree Status ==\n");
    if worktrees.is_empty() {
        out.push_str("(no engineers)\n");
    } else {
        for status in worktrees {
            let dirty = match status.dirty {
                Some(true) => "dirty",
                Some(false) => "clean",
                None => "missing",
            };
            out.push_str(&format!(
                "{}: path={} branch={} status={}\n",
                status.member,
                status.path.display(),
                status.branch.as_deref().unwrap_or("-"),
                dirty,
            ));
        }
    }
    out.push('\n');

    out.push_str("== Log Sizes ==\n");
    for log in log_sizes {
        match log.bytes {
            Some(bytes) => out.push_str(&format!("{}: {} bytes\n", log.name, bytes)),
            None => out.push_str(&format!("{}: missing\n", log.name)),
        }
    }

    out
}

fn build_resume_eligibility(
    project_root: &Path,
    team_config: Option<&TeamConfig>,
    members: &[MemberInstance],
    launch_state: &Option<HashMap<String, LaunchIdentityRecord>>,
) -> Vec<ResumeEligibility> {
    let Some(launch_state) = launch_state.as_ref() else {
        return members
            .iter()
            .map(|member| ResumeEligibility {
                member: member.name.clone(),
                eligible: false,
                reason: "no_launch_state".to_string(),
                stored_prompt_hash: None,
                current_prompt_hash: None,
                session_id: None,
            })
            .collect();
    };

    let config_dir = super::team_config_dir(project_root);
    members
        .iter()
        .map(|member| {
            let Some(stored) = launch_state.get(&member.name) else {
                return ResumeEligibility {
                    member: member.name.clone(),
                    eligible: false,
                    reason: "missing_member_launch_state".to_string(),
                    stored_prompt_hash: None,
                    current_prompt_hash: team_config
                        .map(|_| short_prompt_hash(&current_prompt(member, &config_dir))),
                    session_id: None,
                };
            };

            let current_prompt = team_config
                .map(|_| current_prompt(member, &config_dir))
                .unwrap_or_default();
            let current_agent = canonical_agent_name(member.agent.as_deref().unwrap_or("claude"));
            let prompt_matches = team_config.is_some() && stored.prompt == current_prompt;
            let agent_matches = stored.agent == current_agent;
            let session_ok = if stored.agent == "claude-code" {
                stored
                    .session_id
                    .as_deref()
                    .is_some_and(claude_session_id_exists)
            } else {
                true
            };
            let eligible = agent_matches && prompt_matches && session_ok;
            let reason = if !agent_matches {
                "agent_changed"
            } else if team_config.is_none() {
                "missing_team_config"
            } else if !prompt_matches {
                "prompt_changed"
            } else if !session_ok {
                "session_missing"
            } else {
                "ok"
            };

            ResumeEligibility {
                member: member.name.clone(),
                eligible,
                reason: reason.to_string(),
                stored_prompt_hash: Some(short_prompt_hash(&stored.prompt)),
                current_prompt_hash: team_config.map(|_| short_prompt_hash(&current_prompt)),
                session_id: stored.session_id.clone(),
            }
        })
        .collect()
}

fn build_worktree_statuses(project_root: &Path, members: &[MemberInstance]) -> Vec<WorktreeStatus> {
    members
        .iter()
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

            let branch = git_output(&path, &["branch", "--show-current"]);
            let dirty = if path.exists() {
                git_output(&path, &["status", "--porcelain"]).map(|output| !output.is_empty())
            } else {
                None
            };

            WorktreeStatus {
                member: member.name.clone(),
                path,
                branch,
                dirty,
            }
        })
        .collect()
}

fn current_prompt(member: &MemberInstance, config_dir: &Path) -> String {
    let prompt_file = member.prompt.as_deref().unwrap_or(match member.role_type {
        RoleType::Architect => "architect.md",
        RoleType::Manager => "manager.md",
        RoleType::Engineer => "engineer.md",
        RoleType::User => "architect.md",
    });

    let path = config_dir.join(prompt_file);
    let content = fs::read_to_string(&path).unwrap_or_else(|_| {
        format!(
            "You are {} (role: {:?}). Work on assigned tasks.",
            member.name, member.role_type
        )
    });

    strip_nudge_section(
        &content
            .replace("{{member_name}}", &member.name)
            .replace("{{role_name}}", &member.role_name)
            .replace(
                "{{reports_to}}",
                member.reports_to.as_deref().unwrap_or("none"),
            ),
    )
}

fn strip_nudge_section(prompt: &str) -> String {
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

fn short_prompt_hash(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn canonical_agent_name(agent_name: &str) -> String {
    match agent_name {
        "claude" | "claude-code" => "claude-code".to_string(),
        "codex" | "codex-cli" => "codex-cli".to_string(),
        _ => agent_name.to_string(),
    }
}

fn git_output(path: &Path, args: &[&str]) -> Option<String> {
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

fn load_team_config(project_root: &Path) -> Result<Option<TeamConfig>> {
    let path = super::team_config_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(TeamConfig::load(&path)?))
}

fn load_launch_state(path: &Path) -> Result<Option<HashMap<String, LaunchIdentityRecord>>> {
    load_json_file(path)
}

fn load_daemon_state(path: &Path) -> Result<Option<DoctorDaemonState>> {
    load_json_file(path)
}

fn load_json_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(parsed))
}

fn launch_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("launch-state.json")
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn default_claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
        .join("projects")
}

fn claude_session_id_exists(session_id: &str) -> bool {
    let session_file = format!("{session_id}.jsonl");
    let Ok(entries) = fs::read_dir(default_claude_projects_root()) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir() && path.join(&session_file).exists()
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn write_team_config(root: &Path) {
        let team_dir = root.join(".batty").join("team_config");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("team.yaml"),
            r#"
name: test
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
"#,
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

    #[test]
    fn test_doctor_parses_launch_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("launch-state.json");
        fs::write(
            &path,
            r#"{"manager":{"agent":"codex-cli","prompt":"Manager prompt","session_id":null}}"#,
        )
        .unwrap();

        let parsed = load_launch_state(&path).unwrap().unwrap();
        assert_eq!(parsed["manager"].agent, "codex-cli");
        assert_eq!(parsed["manager"].prompt, "Manager prompt");
        assert_eq!(parsed["manager"].session_id, None);
    }

    #[test]
    fn test_doctor_parses_daemon_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon-state.json");
        fs::write(
            &path,
            r#"{"clean_shutdown":true,"saved_at":10,"states":{"manager":"idle"},"active_tasks":{"eng-1":42}}"#,
        )
        .unwrap();

        let parsed = load_daemon_state(&path).unwrap().unwrap();
        assert!(parsed.clean_shutdown);
        assert_eq!(parsed.states["manager"], MemberState::Idle);
        assert_eq!(parsed.active_tasks["eng-1"], 42);
    }

    #[test]
    fn test_doctor_formats_output() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("engineer")).unwrap();
        let launch_state = HashMap::from([
            (
                "architect".to_string(),
                LaunchIdentityRecord {
                    agent: "claude-code".to_string(),
                    prompt: strip_nudge_section(
                        "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
                    ),
                    session_id: Some("missing".to_string()),
                },
            ),
            (
                "manager".to_string(),
                LaunchIdentityRecord {
                    agent: "codex-cli".to_string(),
                    prompt: "Manager prompt".to_string(),
                    session_id: None,
                },
            ),
            (
                "engineer".to_string(),
                LaunchIdentityRecord {
                    agent: "codex-cli".to_string(),
                    prompt: "Engineer prompt".to_string(),
                    session_id: None,
                },
            ),
        ]);
        fs::write(
            launch_state_path(tmp.path()),
            serde_json::to_string(&launch_state).unwrap(),
        )
        .unwrap();
        fs::write(
            super::super::daemon_state_path(tmp.path()),
            r#"{"clean_shutdown":false,"saved_at":10,"states":{"architect":"working","manager":"idle"},"active_tasks":{"engineer":58}}"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(tmp.path().join(".batty").join("daemon.log"), "daemon").unwrap();
        fs::write(
            tmp.path().join(".batty").join("orchestrator.log"),
            "orchestrator",
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Launch State =="));
        assert!(report.contains("== Daemon State =="));
        assert!(report.contains("== Resume Eligibility =="));
        assert!(report.contains("== Worktree Status =="));
        assert!(report.contains("== Log Sizes =="));
        assert!(report.contains("manager: agent=codex-cli"));
        assert!(report.contains("clean_shutdown: false"));
        assert!(report.contains("path="));
        assert!(report.contains("status=missing"));
        assert!(report.contains("daemon.log: 6 bytes"));
    }

    #[test]
    fn test_doctor_handles_missing_files() {
        let tmp = tempfile::tempdir().unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Launch State =="));
        assert!(report.contains("(missing)"));
        assert!(report.contains("== Daemon State =="));
        assert!(report.contains("== Resume Eligibility =="));
        assert!(report.contains("(no team config or members)"));
        assert!(report.contains("daemon.log: missing"));
        assert!(report.contains("orchestrator.log: missing"));
    }
}
