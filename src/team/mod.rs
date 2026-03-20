//! Team mode — hierarchical agent org chart with daemon-managed communication.
//!
//! A YAML-defined team (architect ↔ manager ↔ N engineers) runs in a tmux
//! session. The daemon monitors panes, routes messages between roles, and
//! manages agent lifecycles.

pub mod artifact;
pub mod board;
pub mod capability;
pub mod comms;
pub mod completion;
pub mod config;
pub mod daemon;
pub mod events;
pub mod hierarchy;
pub mod inbox;
pub mod layout;
pub mod message;
pub mod metrics;
pub mod nudge;
pub mod policy;
pub mod resolver;
pub mod review;
pub mod standup;
pub mod task_cmd;
pub mod task_loop;
pub mod telegram;
pub mod validation;
pub mod watcher;
pub mod workflow;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::tmux;

/// Team config directory name inside `.batty/`.
pub const TEAM_CONFIG_DIR: &str = "team_config";
/// Team config filename.
pub const TEAM_CONFIG_FILE: &str = "team.yaml";

/// Default duration window for load graph rendering, in seconds (1 hour).
const LOAD_GRAPH_WINDOW_SECONDS: u64 = 3_600;
const LOAD_GRAPH_WIDTH: usize = 30;
const INBOX_BODY_PREVIEW_CHARS: usize = 140;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentResultStatus {
    Delivered,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssignmentDeliveryResult {
    pub message_id: String,
    pub status: AssignmentResultStatus,
    pub engineer: String,
    pub task_summary: String,
    pub branch: Option<String>,
    pub work_dir: Option<String>,
    pub detail: String,
    pub ts: u64,
}

/// Resolve the team config directory for a project root.
pub fn team_config_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join(TEAM_CONFIG_DIR)
}

/// Resolve the path to team.yaml.
pub fn team_config_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join(TEAM_CONFIG_FILE)
}

pub fn team_events_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join("events.jsonl")
}

pub(crate) fn orchestrator_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("orchestrator.log")
}

#[derive(Debug, Clone, Copy)]
pub struct TeamLoadSnapshot {
    pub timestamp: u64,
    pub total_members: usize,
    pub working_members: usize,
    pub load: f64,
    pub session_running: bool,
}

fn assignment_results_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("assignment_results")
}

fn assignment_result_path(project_root: &Path, message_id: &str) -> PathBuf {
    assignment_results_dir(project_root).join(format!("{message_id}.json"))
}

pub(crate) fn store_assignment_result(
    project_root: &Path,
    result: &AssignmentDeliveryResult,
) -> Result<()> {
    let path = assignment_result_path(project_root, &result.message_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec_pretty(result)?;
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write assignment result {}", path.display()))?;
    Ok(())
}

pub fn load_assignment_result(
    project_root: &Path,
    message_id: &str,
) -> Result<Option<AssignmentDeliveryResult>> {
    let path = assignment_result_path(project_root, message_id);
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read(&path)
        .with_context(|| format!("failed to read assignment result {}", path.display()))?;
    let result = serde_json::from_slice(&data)
        .with_context(|| format!("failed to parse assignment result {}", path.display()))?;
    Ok(Some(result))
}

pub fn wait_for_assignment_result(
    project_root: &Path,
    message_id: &str,
    timeout: Duration,
) -> Result<Option<AssignmentDeliveryResult>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(result) = load_assignment_result(project_root, message_id)? {
            return Ok(Some(result));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn format_assignment_result(result: &AssignmentDeliveryResult) -> String {
    let mut text = match result.status {
        AssignmentResultStatus::Delivered => {
            format!(
                "Assignment delivered: {} -> {}",
                result.message_id, result.engineer
            )
        }
        AssignmentResultStatus::Failed => {
            format!(
                "Assignment failed: {} -> {}",
                result.message_id, result.engineer
            )
        }
    };

    text.push_str(&format!("\nTask: {}", result.task_summary));
    if let Some(branch) = result.branch.as_deref() {
        text.push_str(&format!("\nBranch: {branch}"));
    }
    if let Some(work_dir) = result.work_dir.as_deref() {
        text.push_str(&format!("\nWorktree: {work_dir}"));
    }
    if !result.detail.is_empty() {
        text.push_str(&format!("\nDetail: {}", result.detail));
    }
    text
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

pub(crate) fn daemon_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("daemon-state.json")
}

fn workflow_mode_declared(config_path: &Path) -> Result<bool> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let value: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(false);
    };

    Ok(mapping.contains_key(serde_yaml::Value::String("workflow_mode".to_string())))
}

fn migration_validation_notes(
    team_config: &config::TeamConfig,
    workflow_mode_is_explicit: bool,
) -> Vec<String> {
    if !workflow_mode_is_explicit {
        return vec![
            "Migration: workflow_mode omitted; defaulting to legacy so existing teams and boards run unchanged.".to_string(),
        ];
    }

    match team_config.workflow_mode {
        config::WorkflowMode::Legacy => vec![
            "Migration: legacy mode selected; Batty keeps current runtime behavior and treats workflow metadata as optional.".to_string(),
        ],
        config::WorkflowMode::Hybrid => vec![
            "Migration: hybrid mode selected; workflow adoption is incremental and legacy runtime behavior remains available.".to_string(),
        ],
        config::WorkflowMode::WorkflowFirst => vec![
            "Migration: workflow_first mode selected; complete board metadata and orchestrator rollout before treating workflow state as primary truth.".to_string(),
        ],
    }
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

    layout::build_layout(
        &session,
        &members,
        &team_config.layout,
        project_root,
        team_config.workflow_mode,
        team_config.orchestrator_enabled(),
    )?;

    // Initialize Maildir inboxes for all members
    let inboxes = inbox::inboxes_root(project_root);
    for member in &members {
        inbox::init_inbox(&inboxes, &member.name)?;
    }

    // Check for resume marker (left by a prior `batty stop`)
    let marker = resume_marker_path(project_root);
    let resume = marker.exists() || should_resume_from_daemon_state(project_root);
    if resume {
        if marker.exists() {
            // Consume the marker — it's a one-shot flag
            std::fs::remove_file(&marker).ok();
        }
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

    let events_path = project_root
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");

    let mut d = daemon::TeamDaemon::new(daemon_config)?;

    // Wrap in catch_unwind so panics are logged to events before exit
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.run(resume)));

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            eprintln!("daemon exited with error: {e:#}");
            // Try to log the error event
            if let Ok(mut sink) = events::EventSink::new(&events_path) {
                let _ = sink.emit(events::TeamEvent::daemon_stopped_with_reason(
                    &format!("error: {e:#}"),
                    0,
                ));
            }
            Err(e)
        }
        Err(panic_payload) => {
            let reason = match panic_payload.downcast_ref::<&str>() {
                Some(s) => s.to_string(),
                None => match panic_payload.downcast_ref::<String>() {
                    Some(s) => s.clone(),
                    None => "unknown panic".to_string(),
                },
            };
            eprintln!("daemon panicked: {reason}");
            // Log panic event
            if let Ok(mut sink) = events::EventSink::new(&events_path) {
                let _ = sink.emit(events::TeamEvent::daemon_panic(&reason));
            }
            std::panic::resume_unwind(panic_payload);
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeMemberStatus {
    state: String,
    signal: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TeamStatusRow {
    name: String,
    role: String,
    role_type: String,
    agent: Option<String>,
    reports_to: Option<String>,
    state: String,
    pending_inbox: usize,
    triage_backlog: usize,
    active_owned_tasks: Vec<u32>,
    review_owned_tasks: Vec<u32>,
    signal: Option<String>,
    runtime_label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TriageBacklogState {
    count: usize,
    newest_result_ts: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OwnedTaskBuckets {
    active: Vec<u32>,
    review: Vec<u32>,
}

fn list_runtime_member_statuses(
    session: &str,
) -> Result<std::collections::HashMap<String, RuntimeMemberStatus>> {
    let output = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_id}\t#{@batty_role}\t#{@batty_status}\t#{pane_dead}",
        ])
        .output()
        .with_context(|| format!("failed to list panes for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-panes runtime status failed: {stderr}");
    }

    let mut statuses = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(4, '\t');
        let Some(_pane_id) = parts.next() else {
            continue;
        };
        let Some(member_name) = parts.next() else {
            continue;
        };
        let Some(raw_status) = parts.next() else {
            continue;
        };
        let Some(pane_dead) = parts.next() else {
            continue;
        };
        if member_name.trim().is_empty() {
            continue;
        }

        statuses.insert(
            member_name.to_string(),
            summarize_runtime_member_status(raw_status, pane_dead == "1"),
        );
    }

    Ok(statuses)
}

