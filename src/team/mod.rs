//! Team mode — hierarchical agent org chart with daemon-managed communication.
//!
//! A YAML-defined team (architect ↔ manager ↔ N engineers) runs in a tmux
//! session. The daemon monitors panes, routes messages between roles, and
//! manages agent lifecycles.

pub mod board;
pub mod comms;
pub mod config;
pub mod daemon;
pub mod events;
pub mod hierarchy;
pub mod inbox;
pub mod layout;
pub mod message;
pub mod standup;
pub mod task_loop;
pub mod telegram;
pub mod watcher;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::tmux;

/// Team config directory name inside `.batty/`.
pub const TEAM_CONFIG_DIR: &str = "team_config";
/// Team config filename.
pub const TEAM_CONFIG_FILE: &str = "team.yaml";

/// Resolve the team config directory for a project root.
pub fn team_config_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join(TEAM_CONFIG_DIR)
}

/// Resolve the path to team.yaml.
pub fn team_config_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join(TEAM_CONFIG_FILE)
}

/// Scaffold `.batty/team_config/` with default team.yaml and prompt templates.
pub fn init_team(project_root: &Path, template: &str) -> Result<Vec<PathBuf>> {
    let config_dir = team_config_dir(project_root);
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;

    let mut created = Vec::new();

    let yaml_path = config_dir.join(TEAM_CONFIG_FILE);
    if yaml_path.exists() {
        bail!(
            "team config already exists at {}; remove it first or edit directly",
            yaml_path.display()
        );
    }

    let yaml_content = match template {
        "solo" => include_str!("templates/team_solo.yaml"),
        "pair" => include_str!("templates/team_pair.yaml"),
        "squad" => include_str!("templates/team_squad.yaml"),
        "large" => include_str!("templates/team_large.yaml"),
        "research" => include_str!("templates/team_research.yaml"),
        "software" => include_str!("templates/team_software.yaml"),
        "batty" => include_str!("templates/team_batty.yaml"),
        _ => include_str!("templates/team_simple.yaml"),
    };
    std::fs::write(&yaml_path, yaml_content)
        .with_context(|| format!("failed to write {}", yaml_path.display()))?;
    created.push(yaml_path);

    // Install prompt .md files matching the template's roles
    let prompt_files: &[(&str, &str)] = match template {
        "research" => &[
            (
                "research_lead.md",
                include_str!("templates/research_lead.md"),
            ),
            ("sub_lead.md", include_str!("templates/sub_lead.md")),
            ("researcher.md", include_str!("templates/researcher.md")),
        ],
        "software" => &[
            ("tech_lead.md", include_str!("templates/tech_lead.md")),
            ("eng_manager.md", include_str!("templates/eng_manager.md")),
            ("developer.md", include_str!("templates/developer.md")),
        ],
        "batty" => &[
            (
                "batty_architect.md",
                include_str!("templates/batty_architect.md"),
            ),
            (
                "batty_manager.md",
                include_str!("templates/batty_manager.md"),
            ),
            (
                "batty_engineer.md",
                include_str!("templates/batty_engineer.md"),
            ),
        ],
        _ => &[
            ("architect.md", include_str!("templates/architect.md")),
            ("manager.md", include_str!("templates/manager.md")),
            ("engineer.md", include_str!("templates/engineer.md")),
        ],
    };

    for (name, content) in prompt_files {
        let path = config_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
            created.push(path);
        }
    }

    // Initialize kanban-md board in the team config directory
    let board_dir = config_dir.join("board");
    if !board_dir.exists() {
        let output = std::process::Command::new("kanban-md")
            .args(["init", "--dir", &board_dir.to_string_lossy()])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                created.push(board_dir);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!("kanban-md init failed: {stderr}; falling back to plain kanban.md");
                let kanban_path = config_dir.join("kanban.md");
                std::fs::write(
                    &kanban_path,
                    "# Kanban Board\n\n## Backlog\n\n## In Progress\n\n## Done\n",
                )?;
                created.push(kanban_path);
            }
            Err(_) => {
                warn!("kanban-md not found; falling back to plain kanban.md");
                let kanban_path = config_dir.join("kanban.md");
                std::fs::write(
                    &kanban_path,
                    "# Kanban Board\n\n## Backlog\n\n## In Progress\n\n## Done\n",
                )?;
                created.push(kanban_path);
            }
        }
    }

    info!(dir = %config_dir.display(), files = created.len(), "scaffolded team config");
    Ok(created)
}