fn summarize_runtime_member_status(raw_status: &str, pane_dead: bool) -> RuntimeMemberStatus {
    if pane_dead {
        return RuntimeMemberStatus {
            state: "crashed".to_string(),
            signal: None,
            label: Some("crashed".to_string()),
        };
    }

    let label = strip_tmux_style(raw_status);
    let normalized = label.to_ascii_lowercase();
    let has_paused_nudge = normalized.contains("nudge paused");
    let has_nudge_sent = normalized.contains("nudge sent");
    let has_waiting_nudge = normalized.contains("nudge") && !has_nudge_sent && !has_paused_nudge;
    let has_paused_standup = normalized.contains("standup paused");
    let has_standup = normalized.contains("standup") && !has_paused_standup;

    let state = if normalized.contains("crashed") {
        "crashed"
    } else if normalized.contains("working") {
        "working"
    } else if normalized.contains("done") || normalized.contains("completed") {
        "done"
    } else if normalized.contains("idle") {
        "idle"
    } else if label.is_empty() {
        "starting"
    } else {
        "unknown"
    };

    let mut signals = Vec::new();
    if has_paused_nudge {
        signals.push("nudge paused");
    } else if has_nudge_sent {
        signals.push("nudged");
    } else if has_waiting_nudge {
        signals.push("waiting for nudge");
    }
    if has_paused_standup {
        signals.push("standup paused");
    } else if has_standup {
        signals.push("standup");
    }
    let signal = (!signals.is_empty()).then(|| signals.join(", "));

    RuntimeMemberStatus {
        state: state.to_string(),
        signal,
        label: (!label.is_empty()).then_some(label),
    }
}

fn strip_tmux_style(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '#' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next == ']' {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }

    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_team_status_rows(
    members: &[hierarchy::MemberInstance],
    session_running: bool,
    runtime_statuses: &std::collections::HashMap<String, RuntimeMemberStatus>,
    pending_inbox_counts: &std::collections::HashMap<String, usize>,
    triage_backlog_counts: &std::collections::HashMap<String, usize>,
    owned_task_buckets: &std::collections::HashMap<String, OwnedTaskBuckets>,
) -> Vec<TeamStatusRow> {
    members
        .iter()
        .map(|member| {
            let runtime = runtime_statuses.get(&member.name);
            let pending_inbox = pending_inbox_counts.get(&member.name).copied().unwrap_or(0);
            let triage_backlog = triage_backlog_counts
                .get(&member.name)
                .copied()
                .unwrap_or(0);
            let owned_tasks = owned_task_buckets
                .get(&member.name)
                .cloned()
                .unwrap_or_default();
            let (state, signal, runtime_label) = if member.role_type == config::RoleType::User {
                ("user".to_string(), None, None)
            } else if !session_running {
                ("stopped".to_string(), None, None)
            } else if let Some(runtime) = runtime {
                (
                    runtime.state.clone(),
                    runtime.signal.clone(),
                    runtime.label.clone(),
                )
            } else {
                ("starting".to_string(), None, None)
            };

            let review_backlog = owned_tasks.review.len();
            let state = if session_running && state == "idle" && review_backlog > 0 {
                "reviewing".to_string()
            } else if session_running && state == "idle" && triage_backlog > 0 {
                "triaging".to_string()
            } else {
                state
            };

            let signal = merge_status_signal(signal, triage_backlog, review_backlog);

            TeamStatusRow {
                name: member.name.clone(),
                role: member.role_name.clone(),
                role_type: format!("{:?}", member.role_type),
                agent: member.agent.clone(),
                reports_to: member.reports_to.clone(),
                state,
                pending_inbox,
                triage_backlog,
                active_owned_tasks: owned_tasks.active,
                review_owned_tasks: owned_tasks.review,
                signal,
                runtime_label,
            }
        })
        .collect()
}

fn merge_status_signal(
    signal: Option<String>,
    triage_backlog: usize,
    review_backlog: usize,
) -> Option<String> {
    let triage_signal = (triage_backlog > 0).then(|| format!("needs triage ({triage_backlog})"));
    let review_signal = (review_backlog > 0).then(|| format!("needs review ({review_backlog})"));
    let mut signals = Vec::new();
    if let Some(existing) = signal {
        signals.push(existing);
    }
    if let Some(triage) = triage_signal {
        signals.push(triage);
    }
    if let Some(review) = review_signal {
        signals.push(review);
    }
    if signals.is_empty() {
        None
    } else {
        Some(signals.join(", "))
    }
}

fn triage_backlog_counts(
    project_root: &Path,
    members: &[hierarchy::MemberInstance],
) -> std::collections::HashMap<String, usize> {
    let root = inbox::inboxes_root(project_root);
    let direct_reports = direct_reports_by_member(members);
    direct_reports
        .into_iter()
        .filter_map(|(member_name, reports)| {
            match delivered_direct_report_triage_state(&root, &member_name, &reports) {
                Ok(state) => Some((member_name, state.count)),
                Err(error) => {
                    warn!(member = %member_name, error = %error, "failed to compute lead triage backlog");
                    None
                }
            }
        })
        .collect()
}

fn direct_reports_by_member(
    members: &[hierarchy::MemberInstance],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut direct_reports: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for member in members {
        if let Some(parent) = &member.reports_to {
            direct_reports
                .entry(parent.clone())
                .or_default()
                .push(member.name.clone());
        }
    }
    direct_reports
}

fn delivered_direct_report_triage_count(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
) -> Result<usize> {
    Ok(delivered_direct_report_triage_state(inbox_root, member_name, direct_reports)?.count)
}

fn delivered_direct_report_triage_state(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
) -> Result<TriageBacklogState> {
    if direct_reports.is_empty() {
        return Ok(TriageBacklogState {
            count: 0,
            newest_result_ts: 0,
        });
    }

    let mut latest_outbound_by_report = std::collections::HashMap::new();
    for report in direct_reports {
        let report_messages = inbox::all_messages(inbox_root, report)?;
        let latest_outbound = report_messages
            .iter()
            .filter_map(|(msg, _)| (msg.from == member_name).then_some(msg.timestamp))
            .max()
            .unwrap_or(0);
        latest_outbound_by_report.insert(report.as_str(), latest_outbound);
    }

    let member_messages = inbox::all_messages(inbox_root, member_name)?;
    let mut count = 0usize;
    let mut newest_result_ts = 0u64;
    for (msg, delivered) in &member_messages {
        let needs_triage = *delivered
            && direct_reports.iter().any(|report| report == &msg.from)
            && msg.timestamp
                > *latest_outbound_by_report
                    .get(msg.from.as_str())
                    .unwrap_or(&0);
        if needs_triage {
            count += 1;
            newest_result_ts = newest_result_ts.max(msg.timestamp);
        }
    }

    Ok(TriageBacklogState {
        count,
        newest_result_ts,
    })
}

fn pending_inbox_counts(
    project_root: &Path,
    members: &[hierarchy::MemberInstance],
) -> std::collections::HashMap<String, usize> {
    let root = inbox::inboxes_root(project_root);
    members
        .iter()
        .filter_map(|member| match inbox::pending_message_count(&root, &member.name) {
            Ok(count) => Some((member.name.clone(), count)),
            Err(error) => {
                warn!(member = %member.name, error = %error, "failed to count pending inbox messages");
                None
            }
        })
        .collect()
}

fn classify_owned_task_status(status: &str) -> Option<bool> {
    match status {
        "done" | "archived" => None,
        "review" => Some(false),
        _ => Some(true),
    }
}

fn owned_task_buckets(
    project_root: &Path,
    members: &[hierarchy::MemberInstance],
) -> std::collections::HashMap<String, OwnedTaskBuckets> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return std::collections::HashMap::new();
    }

    let member_names: std::collections::HashSet<&str> =
        members.iter().map(|member| member.name.as_str()).collect();
    let mut owned = std::collections::HashMap::<String, OwnedTaskBuckets>::new();
    let tasks = match crate::task::load_tasks_from_dir(&tasks_dir) {
        Ok(tasks) => tasks,
        Err(error) => {
            warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for status");
            return std::collections::HashMap::new();
        }
    };

    for task in tasks {
        let Some(claimed_by) = task.claimed_by else {
            continue;
        };
        if !member_names.contains(claimed_by.as_str()) {
            continue;
        }
        let Some(is_active) = classify_owned_task_status(task.status.as_str()) else {
            continue;
        };
        let owner = if is_active {
            claimed_by
        } else {
            members
                .iter()
                .find(|member| member.name == claimed_by)
                .and_then(|member| member.reports_to.as_deref())
                .unwrap_or(claimed_by.as_str())
                .to_string()
        };
        let entry = owned.entry(owner).or_default();
        if is_active {
            entry.active.push(task.id);
        } else {
            entry.review.push(task.id);
        }
    }

    for buckets in owned.values_mut() {
        buckets.active.sort_unstable();
        buckets.review.sort_unstable();
    }

    owned
}