/// Path to the daemon PID file.
fn daemon_pid_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("daemon.pid")
}

/// Path to the daemon log file.
fn daemon_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("daemon.log")
}

/// Spawn the daemon as a detached background process.
///
/// The daemon runs in its own process group with stdio redirected to a log
/// file, so it survives terminal closure. PID is saved to `.batty/daemon.pid`.
fn spawn_daemon(project_root: &Path, resume: bool) -> Result<u32> {
    use std::fs::File;
    use std::process::{Command, Stdio};

    let log_path = daemon_log_path(project_root);
    let pid_path = daemon_pid_path(project_root);

    // Ensure .batty/ exists
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let log_file = File::create(&log_path)
        .with_context(|| format!("failed to create daemon log: {}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .context("failed to clone log file handle")?;

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let root_str = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();

    let mut cmd = Command::new(exe);
    let mut args = vec!["daemon", "--project-root", &root_str];
    if resume {
        args.push("--resume");
    }
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err);

    // Detach into a new process group so it survives terminal closure
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().context("failed to spawn daemon process")?;
    let pid = child.id();

    std::fs::write(&pid_path, pid.to_string())
        .with_context(|| format!("failed to write PID file: {}", pid_path.display()))?;

    info!(pid, log = %log_path.display(), "daemon spawned");
    Ok(pid)
}

/// Kill the daemon process if it's running.
fn kill_daemon(project_root: &Path) {
    let pid_path = daemon_pid_path(project_root);
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            #[cfg(unix)]
            {
                // Send SIGTERM to the daemon process
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                info!(pid, "sent SIGTERM to daemon");
            }
            #[cfg(not(unix))]
            {
                warn!(pid, "cannot kill daemon on this platform");
            }
        }
        let _ = std::fs::remove_file(&pid_path);
    }
}

/// Start a team session: load config, resolve hierarchy, create tmux layout,
/// spawn the daemon as a background process, and optionally attach.
///
/// Returns the tmux session name.
pub fn start_team(project_root: &Path, attach: bool) -> Result<String> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!(
            "no team config found at {}; run `batty init` first",
            config_path.display()
        );
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    team_config.validate()?;

    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let session = format!("batty-{}", team_config.name);

    if tmux::session_exists(&session) {
        bail!("session '{session}' already exists; use `batty attach` or `batty stop` first");
    }

    layout::build_layout(&session, &members, &team_config.layout, project_root)?;

    // Initialize Maildir inboxes for all members
    let inboxes = inbox::inboxes_root(project_root);
    for member in &members {
        inbox::init_inbox(&inboxes, &member.name)?;
    }

    // Check for resume marker (left by a prior `batty stop`)
    let marker = resume_marker_path(project_root);
    let resume = marker.exists();
    if resume {
        // Consume the marker — it's a one-shot flag
        std::fs::remove_file(&marker).ok();
        info!("resuming agent sessions from previous run");
    }

    info!(session = %session, members = members.len(), resume, "team session started");

    // Spawn daemon as a detached background process
    let pid = spawn_daemon(project_root, resume)?;
    info!(pid, "daemon process launched");

    // Give daemon a moment to start spawning agents
    std::thread::sleep(std::time::Duration::from_secs(2));

    if attach {
        tmux::attach(&session)?;
    }

    Ok(session)
}