fn format_owned_tasks_summary(task_ids: &[u32]) -> String {
    match task_ids {
        [] => "-".to_string(),
        [task_id] => format!("#{task_id}"),
        [first, second] => format!("#{first},#{second}"),
        [first, second, rest @ ..] => format!("#{first},#{second},+{}", rest.len()),
    }
}

/// Path to the resume marker file. Presence indicates agents have prior sessions.
fn resume_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("resume")
}

#[derive(Debug, Deserialize)]
struct DaemonStateResumeProbe {
    #[serde(default)]
    clean_shutdown: bool,
}

fn should_resume_from_daemon_state(project_root: &Path) -> bool {
    let path = daemon_state_path(project_root);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };

    match serde_json::from_str::<DaemonStateResumeProbe>(&content) {
        Ok(state) => !state.clean_shutdown,
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to parse daemon state while probing for resume"
            );
            false
        }
    }
}

/// Path to the pause marker file. Presence pauses nudges and standups.
pub fn pause_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("paused")
}

/// Create the pause marker file, pausing nudges and standups.
pub fn pause_team(project_root: &Path) -> Result<()> {
    let marker = pause_marker_path(project_root);
    if marker.exists() {
        bail!("Team is already paused.");
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&marker, "").context("failed to write pause marker")?;
    info!("paused nudges and standups");
    Ok(())
}