/// Run the daemon loop directly (called by the hidden `batty daemon` subcommand).
///
/// This is the entry point for the daemonized background process.
pub fn run_daemon(project_root: &Path, resume: bool) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!(
            "no team config found at {}; run `batty init` first",
            config_path.display()
        );
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let session = format!("batty-{}", team_config.name);

    // Wait for tmux session to be ready (start_team creates it before spawning us)
    for _ in 0..30 {
        if tmux::session_exists(&session) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    if !tmux::session_exists(&session) {
        bail!("tmux session '{session}' not found — did `batty start` create it?");
    }

    // Reconstruct pane_map from tmux pane options
    let mut pane_map = std::collections::HashMap::new();
    for member in &members {
        // Query tmux for the pane ID tagged with this member's role
        if let Some(pane_id) = find_pane_for_member(&session, &member.name) {
            pane_map.insert(member.name.clone(), pane_id);
        }
    }

    let daemon_config = daemon::DaemonConfig {
        project_root: project_root.to_path_buf(),
        team_config,
        session,
        members,
        pane_map,
    };

    let mut d = daemon::TeamDaemon::new(daemon_config)?;
    d.run(resume)
}

/// Find the tmux pane ID tagged with `@batty_role=<member_name>` in a session.
fn find_pane_for_member(session: &str, member_name: &str) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_id} #{@batty_role}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 && parts[1] == member_name {
            return Some(parts[0].to_string());
        }
    }
    None
}

/// Path to the resume marker file. Presence indicates agents have prior sessions.
fn resume_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("resume")
}

/// Stop a running team session and clean up any orphaned `batty-` sessions.
pub fn stop_team(project_root: &Path) -> Result<()> {
    // Write resume marker before tearing down — agents have sessions to continue
    let marker = resume_marker_path(project_root);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&marker, "").ok();

    // Kill the daemon process first
    kill_daemon(project_root);

    let config_path = team_config_path(project_root);
    let primary_session = if config_path.exists() {
        let team_config = config::TeamConfig::load(&config_path)?;
        Some(format!("batty-{}", team_config.name))
    } else {
        None
    };

    // Kill only the session belonging to this project
    match &primary_session {
        Some(session) if tmux::session_exists(session) => {
            tmux::kill_session(session)?;
            info!(session = %session, "team session stopped");
        }
        Some(session) => {
            info!(session = %session, "no running session to stop");
        }
        None => {
            bail!("no team config found at {}", config_path.display());
        }
    }

    Ok(())
}

/// Attach to a running team session.
///
/// First tries the team config in the project root. If not found, looks for
/// any running `batty-*` tmux session and attaches to it.
pub fn attach_team(project_root: &Path) -> Result<()> {
    let config_path = team_config_path(project_root);

    let session = if config_path.exists() {
        let team_config = config::TeamConfig::load(&config_path)?;
        format!("batty-{}", team_config.name)
    } else {
        // No local config — find any running batty session
        let sessions = tmux::list_sessions_with_prefix("batty-");
        match sessions.len() {
            0 => bail!("no team config found and no batty sessions running"),
            1 => sessions.into_iter().next().unwrap(),
            _ => {
                let list = sessions.join(", ");
                bail!(
                    "no team config found and multiple batty sessions running: {list}\n\
                     Run from the project directory, or use: tmux attach -t <session>"
                );
            }
        }
    };

    if !tmux::session_exists(&session) {
        bail!("no running session '{session}'; run `batty start` first");
    }

    tmux::attach(&session)
}

/// Show team status.
pub fn team_status(project_root: &Path, json: bool) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let session = format!("batty-{}", team_config.name);
    let session_running = tmux::session_exists(&session);

    if json {
        let status = serde_json::json!({
            "team": team_config.name,
            "session": session,
            "running": session_running,
            "members": members.iter().map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "role": m.role_name,
                    "role_type": format!("{:?}", m.role_type),
                    "agent": m.agent,
                    "reports_to": m.reports_to,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("Team: {}", team_config.name);
        println!(
            "Session: {} ({})",
            session,
            if session_running {
                "running"
            } else {
                "stopped"
            }
        );
        println!();
        println!(
            "{:<20} {:<12} {:<10} {:<20}",
            "MEMBER", "ROLE", "AGENT", "REPORTS TO"
        );
        println!("{}", "-".repeat(62));
        for m in &members {
            println!(
                "{:<20} {:<12} {:<10} {:<20}",
                m.name,
                m.role_name,
                m.agent.as_deref().unwrap_or("-"),
                m.reports_to.as_deref().unwrap_or("-"),
            );
        }
    }

    Ok(())
}

/// Validate team config without launching.
pub fn validate_team(project_root: &Path) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    team_config.validate()?;

    let members = hierarchy::resolve_hierarchy(&team_config)?;

    println!("Config: {}", config_path.display());
    println!("Team: {}", team_config.name);
    println!("Roles: {}", team_config.roles.len());
    println!("Total members: {}", members.len());
    println!("Valid.");
    Ok(())
}

/// Resolve a member instance name (e.g. "eng-1-2") to its role definition name
/// (e.g. "engineer"). Returns the name itself if no config is available.
fn resolve_role_name(project_root: &Path, member_name: &str) -> String {
    // "human" is not a member instance — it's the CLI user
    if member_name == "human" {
        return "human".to_string();
    }
    let config_path = team_config_path(project_root);
    if let Ok(team_config) = config::TeamConfig::load(&config_path) {
        if let Ok(members) = hierarchy::resolve_hierarchy(&team_config) {
            if let Some(m) = members.iter().find(|m| m.name == member_name) {
                return m.role_name.clone();
            }
        }
    }
    // Fallback: the name might already be a role name
    member_name.to_string()
}

/// Send a message to a role via their Maildir inbox.
///
/// The sender is auto-detected from the `@batty_role` tmux pane option
/// (set during layout). Falls back to "human" if not in a batty pane.
/// Enforces communication routing rules from team config.
pub fn send_message(project_root: &Path, role: &str, msg: &str) -> Result<()> {
    let from = detect_sender().unwrap_or_else(|| "human".to_string());

    // Enforce routing: check talks_to rules
    let config_path = team_config_path(project_root);
    if config_path.exists() {
        if let Ok(team_config) = config::TeamConfig::load(&config_path) {
            let from_role = resolve_role_name(project_root, &from);
            let to_role = resolve_role_name(project_root, role);
            if !team_config.can_talk(&from_role, &to_role) {
                bail!(
                    "{from} ({from_role}) is not allowed to message {role} ({to_role}). \
                     Check talks_to in team.yaml."
                );
            }
        }
    }

    let root = inbox::inboxes_root(project_root);
    let inbox_msg = inbox::InboxMessage::new_send(&from, role, msg);
    let id = inbox::deliver_to_inbox(&root, &inbox_msg)?;
    info!(to = role, id = %id, "message delivered to inbox");
    Ok(())
}

/// Detect who is calling `batty send` by reading the `@batty_role` option
/// from the current tmux pane.
fn detect_sender() -> Option<String> {
    let pane_id = std::env::var("TMUX_PANE").ok()?;
    let output = std::process::Command::new("tmux")
        .args(["show-options", "-p", "-t", &pane_id, "-v", "@batty_role"])
        .output()
        .ok()?;
    if output.status.success() {
        let role = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !role.is_empty() { Some(role) } else { None }
    } else {
        None
    }
}