/// Remove the pause marker file, resuming nudges and standups.
pub fn resume_team(project_root: &Path) -> Result<()> {
    let marker = pause_marker_path(project_root);
    if !marker.exists() {
        bail!("Team is not paused.");
    }
    std::fs::remove_file(&marker).context("failed to remove pause marker")?;
    info!("resumed nudges and standups");
    Ok(())
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
    let runtime_statuses = if session_running {
        match list_runtime_member_statuses(&session) {
            Ok(statuses) => statuses,
            Err(error) => {
                warn!(session = %session, error = %error, "failed to read live runtime statuses");
                std::collections::HashMap::new()
            }
        }
    } else {
        std::collections::HashMap::new()
    };
    let pending_inbox_counts = pending_inbox_counts(project_root, &members);
    let triage_backlog_counts = triage_backlog_counts(project_root, &members);
    let owned_task_buckets = owned_task_buckets(project_root, &members);
    let rows = build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &pending_inbox_counts,
        &triage_backlog_counts,
        &owned_task_buckets,
    );
    let workflow_metrics = workflow_metrics_section(project_root, &members);

    if json {
        let status = serde_json::json!({
            "team": team_config.name,
            "session": session,
            "running": session_running,
            "workflow_metrics": workflow_metrics.as_ref().map(|(_, metrics)| serde_json::json!({
                "runnable_count": metrics.runnable_count,
                "blocked_count": metrics.blocked_count,
                "in_review_count": metrics.in_review_count,
                "in_progress_count": metrics.in_progress_count,
                "idle_with_runnable": metrics.idle_with_runnable,
                "oldest_review_age_secs": metrics.oldest_review_age_secs,
                "oldest_assignment_age_secs": metrics.oldest_assignment_age_secs,
            })),
            "members": rows.iter().map(|row| {
                serde_json::json!({
                    "name": row.name,
                    "role": row.role,
                    "role_type": row.role_type,
                    "agent": row.agent,
                    "reports_to": row.reports_to,
                    "state": row.state,
                    "pending_inbox": row.pending_inbox,
                    "triage_backlog": row.triage_backlog,
                    "owned_tasks": row.active_owned_tasks,
                    "active_owned_tasks": row.active_owned_tasks,
                    "review_owned_tasks": row.review_owned_tasks,
                    "signal": row.signal,
                    "runtime_label": row.runtime_label,
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
            "{:<20} {:<12} {:<10} {:<12} {:>5} {:>6} {:<14} {:<14} {:<24} {:<20}",
            "MEMBER",
            "ROLE",
            "AGENT",
            "STATE",
            "INBOX",
            "TRIAGE",
            "ACTIVE",
            "REVIEW",
            "SIGNAL",
            "REPORTS TO"
        );
        println!("{}", "-".repeat(160));
        for row in &rows {
            println!(
                "{:<20} {:<12} {:<10} {:<12} {:>5} {:>6} {:<14} {:<14} {:<24} {:<20}",
                row.name,
                row.role,
                row.agent.as_deref().unwrap_or("-"),
                row.state,
                row.pending_inbox,
                row.triage_backlog,
                format_owned_tasks_summary(&row.active_owned_tasks),
                format_owned_tasks_summary(&row.review_owned_tasks),
                row.signal.as_deref().unwrap_or("-"),
                row.reports_to.as_deref().unwrap_or("-"),
            );
        }
        if let Some((formatted, _)) = workflow_metrics {
            println!();
            println!("{formatted}");
        }
    }

    Ok(())
}

fn workflow_metrics_section(
    project_root: &Path,
    members: &[hierarchy::MemberInstance],
) -> Option<(String, metrics::WorkflowMetrics)> {
    let config_path = team_config_path(project_root);
    if !workflow_metrics_enabled(&config_path) {
        return None;
    }

    let board_dir = team_config_dir(project_root).join("board");
    match metrics::compute_metrics(&board_dir, members) {
        Ok(metrics) => {
            let formatted = metrics::format_metrics(&metrics);
            Some((formatted, metrics))
        }
        Err(error) => {
            warn!(path = %board_dir.display(), error = %error, "failed to compute workflow metrics");
            None
        }
    }
}

fn workflow_metrics_enabled(config_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };

    content.lines().any(|line| {
        let line = line.trim();
        if !line.starts_with("workflow_mode:") {
            return false;
        }
        let value = line
            .split_once(':')
            .map(|(_, value)| value.trim().trim_matches('"').trim_matches('\''))
            .unwrap_or("");
        matches!(value, "hybrid" | "workflow_first")
    })
}

/// Show an estimated team load value from live state, store it, and show recent load trends.
pub fn show_load(project_root: &Path) -> Result<()> {
    let current = capture_team_load(project_root)?;
    if let Err(error) = log_team_load_snapshot(project_root, &current) {
        warn!(error = %error, "failed to append load snapshot to team event log");
    }

    let mut history = read_team_load_history(project_root)?;
    history.push(current);
    history.sort_by_key(|snapshot| snapshot.timestamp);

    println!(
        "Current load: {:.1}% ({} / {} members working)",
        current.load * 100.0,
        current.working_members,
        current.total_members.max(1)
    );
    println!(
        "Session: {}",
        if current.session_running {
            "running"
        } else {
            "stopped"
        }
    );

    if let Some(avg) = average_load(&history, current.timestamp, 10 * 60) {
        println!("10m avg: {:.1}%", avg * 100.0);
    } else {
        println!("10m avg: n/a");
    }
    println!(
        "30m avg: {}",
        average_load(&history, current.timestamp, 30 * 60)
            .map(|avg| format!("{:.1}%", avg * 100.0))
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "60m avg: {}",
        average_load(&history, current.timestamp, 60 * 60)
            .map(|avg| format!("{:.1}%", avg * 100.0))
            .unwrap_or_else(|| "n/a".to_string())
    );

    println!("Load graph (1h):");
    println!("{}", render_load_graph(&history, current.timestamp));
    Ok(())
}

fn capture_team_load(project_root: &Path) -> Result<TeamLoadSnapshot> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let session = format!("batty-{}", team_config.name);
    let session_running = tmux::session_exists(&session);
    let runtime_statuses = if session_running {
        match list_runtime_member_statuses(&session) {
            Ok(statuses) => statuses,
            Err(error) => {
                warn!(session = %session, error = %error, "failed to read runtime statuses for load sampling");
                std::collections::HashMap::new()
            }
        }
    } else {
        std::collections::HashMap::new()
    };

    let triage_backlog_counts = triage_backlog_counts(project_root, &members);
    let owned_task_buckets = owned_task_buckets(project_root, &members);
    let rows = build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &Default::default(),
        &triage_backlog_counts,
        &owned_task_buckets,
    );
    let mut total_members = 0usize;
    let mut working_members = 0usize;

    for row in &rows {
        if row.role_type == "User" {
            continue;
        }
        total_members += 1;
        if counts_as_active_load(row) {
            working_members += 1;
        }
    }

    let load = if total_members == 0 {
        0.0
    } else {
        working_members as f64 / total_members as f64
    };

    Ok(TeamLoadSnapshot {
        timestamp: now_unix(),
        total_members,
        working_members: working_members.min(total_members),
        load,
        session_running,
    })
}

fn counts_as_active_load(row: &TeamStatusRow) -> bool {
    matches!(row.state.as_str(), "working" | "triaging" | "reviewing")
}

fn log_team_load_snapshot(project_root: &Path, snapshot: &TeamLoadSnapshot) -> Result<()> {
    let events_path = team_events_path(project_root);
    let mut sink = events::EventSink::new(&events_path)?;
    let event = events::TeamEvent::load_snapshot(
        snapshot.working_members as u32,
        snapshot.total_members as u32,
        snapshot.session_running,
    );
    sink.emit(event)?;
    Ok(())
}

fn read_team_load_history(project_root: &Path) -> Result<Vec<TeamLoadSnapshot>> {
    let events_path = team_events_path(project_root);
    let events = events::read_events(&events_path)?;
    let mut history = Vec::new();
    for event in events {
        if event.event != "load_snapshot" {
            continue;
        }
        let Some(load) = event.load else {
            continue;
        };
        let Some(working_members) = event.working_members else {
            continue;
        };
        let Some(total_members) = event.total_members else {
            continue;
        };

        history.push(TeamLoadSnapshot {
            timestamp: event.ts,
            total_members: total_members as usize,
            working_members: working_members as usize,
            load,
            session_running: event.session_running.unwrap_or(false),
        });
    }
    Ok(history)
}

fn average_load(samples: &[TeamLoadSnapshot], now: u64, window_seconds: u64) -> Option<f64> {
    let cutoff = now.saturating_sub(window_seconds);
    let mut values = Vec::new();
    for sample in samples {
        if sample.timestamp >= cutoff && sample.timestamp <= now {
            values.push(sample.load);
        }
    }
    if values.is_empty() {
        return None;
    }
    let sum: f64 = values.iter().copied().sum();
    Some(sum / values.len() as f64)
}

fn render_load_graph(samples: &[TeamLoadSnapshot], now: u64) -> String {
    if samples.is_empty() {
        return "(no historical load data yet)".to_string();
    }

    let bucket_size = (LOAD_GRAPH_WINDOW_SECONDS / LOAD_GRAPH_WIDTH as u64).max(1);
    let window_start = now.saturating_sub(LOAD_GRAPH_WINDOW_SECONDS);
    let mut history = String::new();
    let mut previous = 0.0;
    for index in 0..LOAD_GRAPH_WIDTH {
        let bucket_start = window_start + (index as u64 * bucket_size);
        let bucket_end = if index + 1 == LOAD_GRAPH_WIDTH {
            now + 1
        } else {
            bucket_start + bucket_size
        };

        let mut sum = 0.0;
        let mut count = 0usize;
        for sample in samples {
            if sample.timestamp >= bucket_start && sample.timestamp < bucket_end {
                sum += sample.load;
                count += 1;
            }
        }

        let value = if count == 0 {
            previous
        } else {
            sum / count as f64
        };
        previous = value;
        history.push(load_point_char(value));
    }

    history
}

fn load_point_char(value: f64) -> char {
    let clamped = value.clamp(0.0, 1.0);
    match (clamped * 5.0).round() as usize {
        0 => ' ',
        1 => '.',
        2 => ':',
        3 => '=',
        4 => '#',
        _ => '@',
    }
}

/// Validate team config without launching.
pub fn validate_team(project_root: &Path) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    team_config.validate()?;
    let workflow_mode_is_explicit = workflow_mode_declared(&config_path)?;

    let members = hierarchy::resolve_hierarchy(&team_config)?;

    println!("Config: {}", config_path.display());
    println!("Team: {}", team_config.name);
    println!(
        "Workflow mode: {}",
        match team_config.workflow_mode {
            config::WorkflowMode::Legacy => "legacy",
            config::WorkflowMode::Hybrid => "hybrid",
            config::WorkflowMode::WorkflowFirst => "workflow_first",
        }
    );
    println!("Roles: {}", team_config.roles.len());
    println!("Total members: {}", members.len());
    for note in migration_validation_notes(&team_config, workflow_mode_is_explicit) {
        println!("{note}");
    }
    println!("Valid.");
    Ok(())
}

/// Resolve a member instance name (e.g. "eng-1-2") to its role definition name
/// (e.g. "engineer"). Returns the name itself if no config is available.
fn resolve_role_name(project_root: &Path, member_name: &str) -> String {
    // "human" is not a member instance — it's the CLI user
    if matches!(member_name, "human" | "daemon") {
        return member_name.to_string();
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

/// Resolve a caller-facing role/member name to a concrete member instance.
///
/// Examples:
/// - exact member names pass through unchanged (`sam-designer-1-1`)
/// - unique role aliases resolve to their single member instance (`sam-designer`)
/// - ambiguous aliases error and require an explicit member name
fn resolve_member_name(project_root: &Path, member_name: &str) -> Result<String> {
    if matches!(member_name, "human" | "daemon") {
        return Ok(member_name.to_string());
    }

    let config_path = team_config_path(project_root);
    if let Ok(team_config) = config::TeamConfig::load(&config_path) {
        if let Ok(members) = hierarchy::resolve_hierarchy(&team_config) {
            if let Some(member) = members.iter().find(|m| m.name == member_name) {
                return Ok(member.name.clone());
            }

            let matches: Vec<String> = members
                .iter()
                .filter(|m| m.role_name == member_name)
                .map(|m| m.name.clone())
                .collect();

            return match matches.len() {
                0 => Ok(member_name.to_string()),
                1 => Ok(matches[0].clone()),
                _ => bail!(
                    "'{member_name}' matches multiple members: {}. Use the explicit member name.",
                    matches.join(", ")
                ),
            };
        }
    }

    Ok(member_name.to_string())
}

/// Send a message to a role via their Maildir inbox.
///
/// The sender is auto-detected from the `@batty_role` tmux pane option
/// (set during layout). Falls back to "human" if not in a batty pane.
/// Enforces communication routing rules from team config.
pub fn send_message(project_root: &Path, role: &str, msg: &str) -> Result<()> {
    let from = detect_sender().unwrap_or_else(|| "human".to_string());
    let recipient = resolve_member_name(project_root, role)?;

    // Enforce routing: check talks_to rules
    let config_path = team_config_path(project_root);
    if config_path.exists() {
        if let Ok(team_config) = config::TeamConfig::load(&config_path) {
            let from_role = resolve_role_name(project_root, &from);
            let to_role = resolve_role_name(project_root, &recipient);
            if !team_config.can_talk(&from_role, &to_role) {
                bail!(
                    "{from} ({from_role}) is not allowed to message {recipient} ({to_role}). \
                     Check talks_to in team.yaml."
                );
            }
        }
    }

    let root = inbox::inboxes_root(project_root);
    let inbox_msg = inbox::InboxMessage::new_send(&from, &recipient, msg);
    let id = inbox::deliver_to_inbox(&root, &inbox_msg)?;
    if let Err(error) = completion::ingest_completion_message(project_root, msg) {
        warn!(from, to = %recipient, error = %error, "failed to ingest completion packet");
    }
    info!(to = %recipient, id = %id, "message delivered to inbox");
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
pub fn assign_task(project_root: &Path, engineer: &str, task: &str) -> Result<String> {
    let from = detect_sender().unwrap_or_else(|| "human".to_string());
    let recipient = resolve_member_name(project_root, engineer)?;

    let config_path = team_config_path(project_root);
    if config_path.exists() {
        if let Ok(team_config) = config::TeamConfig::load(&config_path) {
            let from_role = resolve_role_name(project_root, &from);
            let to_role = resolve_role_name(project_root, &recipient);
            if !team_config.can_talk(&from_role, &to_role) {
                bail!(
                    "{from} ({from_role}) is not allowed to assign {recipient} ({to_role}). \
                     Check talks_to in team.yaml."
                );
            }
        }
    }

    let root = inbox::inboxes_root(project_root);
    let msg = inbox::InboxMessage::new_assign(&from, &recipient, task);
    let id = inbox::deliver_to_inbox(&root, &msg)?;
    info!(from, engineer = %recipient, task, id = %id, "assignment delivered to inbox");
    Ok(id)
}

/// List inbox messages for a member.
pub fn list_inbox(project_root: &Path, member: &str, limit: Option<usize>) -> Result<()> {
    let member = resolve_member_name(project_root, member)?;
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, &member)?;
    print!("{}", format_inbox_listing(&member, &messages, limit));
    Ok(())
}

fn format_inbox_listing(
    member: &str,
    messages: &[(inbox::InboxMessage, bool)],
    limit: Option<usize>,
) -> String {
    if messages.is_empty() {
        return format!("No messages for {member}.\n");
    }

    let start = match limit {
        Some(0) => messages.len(),
        Some(n) => messages.len().saturating_sub(n),
        None => 0,
    };
    let shown = &messages[start..];
    let refs = inbox_message_refs(messages);
    let shown_refs = &refs[start..];

    let mut out = String::new();
    if shown.len() < messages.len() {
        out.push_str(&format!(
            "Showing {} of {} messages for {member}. Use `-n <N>` or `--all` to see more.\n",
            shown.len(),
            messages.len()
        ));
    }
    out.push_str(&format!(
        "{:<10} {:<12} {:<12} {:<14} BODY\n",
        "STATUS", "FROM", "TYPE", "REF"
    ));
    out.push_str(&format!("{}\n", "-".repeat(96)));
    for ((msg, delivered), msg_ref) in shown.iter().zip(shown_refs.iter()) {
        let status = if *delivered { "delivered" } else { "pending" };
        let body_short = truncate_chars(&msg.body, INBOX_BODY_PREVIEW_CHARS);
        out.push_str(&format!(
            "{:<10} {:<12} {:<12} {:<14} {}\n",
            status,
            msg.from,
            format!("{:?}", msg.msg_type).to_lowercase(),
            msg_ref,
            body_short,
        ));
    }
    out
}

fn inbox_message_refs(messages: &[(inbox::InboxMessage, bool)]) -> Vec<String> {
    let mut totals = HashMap::new();
    for (msg, _) in messages {
        *totals.entry(msg.timestamp).or_insert(0usize) += 1;
    }

    let mut seen = HashMap::new();
    messages
        .iter()
        .map(|(msg, _)| {
            let ordinal = seen.entry(msg.timestamp).or_insert(0usize);
            *ordinal += 1;
            if totals.get(&msg.timestamp).copied().unwrap_or(0) <= 1 {
                msg.timestamp.to_string()
            } else {
                format!("{}-{}", msg.timestamp, ordinal)
            }
        })
        .collect()
}

fn resolve_inbox_message_indices(
    messages: &[(inbox::InboxMessage, bool)],
    selector: &str,
) -> Vec<usize> {
    let refs = inbox_message_refs(messages);
    messages
        .iter()
        .enumerate()
        .filter_map(|(idx, (msg, _))| {
            if msg.id == selector || msg.id.starts_with(selector) || refs[idx] == selector {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut truncated: String = input.chars().take(max_chars).collect();
    truncated.push_str("...");
    truncated
}

/// Read a specific message from a member's inbox by ID, ID prefix, or REF.
pub fn read_message(project_root: &Path, member: &str, id: &str) -> Result<()> {
    let member = resolve_member_name(project_root, member)?;
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, &member)?;

    let matching = resolve_inbox_message_indices(&messages, id);

    match matching.len() {
        0 => bail!("no message matching '{id}' in {member}'s inbox"),
        1 => {
            let (msg, delivered) = &messages[matching[0]];
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
            bail!(
                "'{id}' matches {n} messages — use a longer prefix or the REF column from `batty inbox`"
            );
        }
    }

    Ok(())
}

/// Acknowledge (mark delivered) a message in a member's inbox by ID, prefix, or REF.
pub fn ack_message(project_root: &Path, member: &str, id: &str) -> Result<()> {
    let member = resolve_member_name(project_root, member)?;
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, &member)?;
    let matching = resolve_inbox_message_indices(&messages, id);
    let resolved_id = match matching.len() {
        0 => bail!("no message matching '{id}' in {member}'s inbox"),
        1 => messages[matching[0]].0.id.clone(),
        n => bail!(
            "'{id}' matches {n} messages — use a longer prefix or the REF column from `batty inbox`"
        ),
    };
    inbox::mark_delivered(&root, &member, &resolved_id)?;
    info!(member, id = %resolved_id, "message acknowledged");
    Ok(())
}

/// Merge an engineer's worktree branch.
pub fn merge_worktree(project_root: &Path, engineer: &str) -> Result<()> {
    let engineer = resolve_member_name(project_root, engineer)?;
    match daemon::merge_engineer_branch(project_root, &engineer)? {
        task_loop::MergeOutcome::Success => Ok(()),
        task_loop::MergeOutcome::RebaseConflict(stderr) => {
            bail!("merge blocked by rebase conflict: {stderr}")
        }
        task_loop::MergeOutcome::MergeFailure(stderr) => bail!("merge failed: {stderr}"),
    }
}

/// Run the interactive Telegram setup wizard.
pub fn setup_telegram(project_root: &Path) -> Result<()> {
    telegram::setup_telegram(project_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::RoleType;
    use serial_test::serial;

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
    fn pause_creates_marker_and_resume_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();

        assert!(!pause_marker_path(tmp.path()).exists());
        pause_team(tmp.path()).unwrap();
        assert!(pause_marker_path(tmp.path()).exists());

        // Double-pause should fail
        assert!(pause_team(tmp.path()).is_err());

        resume_team(tmp.path()).unwrap();
        assert!(!pause_marker_path(tmp.path()).exists());

        // Double-resume should fail
        assert!(resume_team(tmp.path()).is_err());
    }

    #[test]
    fn daemon_state_probe_requests_resume_after_unclean_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = daemon_state_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"clean_shutdown":false}"#).unwrap();

        assert!(should_resume_from_daemon_state(tmp.path()));
    }

    #[test]
    fn daemon_state_probe_ignores_clean_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = daemon_state_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"clean_shutdown":true}"#).unwrap();

        assert!(!should_resume_from_daemon_state(tmp.path()));
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
    fn send_message_ingests_completion_packet_into_workflow_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-completion-packets.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: human\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        send_message(
            tmp.path(),
            "architect",
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":true,"artifacts":["docs/workflow.md"],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap();

        let metadata = board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-4/task-27"));
        assert_eq!(
            metadata.worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1-4")
        );
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(true));
        assert_eq!(metadata.outcome.as_deref(), Some("ready_for_review"));
        assert!(metadata.review_blockers.is_empty());
    }

    #[test]
    fn assign_task_delivers_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let id = assign_task(tmp.path(), "eng-1-1", "fix bug").unwrap();
        assert!(!id.is_empty());

        let root = inbox::inboxes_root(tmp.path());
        let pending = inbox::pending_messages(&root, "eng-1-1").unwrap();
        assert_eq!(pending.len(), 1);
        let expected_from = detect_sender().unwrap_or_else(|| "human".to_string());
        assert_eq!(pending[0].from, expected_from);
        assert_eq!(pending[0].to, "eng-1-1");
        assert_eq!(pending[0].body, "fix bug");
        assert_eq!(pending[0].msg_type, inbox::MessageType::Assign);
    }

    fn write_team_config(project_root: &Path, yaml: &str) {
        std::fs::create_dir_all(team_config_dir(project_root)).unwrap();
        std::fs::write(team_config_path(project_root), yaml).unwrap();
    }

    #[test]
    fn workflow_mode_declared_detects_absent_field() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        );

        assert!(!workflow_mode_declared(&team_config_path(tmp.path())).unwrap());
    }

    #[test]
    fn workflow_mode_declared_detects_present_field() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