/// Assign a task to an engineer via their Maildir inbox.
pub fn assign_task(project_root: &Path, engineer: &str, task: &str) -> Result<()> {
    let from = detect_sender().unwrap_or_else(|| "human".to_string());

    let config_path = team_config_path(project_root);
    if config_path.exists() {
        if let Ok(team_config) = config::TeamConfig::load(&config_path) {
            let from_role = resolve_role_name(project_root, &from);
            let to_role = resolve_role_name(project_root, engineer);
            if !team_config.can_talk(&from_role, &to_role) {
                bail!(
                    "{from} ({from_role}) is not allowed to assign {engineer} ({to_role}). \
                     Check talks_to in team.yaml."
                );
            }
        }
    }

    let root = inbox::inboxes_root(project_root);
    let msg = inbox::InboxMessage::new_assign(&from, engineer, task);
    let id = inbox::deliver_to_inbox(&root, &msg)?;
    info!(from, engineer, task, id = %id, "assignment delivered to inbox");
    Ok(())
}

/// List inbox messages for a member.
pub fn list_inbox(project_root: &Path, member: &str) -> Result<()> {
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, member)?;

    if messages.is_empty() {
        println!("No messages for {member}.");
        return Ok(());
    }

    println!(
        "{:<8} {:<12} {:<12} {:<8} BODY",
        "STATUS", "FROM", "TYPE", "ID"
    );
    println!("{}", "-".repeat(72));
    for (msg, delivered) in &messages {
        let status = if *delivered { "delivered" } else { "pending" };
        let id_short = if msg.id.len() > 8 {
            &msg.id[..8]
        } else {
            &msg.id
        };
        let body_short = if msg.body.len() > 40 {
            format!("{}...", &msg.body[..40])
        } else {
            msg.body.clone()
        };
        println!(
            "{:<8} {:<12} {:<12} {:<8} {}",
            status,
            msg.from,
            format!("{:?}", msg.msg_type).to_lowercase(),
            id_short,
            body_short,
        );
    }

    Ok(())
}

/// Read a specific message from a member's inbox by ID or prefix.
pub fn read_message(project_root: &Path, member: &str, id: &str) -> Result<()> {
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, member)?;

    // Find message by exact ID or prefix match
    let matching: Vec<_> = messages
        .iter()
        .filter(|(msg, _)| msg.id == id || msg.id.starts_with(id))
        .collect();

    match matching.len() {
        0 => bail!("no message matching '{id}' in {member}'s inbox"),
        1 => {
            let (msg, delivered) = matching[0];
            let status = if *delivered { "delivered" } else { "pending" };
            println!("ID:     {}", msg.id);
            println!("From:   {}", msg.from);
            println!("To:     {}", msg.to);
            println!("Type:   {:?}", msg.msg_type);
            println!("Status: {status}");
            println!("Time:   {}", msg.timestamp);
            println!();
            println!("{}", msg.body);
        }
        n => {
            bail!("'{id}' matches {n} messages — use a longer prefix");
        }
    }

    Ok(())
}

/// Acknowledge (mark delivered) a message in a member's inbox.
pub fn ack_message(project_root: &Path, member: &str, id: &str) -> Result<()> {
    let root = inbox::inboxes_root(project_root);
    inbox::mark_delivered(&root, member, id)?;
    info!(member, id, "message acknowledged");
    Ok(())
}

/// Merge an engineer's worktree branch.
pub fn merge_worktree(project_root: &Path, engineer: &str) -> Result<()> {
    match daemon::merge_engineer_branch(project_root, engineer)? {
        task_loop::MergeOutcome::Success => Ok(()),
        task_loop::MergeOutcome::RebaseConflict(stderr) => {
            bail!("merge blocked by rebase conflict: {stderr}")
        }
    }
}