workflow_mode: hybrid
roles:
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        );

        assert!(workflow_mode_declared(&team_config_path(tmp.path())).unwrap());
    }

    #[test]
    fn migration_validation_notes_explain_legacy_default_for_older_configs() {
        let config =
            config::TeamConfig::load(Path::new("src/team/templates/team_pair.yaml")).unwrap();
        let notes = migration_validation_notes(&config, false);

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("workflow_mode omitted"));
        assert!(notes[0].contains("run unchanged"));
    }

    #[test]
    fn migration_validation_notes_warn_about_workflow_first_partial_rollout() {
        let config: config::TeamConfig = serde_yaml::from_str(
            r#"
name: test
workflow_mode: workflow_first
roles:
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();
        let notes = migration_validation_notes(&config, true);

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("workflow_first mode selected"));
        assert!(notes[0].contains("primary truth"));
    }

    #[test]
    fn resolve_member_name_maps_unique_role_alias_to_instance() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: human
    role_type: user
    talks_to:
      - sam-designer
  - name: jordan-pm
    role_type: manager
    agent: claude
    instances: 1
  - name: sam-designer
    role_type: engineer
    agent: codex
    instances: 1
    talks_to:
      - jordan-pm
"#,
        );

        assert_eq!(
            resolve_member_name(tmp.path(), "sam-designer").unwrap(),
            "sam-designer-1-1"
        );
        assert_eq!(
            resolve_member_name(tmp.path(), "sam-designer-1-1").unwrap(),
            "sam-designer-1-1"
        );
    }

    #[test]
    fn resolve_member_name_rejects_ambiguous_role_alias() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: jordan-pm
    role_type: manager
    agent: claude
    instances: 2
  - name: sam-designer
    role_type: engineer
    agent: codex
    instances: 1
    talks_to:
      - jordan-pm
"#,
        );

        let error = resolve_member_name(tmp.path(), "sam-designer")
            .unwrap_err()
            .to_string();
        assert!(error.contains("matches multiple members"));
        assert!(error.contains("sam-designer-1-1"));
        assert!(error.contains("sam-designer-2-1"));
    }

    #[test]
    #[serial]
    fn send_message_delivers_to_unique_instance_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: human
    role_type: user
    talks_to:
      - sam-designer
  - name: jordan-pm
    role_type: manager
    agent: claude
    instances: 1
  - name: sam-designer
    role_type: engineer
    agent: codex
    instances: 1
    talks_to:
      - jordan-pm
"#,
        );

        let original_tmux_pane = std::env::var_os("TMUX_PANE");
        unsafe {
            std::env::remove_var("TMUX_PANE");
        }
        let send_result = send_message(tmp.path(), "sam-designer", "hello");
        match original_tmux_pane {
            Some(value) => unsafe {
                std::env::set_var("TMUX_PANE", value);
            },
            None => unsafe {
                std::env::remove_var("TMUX_PANE");
            },
        }
        send_result.unwrap();

        let root = inbox::inboxes_root(tmp.path());
        assert!(
            inbox::pending_messages(&root, "sam-designer")
                .unwrap()
                .is_empty()
        );

        let pending = inbox::pending_messages(&root, "sam-designer-1-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].to, "sam-designer-1-1");
        assert_eq!(pending[0].body, "hello");
    }

    #[test]
    fn truncate_chars_handles_unicode_boundaries() {
        let body = "Task #109 confirmed complete on main. I’m available for next assignment.";
        let truncated = truncate_chars(body, 40);
        assert!(truncated.ends_with("..."));
        assert!(truncated.starts_with("Task #109 confirmed complete on main."));
    }

    #[test]
    fn format_inbox_listing_shows_most_recent_messages_by_default_limit() {
        let messages: Vec<_> = (0..25)
            .map(|idx| {
                (
                    inbox::InboxMessage {
                        id: format!("msg{idx:05}"),
                        from: "architect".to_string(),
                        to: "black-lead".to_string(),
                        body: format!("message {idx}"),
                        msg_type: inbox::MessageType::Send,
                        timestamp: idx,
                    },
                    true,
                )
            })
            .collect();

        let rendered = format_inbox_listing("black-lead", &messages, Some(20));
        assert!(rendered.contains("Showing 20 of 25 messages for black-lead."));
        assert!(!rendered.contains("message 0"));
        assert!(rendered.contains("message 5"));
        assert!(rendered.contains("message 24"));
        assert!(!rendered.contains("msg00005"));
        assert!(!rendered.contains("msg00024"));
    }

    #[test]
    fn format_inbox_listing_allows_showing_all_messages() {
        let messages: Vec<_> = (0..3)
            .map(|idx| {
                (
                    inbox::InboxMessage {
                        id: format!("msg{idx:05}"),
                        from: "architect".to_string(),
                        to: "black-lead".to_string(),
                        body: format!("message {idx}"),
                        msg_type: inbox::MessageType::Send,
                        timestamp: idx,
                    },
                    idx % 2 == 0,
                )
            })
            .collect();

        let rendered = format_inbox_listing("black-lead", &messages, None);
        assert!(!rendered.contains("Showing 20"));
        assert!(rendered.contains("REF"));
        assert!(rendered.contains("BODY"));
        assert!(rendered.contains("message 0"));
        assert!(rendered.contains("message 1"));
        assert!(rendered.contains("message 2"));
        assert!(!rendered.contains("msg00000"));
        assert!(!rendered.contains("msg00001"));
        assert!(!rendered.contains("msg00002"));
    }

    #[test]
    fn format_inbox_listing_hides_internal_message_ids() {
        let messages = vec![(
            inbox::InboxMessage {
                id: "1773930387654321.M123456P7890Q42.example".to_string(),
                from: "architect".to_string(),
                to: "black-lead".to_string(),
                body: "message body".to_string(),
                msg_type: inbox::MessageType::Send,
                timestamp: 1_773_930_725,
            },
            true,
        )];

        let rendered = format_inbox_listing("black-lead", &messages, None);
        assert!(rendered.contains("1773930725"));
        assert!(!rendered.contains("1773930387654321.M123456P7890Q42.example"));
        assert!(!rendered.contains("ID BODY"));
    }

    #[test]
    fn inbox_message_refs_use_timestamp_when_unique() {
        let messages = vec![(
            inbox::InboxMessage {
                id: "msg-1".to_string(),
                from: "architect".to_string(),
                to: "black-lead".to_string(),
                body: "message body".to_string(),
                msg_type: inbox::MessageType::Send,
                timestamp: 1_773_930_725,
            },
            true,
        )];

        let refs = inbox_message_refs(&messages);
        assert_eq!(refs, vec!["1773930725".to_string()]);
        assert_eq!(
            resolve_inbox_message_indices(&messages, "1773930725"),
            vec![0]
        );
    }

    #[test]
    fn inbox_message_refs_suffix_same_second_collisions() {
        let messages = vec![
            (
                inbox::InboxMessage {
                    id: "msg-1".to_string(),
                    from: "architect".to_string(),
                    to: "black-lead".to_string(),
                    body: "first".to_string(),
                    msg_type: inbox::MessageType::Send,
                    timestamp: 1_773_930_725,
                },
                true,
            ),
            (
                inbox::InboxMessage {
                    id: "msg-2".to_string(),
                    from: "architect".to_string(),
                    to: "black-lead".to_string(),
                    body: "second".to_string(),
                    msg_type: inbox::MessageType::Send,
                    timestamp: 1_773_930_725,
                },
                true,
            ),
        ];

        let refs = inbox_message_refs(&messages);
        assert_eq!(
            refs,
            vec!["1773930725-1".to_string(), "1773930725-2".to_string()]
        );
        assert!(resolve_inbox_message_indices(&messages, "1773930725").is_empty());
        assert_eq!(
            resolve_inbox_message_indices(&messages, "1773930725-1"),
            vec![0]
        );
        assert_eq!(
            resolve_inbox_message_indices(&messages, "1773930725-2"),
            vec![1]
        );
    }

    #[test]
    fn assignment_result_round_trip_and_format() {
        let tmp = tempfile::tempdir().unwrap();
        let result = AssignmentDeliveryResult {
            message_id: "msg-1".to_string(),
            status: AssignmentResultStatus::Delivered,
            engineer: "eng-1-1".to_string(),
            task_summary: "Say Hello".to_string(),
            branch: Some("eng-1-1/task-1".to_string()),
            work_dir: Some("/tmp/worktree".to_string()),
            detail: "assignment launched".to_string(),
            ts: now_unix(),
        };

        store_assignment_result(tmp.path(), &result).unwrap();
        let loaded = load_assignment_result(tmp.path(), "msg-1")
            .unwrap()
            .unwrap();
        assert_eq!(loaded, result);

        let formatted = format_assignment_result(&loaded);
        assert!(formatted.contains("Assignment delivered: msg-1 -> eng-1-1"));
        assert!(formatted.contains("Branch: eng-1-1/task-1"));
        assert!(formatted.contains("Worktree: /tmp/worktree"));
    }

    #[test]
    fn wait_for_assignment_result_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result =
            wait_for_assignment_result(tmp.path(), "missing", Duration::from_millis(10)).unwrap();
        assert!(result.is_none());
    }

    fn make_member(name: &str, role_name: &str, role_type: RoleType) -> hierarchy::MemberInstance {
        hierarchy::MemberInstance {
            name: name.to_string(),
            role_name: role_name.to_string(),
            role_type,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }
    }

    #[test]
    fn strip_tmux_style_removes_formatting_sequences() {
        let raw = "#[fg=yellow]idle#[default] #[fg=magenta]nudge 1:05#[default]";
        assert_eq!(strip_tmux_style(raw), "idle nudge 1:05");
    }

    #[test]
    fn summarize_runtime_member_status_extracts_state_and_signal() {
        let summary = summarize_runtime_member_status(
            "#[fg=cyan]working#[default] #[fg=blue]standup 4:12#[default]",
            false,
        );

        assert_eq!(summary.state, "working");
        assert_eq!(summary.signal.as_deref(), Some("standup"));
        assert_eq!(summary.label.as_deref(), Some("working standup 4:12"));
    }

    #[test]
    fn summarize_runtime_member_status_marks_nudge_and_standup_together() {
        let summary = summarize_runtime_member_status(
            "#[fg=yellow]idle#[default] #[fg=magenta]nudge now#[default] #[fg=blue]standup 0:10#[default]",
            false,
        );

        assert_eq!(summary.state, "idle");
        assert_eq!(
            summary.signal.as_deref(),
            Some("waiting for nudge, standup")
        );
    }

    #[test]
    fn summarize_runtime_member_status_distinguishes_sent_nudge() {
        let summary = summarize_runtime_member_status(
            "#[fg=yellow]idle#[default] #[fg=magenta]nudge sent#[default]",
            false,
        );

        assert_eq!(summary.state, "idle");
        assert_eq!(summary.signal.as_deref(), Some("nudged"));
        assert_eq!(summary.label.as_deref(), Some("idle nudge sent"));
    }

    #[test]
    fn summarize_runtime_member_status_tracks_paused_automation() {
        let summary = summarize_runtime_member_status(
            "#[fg=cyan]working#[default] #[fg=244]nudge paused#[default] #[fg=244]standup paused#[default]",
            false,
        );

        assert_eq!(summary.state, "working");
        assert_eq!(
            summary.signal.as_deref(),
            Some("nudge paused, standup paused")
        );
        assert_eq!(
            summary.label.as_deref(),
            Some("working nudge paused standup paused")
        );
    }

    #[test]
    fn build_team_status_rows_defaults_by_session_state() {
        let architect = make_member("architect", "architect", RoleType::Architect);
        let human = hierarchy::MemberInstance {
            name: "human".to_string(),
            role_name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };

        let pending = std::collections::HashMap::from([
            (architect.name.clone(), 3usize),
            (human.name.clone(), 1usize),
        ]);
        let triage = std::collections::HashMap::from([(architect.name.clone(), 2usize)]);
        let owned = std::collections::HashMap::from([(
            architect.name.clone(),
            OwnedTaskBuckets {
                active: vec![191u32],
                review: vec![193u32],
            },
        )]);
        let rows = build_team_status_rows(
            &[architect.clone(), human.clone()],
            false,
            &Default::default(),
            &pending,
            &triage,
            &owned,
        );
        assert_eq!(rows[0].state, "stopped");
        assert_eq!(rows[0].pending_inbox, 3);
        assert_eq!(rows[0].triage_backlog, 2);
        assert_eq!(rows[0].active_owned_tasks, vec![191]);
        assert_eq!(rows[0].review_owned_tasks, vec![193]);
        assert_eq!(rows[1].state, "user");
        assert_eq!(rows[1].pending_inbox, 1);
        assert_eq!(rows[1].triage_backlog, 0);
        assert!(rows[1].active_owned_tasks.is_empty());
        assert!(rows[1].review_owned_tasks.is_empty());

        let runtime = std::collections::HashMap::from([(
            architect.name.clone(),
            RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: Some("standup".to_string()),
                label: Some("idle standup 2:00".to_string()),
            },
        )]);
        let rows = build_team_status_rows(&[architect], true, &runtime, &pending, &triage, &owned);
        assert_eq!(rows[0].state, "reviewing");
        assert_eq!(rows[0].pending_inbox, 3);
        assert_eq!(rows[0].triage_backlog, 2);
        assert_eq!(rows[0].active_owned_tasks, vec![191]);
        assert_eq!(rows[0].review_owned_tasks, vec![193]);
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("standup, needs triage (2), needs review (1)")
        );
        assert_eq!(rows[0].runtime_label.as_deref(), Some("idle standup 2:00"));
    }

    #[test]
    fn delivered_direct_report_triage_count_only_counts_results_newer_than_lead_response() {
        let tmp = tempfile::tempdir().unwrap();
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "eng-2").unwrap();

        let mut old_result = inbox::InboxMessage::new_send("eng-1", "lead", "old result");
        old_result.timestamp = 10;
        let old_result_id = inbox::deliver_to_inbox(&root, &old_result).unwrap();
        inbox::mark_delivered(&root, "lead", &old_result_id).unwrap();

        let mut lead_reply = inbox::InboxMessage::new_send("lead", "eng-1", "next task");
        lead_reply.timestamp = 20;
        let lead_reply_id = inbox::deliver_to_inbox(&root, &lead_reply).unwrap();
        inbox::mark_delivered(&root, "eng-1", &lead_reply_id).unwrap();

        let mut new_result = inbox::InboxMessage::new_send("eng-1", "lead", "new result");
        new_result.timestamp = 30;
        let new_result_id = inbox::deliver_to_inbox(&root, &new_result).unwrap();
        inbox::mark_delivered(&root, "lead", &new_result_id).unwrap();

        let mut other_result = inbox::InboxMessage::new_send("eng-2", "lead", "parallel result");
        other_result.timestamp = 40;
        let other_result_id = inbox::deliver_to_inbox(&root, &other_result).unwrap();
        inbox::mark_delivered(&root, "lead", &other_result_id).unwrap();

        let triage_count = delivered_direct_report_triage_count(
            &root,
            "lead",
            &["eng-1".to_string(), "eng-2".to_string()],
        )
        .unwrap();
        assert_eq!(triage_count, 2);
    }

    #[test]
    fn counts_as_active_load_treats_triaging_as_working() {
        let triaging = TeamStatusRow {
            name: "lead".to_string(),
            role: "lead".to_string(),
            role_type: "Manager".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("architect".to_string()),
            state: "triaging".to_string(),
            pending_inbox: 0,
            triage_backlog: 2,
            active_owned_tasks: vec![191],
            review_owned_tasks: vec![193],
            signal: Some("needs triage (2)".to_string()),
            runtime_label: Some("idle".to_string()),
        };
        let reviewing = TeamStatusRow {
            state: "reviewing".to_string(),
            triage_backlog: 0,
            signal: Some("needs review (1)".to_string()),
            runtime_label: Some("idle".to_string()),
            ..triaging.clone()
        };
        let idle = TeamStatusRow {
            state: "idle".to_string(),
            triage_backlog: 0,
            signal: None,
            runtime_label: Some("idle".to_string()),
            ..triaging.clone()
        };

        assert!(counts_as_active_load(&triaging));
        assert!(counts_as_active_load(&reviewing));
        assert!(!counts_as_active_load(&idle));
    }

    #[test]
    fn format_owned_tasks_summary_compacts_multiple_ids() {
        assert_eq!(format_owned_tasks_summary(&[]), "-");
        assert_eq!(format_owned_tasks_summary(&[191]), "#191");
        assert_eq!(format_owned_tasks_summary(&[191, 192]), "#191,#192");
        assert_eq!(format_owned_tasks_summary(&[191, 192, 193]), "#191,#192,+1");
    }

    #[test]
    fn owned_task_buckets_split_active_and_review_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let members = vec![
            make_member("lead", "lead", RoleType::Manager),
            hierarchy::MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                prompt: None,
                reports_to: Some("lead".to_string()),
                use_worktrees: false,
            },
        ];
        std::fs::create_dir_all(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();
        std::fs::write(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("191-active.md"),
            "---\nid: 191\ntitle: Active\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("193-review.md"),
            "---\nid: 193\ntitle: Review\nstatus: review\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        )
        .unwrap();

        let owned = owned_task_buckets(tmp.path(), &members);
        let buckets = owned.get("eng-1").unwrap();
        assert_eq!(buckets.active, vec![191]);
        assert!(buckets.review.is_empty());
        let review_buckets = owned.get("lead").unwrap();
        assert!(review_buckets.active.is_empty());
        assert_eq!(review_buckets.review, vec![193]);
    }

    #[test]
    fn workflow_metrics_enabled_detects_supported_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("team.yaml");

        std::fs::write(
            &config_path,
            "name: test\nworkflow_mode: hybrid\nroles: []\n",
        )
        .unwrap();
        assert!(workflow_metrics_enabled(&config_path));

        std::fs::write(
            &config_path,
            "name: test\nworkflow_mode: workflow_first\nroles: []\n",
        )
        .unwrap();
        assert!(workflow_metrics_enabled(&config_path));

        std::fs::write(&config_path, "name: test\nroles: []\n").unwrap();
        assert!(!workflow_metrics_enabled(&config_path));
    }

    #[test]
    fn team_status_metrics_section_renders_when_workflow_mode_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let team_dir = tmp.path().join(".batty").join("team_config");
        let board_dir = team_dir.join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            team_dir.join("team.yaml"),
            "name: test\nworkflow_mode: hybrid\nroles:\n  - name: engineer\n    role_type: engineer\n    agent: codex\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("031-runnable.md"),
            "---\nid: 31\ntitle: Runnable\nstatus: todo\npriority: medium\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let members = vec![make_member("eng-1-1", "engineer", RoleType::Engineer)];
        let section = workflow_metrics_section(tmp.path(), &members).unwrap();

        assert!(section.0.contains("Workflow Metrics"));
        assert_eq!(section.1.runnable_count, 1);
        assert_eq!(section.1.idle_with_runnable, vec!["eng-1-1"]);
    }

    #[test]
    #[serial]
    fn list_runtime_member_statuses_reads_tmux_role_and_status_options() {
        let session = "batty-test-team-status-runtime";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(session, "sleep", &["20".to_string()], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();

        let role_output = std::process::Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "@batty_role", "eng-1"])
            .output()
            .unwrap();
        assert!(role_output.status.success());

        let status_output = std::process::Command::new("tmux")
            .args([
                "set-option",
                "-p",
                "-t",
                &pane_id,
                "@batty_status",
                "#[fg=yellow]idle#[default] #[fg=magenta]nudge 0:30#[default]",
            ])
            .output()
            .unwrap();
        assert!(status_output.status.success());

        let statuses = list_runtime_member_statuses(session).unwrap();
        let eng = statuses.get("eng-1").unwrap();
        assert_eq!(eng.state, "idle");
        assert_eq!(eng.signal.as_deref(), Some("waiting for nudge"));
        assert_eq!(eng.label.as_deref(), Some("idle nudge 0:30"));

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    fn average_load_ignores_points_older_than_window() {
        let now = 10_000u64;
        let samples = vec![
            TeamLoadSnapshot {
                timestamp: now - 3_000,
                total_members: 10,
                working_members: 0,
                load: 0.8,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 10,
                total_members: 10,
                working_members: 0,
                load: 0.4,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 20,
                total_members: 10,
                working_members: 0,
                load: 0.6,
                session_running: true,
            },
        ];

        let avg_60s = average_load(&samples, now, 60).unwrap();
        assert!((avg_60s - 0.5).abs() < 0.0001);
        assert!(average_load(&samples, now, 5).is_none());
    }

    #[test]
    fn render_load_graph_returns_expected_width() {
        let now = 10_000u64;
        let samples = vec![
            TeamLoadSnapshot {
                timestamp: now - 3_600,
                total_members: 10,
                working_members: 2,
                load: 0.2,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 1_800,
                total_members: 10,
                working_members: 5,
                load: 0.5,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 900,
                total_members: 10,
                working_members: 10,
                load: 1.0,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 600,
                total_members: 10,
                working_members: 0,
                load: 0.0,
                session_running: true,
            },
        ];

        let graph = render_load_graph(&samples, now);
        assert_eq!(graph.len(), LOAD_GRAPH_WIDTH);
        assert!(graph.chars().all(|c| " .:=#@".contains(c)));
    }
}