/// Run the interactive Telegram setup wizard.
pub fn setup_telegram(project_root: &Path) -> Result<()> {
    telegram::setup_telegram(project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_config_dir_is_under_batty() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            team_config_dir(root),
            PathBuf::from("/tmp/project/.batty/team_config")
        );
    }

    #[test]
    fn team_config_path_points_to_yaml() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            team_config_path(root),
            PathBuf::from("/tmp/project/.batty/team_config/team.yaml")
        );
    }

    #[test]
    fn init_team_creates_scaffolding() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "simple").unwrap();
        assert!(!created.is_empty());
        assert!(team_config_path(tmp.path()).exists());
        assert!(team_config_dir(tmp.path()).join("architect.md").exists());
        assert!(team_config_dir(tmp.path()).join("manager.md").exists());
        assert!(team_config_dir(tmp.path()).join("engineer.md").exists());
        // kanban-md creates board/ directory; fallback creates kanban.md
        let config = team_config_dir(tmp.path());
        assert!(config.join("board").is_dir() || config.join("kanban.md").exists());
    }

    #[test]
    fn init_team_refuses_if_exists() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "simple").unwrap();
        let result = init_team(tmp.path(), "simple");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn init_team_large_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "large").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("instances: 3") || content.contains("instances: 5"));
    }

    #[test]
    fn init_team_solo_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "solo").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("role_type: engineer"));
        assert!(!content.contains("role_type: manager"));
    }

    #[test]
    fn init_team_pair_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "pair").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("role_type: architect"));
        assert!(content.contains("role_type: engineer"));
        assert!(!content.contains("role_type: manager"));
    }

    #[test]
    fn init_team_squad_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "squad").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("instances: 5"));
        assert!(content.contains("layout:"));
    }

    #[test]
    fn init_team_research_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "research").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("principal"));
        assert!(content.contains("sub-lead"));
        assert!(content.contains("researcher"));
        // Research-specific .md files installed
        assert!(
            team_config_dir(tmp.path())
                .join("research_lead.md")
                .exists()
        );
        assert!(team_config_dir(tmp.path()).join("sub_lead.md").exists());
        assert!(team_config_dir(tmp.path()).join("researcher.md").exists());
        // Generic files NOT installed
        assert!(!team_config_dir(tmp.path()).join("architect.md").exists());
    }

    #[test]
    fn init_team_software_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "software").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("tech-lead"));
        assert!(content.contains("backend-mgr"));
        assert!(content.contains("frontend-mgr"));
        assert!(content.contains("developer"));
        // Software-specific .md files installed
        assert!(team_config_dir(tmp.path()).join("tech_lead.md").exists());
        assert!(team_config_dir(tmp.path()).join("eng_manager.md").exists());
        assert!(team_config_dir(tmp.path()).join("developer.md").exists());
    }

    #[test]
    fn init_team_batty_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "batty").unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("batty-dev"));
        assert!(content.contains("role_type: architect"));
        assert!(content.contains("role_type: manager"));
        assert!(content.contains("instances: 4"));
        assert!(content.contains("batty_architect.md"));
        // Batty-specific .md files installed
        assert!(
            team_config_dir(tmp.path())
                .join("batty_architect.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("batty_manager.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("batty_engineer.md")
                .exists()
        );
    }

    #[test]
    fn send_message_delivers_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        send_message(tmp.path(), "architect", "hello").unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        // detect_sender() returns the tmux pane role if running inside a batty
        // session, or "human" otherwise. Accept either.
        let expected_from = detect_sender().unwrap_or_else(|| "human".to_string());
        assert_eq!(pending[0].from, expected_from);
        assert_eq!(pending[0].to, "architect");
        assert_eq!(pending[0].body, "hello");
    }

    #[test]
    fn assign_task_delivers_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        assign_task(tmp.path(), "eng-1-1", "fix bug").unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let pending = inbox::pending_messages(&root, "eng-1-1").unwrap();
        assert_eq!(pending.len(), 1);
        let expected_from = detect_sender().unwrap_or_else(|| "human".to_string());
        assert_eq!(pending[0].from, expected_from);
        assert_eq!(pending[0].to, "eng-1-1");
        assert_eq!(pending[0].body, "fix bug");
        assert_eq!(pending[0].msg_type, inbox::MessageType::Assign);
    }
}
