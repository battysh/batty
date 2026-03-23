//! Team mode — hierarchical agent org chart with daemon-managed communication.
//!
//! A YAML-defined team (architect ↔ manager ↔ N engineers) runs in a tmux
//! session. The daemon monitors panes, routes messages between roles, and
//! manages agent lifecycles.

pub mod artifact;
pub mod auto_merge;
#[cfg(test)]
mod behavioral_tests;
pub mod board;
// -- Decomposed submodules --
mod init;
pub use init::*;
mod load;
pub use load::*;
mod messaging;
pub use messaging::*;
pub mod board_cmd;
pub mod board_health;
pub mod capability;
pub mod checkpoint;
pub mod comms;
pub mod completion;
pub mod config;
pub mod cost;
pub mod daemon;
pub mod delivery;
pub mod deps;
pub mod doctor;
pub mod errors;
pub mod estimation;
pub mod events;
pub mod failure_patterns;
pub mod git_cmd;
pub mod grafana;
pub mod harness;
pub mod hierarchy;
pub mod inbox;
pub mod layout;
pub mod merge;
pub mod message;
pub mod metrics;
pub mod metrics_cmd;
pub mod nudge;
pub mod policy;
pub mod resolver;
pub mod retrospective;
pub mod retry;
pub mod review;
pub mod standup;
pub mod status;
pub mod task_cmd;
pub mod task_loop;
pub mod telegram;
pub mod telemetry_db;
#[cfg(test)]
pub mod test_helpers;
#[cfg(test)]
pub mod test_support;
pub mod validation;
pub mod watcher;
pub mod workflow;

use std::fs::File;
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

const TRIAGE_RESULT_FRESHNESS_SECONDS: u64 = 300;
const LOG_ROTATION_BYTES: u64 = 5 * 1024 * 1024;
const LOG_ROTATION_KEEP: usize = 3;
pub(crate) const DEFAULT_EVENT_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DAEMON_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const DAEMON_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

pub(crate) fn orchestrator_ansi_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("orchestrator.ansi.log")
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

/// Path to the daemon PID file.
fn daemon_pid_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("daemon.pid")
}

/// Path to the daemon log file.
pub(crate) fn daemon_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("daemon.log")
}

fn rotated_log_path(path: &Path, generation: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), generation))
}

pub(crate) fn rotate_log_if_needed(path: &Path) -> Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };

    if len <= LOG_ROTATION_BYTES {
        return Ok(());
    }

    let oldest = rotated_log_path(path, LOG_ROTATION_KEEP);
    if oldest.exists() {
        std::fs::remove_file(&oldest)
            .with_context(|| format!("failed to remove {}", oldest.display()))?;
    }

    for generation in (1..LOG_ROTATION_KEEP).rev() {
        let source = rotated_log_path(path, generation);
        if !source.exists() {
            continue;
        }
        let destination = rotated_log_path(path, generation + 1);
        std::fs::rename(&source, &destination).with_context(|| {
            format!(
                "failed to rotate {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    }

    let rotated = rotated_log_path(path, 1);
    std::fs::rename(path, &rotated).with_context(|| {
        format!(
            "failed to rotate {} to {}",
            path.display(),
            rotated.display()
        )
    })?;
    Ok(())
}

pub(crate) fn open_log_for_append(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate_log_if_needed(path)?;
    File::options()
        .append(true)
        .create(true)
        .open(path)
        .with_context(|| format!("failed to open log file: {}", path.display()))
}

fn daemon_spawn_args(root_str: &str, resume: bool) -> Vec<String> {
    let mut args = vec![
        "-v".to_string(),
        "daemon".to_string(),
        "--project-root".to_string(),
        root_str.to_string(),
    ];
    if resume {
        args.push("--resume".to_string());
    }
    args
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
    use std::process::{Command, Stdio};

    let log_path = daemon_log_path(project_root);
    let pid_path = daemon_pid_path(project_root);

    // Ensure .batty/ exists
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let log_file = open_log_for_append(&log_path)?;
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
    let args = daemon_spawn_args(&root_str, resume);
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

    let mut child = cmd.spawn().context("failed to spawn daemon process")?;
    let pid = child.id();

    // Give the child a moment to start up and verify it didn't exit immediately
    // (e.g. due to an unrecognized subcommand in an outdated binary).
    std::thread::sleep(std::time::Duration::from_millis(500));
    match child.try_wait() {
        Ok(Some(status)) => {
            let _ = std::fs::remove_file(&pid_path);
            // Read the last few lines of the daemon log for the actual error
            let tail = std::fs::read_to_string(&log_path).ok().and_then(|s| {
                let lines: Vec<&str> = s.lines().collect();
                let start = lines.len().saturating_sub(5);
                let tail = lines[start..].join("\n");
                if tail.trim().is_empty() {
                    None
                } else {
                    Some(tail)
                }
            });
            match tail {
                Some(detail) => bail!(
                    "daemon process exited immediately with {status}\n\n\
                     {detail}\n\n\
                     see full log: {log}",
                    log = log_path.display(),
                ),
                None => bail!(
                    "daemon process exited immediately with {status}; \
                     see {log} for details",
                    log = log_path.display(),
                ),
            }
        }
        Ok(None) => {} // still running — good
        Err(e) => {
            warn!(pid, error = %e, "failed to check daemon process status");
        }
    }

    std::fs::write(&pid_path, pid.to_string())
        .with_context(|| format!("failed to write PID file: {}", pid_path.display()))?;

    info!(pid, log = %log_path.display(), "daemon spawned");
    Ok(pid)
}

/// Kill the daemon process if it's running.
fn read_daemon_pid(project_root: &Path) -> Option<u32> {
    let pid_path = daemon_pid_path(project_root);
    let pid_str = std::fs::read_to_string(pid_path).ok()?;
    pid_str.trim().parse::<u32>().ok()
}

#[cfg(unix)]
fn send_unix_signal(pid: u32, signal: libc::c_int) -> bool {
    let status = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if status == 0 {
        true
    } else {
        let error = std::io::Error::last_os_error();
        warn!(pid, signal, error = %error, "failed to signal daemon");
        false
    }
}

#[cfg(not(unix))]
fn send_unix_signal(_pid: u32, _signal: i32) -> bool {
    false
}

#[cfg(unix)]
fn daemon_process_exists(pid: u32) -> bool {
    let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if status == 0 {
        true
    } else {
        !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        )
    }
}

#[cfg(not(unix))]
fn daemon_process_exists(_pid: u32) -> bool {
    false
}

fn wait_for_graceful_daemon_shutdown(
    project_root: &Path,
    pid: u32,
    previous_saved_at: Option<u64>,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let clean_snapshot = daemon_state_indicates_clean_shutdown(project_root, previous_saved_at);
        if clean_snapshot {
            let _ = std::fs::remove_file(daemon_pid_path(project_root));
            return true;
        }
        let running = daemon_process_exists(pid);
        if !running {
            let _ = std::fs::remove_file(daemon_pid_path(project_root));
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(DAEMON_SHUTDOWN_POLL_INTERVAL);
    }
}

fn request_graceful_daemon_shutdown(project_root: &Path, timeout: Duration) -> bool {
    let Some(pid) = read_daemon_pid(project_root) else {
        return true;
    };

    let previous_saved_at = read_daemon_state_probe(project_root).and_then(|state| state.saved_at);
    #[cfg(unix)]
    {
        if !send_unix_signal(pid, libc::SIGTERM) {
            return false;
        }
        info!(pid, "sent SIGTERM to daemon");
    }
    #[cfg(not(unix))]
    {
        warn!(
            pid,
            "graceful daemon shutdown is not supported on this platform"
        );
        return false;
    }

    wait_for_graceful_daemon_shutdown(project_root, pid, previous_saved_at, timeout)
}

fn force_kill_daemon(project_root: &Path) {
    let Some(pid) = read_daemon_pid(project_root) else {
        return;
    };

    #[cfg(unix)]
    {
        if send_unix_signal(pid, libc::SIGKILL) {
            info!(pid, "sent SIGKILL to daemon");
        }
    }
    #[cfg(not(unix))]
    {
        warn!(pid, "cannot force-kill daemon on this platform");
    }

    let _ = std::fs::remove_file(daemon_pid_path(project_root));
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
        team_config.orchestrator_position,
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

/// Path to the resume marker file. Presence indicates agents have prior sessions.
fn resume_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("resume")
}

#[derive(Debug, Deserialize)]
struct DaemonStateResumeProbe {
    #[serde(default)]
    clean_shutdown: bool,
    #[serde(default)]
    saved_at: Option<u64>,
}

fn read_daemon_state_probe(project_root: &Path) -> Option<DaemonStateResumeProbe> {
    let path = daemon_state_path(project_root);
    let content = std::fs::read_to_string(&path).ok()?;

    match serde_json::from_str::<DaemonStateResumeProbe>(&content) {
        Ok(state) => Some(state),
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to parse daemon state while probing for resume"
            );
            None
        }
    }
}

fn daemon_state_indicates_clean_shutdown(
    project_root: &Path,
    previous_saved_at: Option<u64>,
) -> bool {
    let Some(state) = read_daemon_state_probe(project_root) else {
        return false;
    };

    state.clean_shutdown
        && match (state.saved_at, previous_saved_at) {
            (Some(saved_at), Some(previous_saved_at)) => saved_at > previous_saved_at,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => true,
        }
}

fn should_resume_from_daemon_state(project_root: &Path) -> bool {
    read_daemon_state_probe(project_root)
        .map(|state| !state.clean_shutdown)
        .unwrap_or(false)
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

/// Path to the nudge-disabled marker for a given intervention.
pub fn nudge_disabled_marker_path(project_root: &Path, intervention: &str) -> PathBuf {
    project_root
        .join(".batty")
        .join(format!("nudge_{intervention}_disabled"))
}

/// Create a nudge-disabled marker file, disabling the intervention at runtime.
pub fn disable_nudge(project_root: &Path, intervention: &str) -> Result<()> {
    let marker = nudge_disabled_marker_path(project_root, intervention);
    if marker.exists() {
        bail!("Intervention '{intervention}' is already disabled.");
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&marker, "").context("failed to write nudge disabled marker")?;
    info!(intervention, "disabled intervention");
    Ok(())
}

/// Remove a nudge-disabled marker file, re-enabling the intervention.
pub fn enable_nudge(project_root: &Path, intervention: &str) -> Result<()> {
    let marker = nudge_disabled_marker_path(project_root, intervention);
    if !marker.exists() {
        bail!("Intervention '{intervention}' is not disabled.");
    }
    std::fs::remove_file(&marker).context("failed to remove nudge disabled marker")?;
    info!(intervention, "enabled intervention");
    Ok(())
}

/// Print a table showing config, runtime, and effective state for each intervention.
pub fn nudge_status(project_root: &Path) -> Result<()> {
    use crate::cli::NudgeIntervention;

    let config_path = team_config_path(project_root);
    let automation = if config_path.exists() {
        let team_config = config::TeamConfig::load(&config_path)?;
        Some(team_config.automation)
    } else {
        None
    };

    println!(
        "{:<16} {:<10} {:<10} {:<10}",
        "INTERVENTION", "CONFIG", "RUNTIME", "EFFECTIVE"
    );

    for intervention in NudgeIntervention::ALL {
        let name = intervention.marker_name();
        let config_enabled = automation
            .as_ref()
            .map(|a| match intervention {
                NudgeIntervention::Replenish => true, // no dedicated config flag
                NudgeIntervention::Triage => a.triage_interventions,
                NudgeIntervention::Review => a.review_interventions,
                NudgeIntervention::Dispatch => a.manager_dispatch_interventions,
                NudgeIntervention::Utilization => a.architect_utilization_interventions,
                NudgeIntervention::OwnedTask => a.owned_task_interventions,
            })
            .unwrap_or(true);

        let runtime_disabled = nudge_disabled_marker_path(project_root, name).exists();
        let runtime_str = if runtime_disabled {
            "disabled"
        } else {
            "enabled"
        };
        let config_str = if config_enabled {
            "enabled"
        } else {
            "disabled"
        };
        let effective = config_enabled && !runtime_disabled;
        let effective_str = if effective { "enabled" } else { "DISABLED" };

        println!(
            "{:<16} {:<10} {:<10} {:<10}",
            name, config_str, runtime_str, effective_str
        );
    }

    Ok(())
}

/// Stop a running team session and clean up any orphaned `batty-` sessions.
/// Summary statistics for a completed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSummary {
    pub tasks_completed: u32,
    pub tasks_merged: u32,
    pub runtime_secs: u64,
}

impl SessionSummary {
    pub fn display(&self) -> String {
        format!(
            "Session summary: {} tasks completed, {} merged, runtime {}",
            self.tasks_completed,
            self.tasks_merged,
            format_runtime(self.runtime_secs),
        )
    }
}

fn format_runtime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {mins}m")
        }
    }
}

/// Compute session summary from the event log.
///
/// Finds the most recent `daemon_started` event and counts completions and
/// merges that occurred after it. Runtime is calculated from the daemon start
/// timestamp to now.
pub(crate) fn compute_session_summary(project_root: &Path) -> Option<SessionSummary> {
    let events_path = team_events_path(project_root);
    let all_events = events::read_events(&events_path).ok()?;

    // Find the most recent daemon_started event.
    let session_start = all_events
        .iter()
        .rev()
        .find(|e| e.event == "daemon_started")?;
    let start_ts = session_start.ts;
    let now_ts = now_unix();

    let session_events: Vec<_> = all_events.iter().filter(|e| e.ts >= start_ts).collect();

    let tasks_completed = session_events
        .iter()
        .filter(|e| e.event == "task_completed")
        .count() as u32;

    let tasks_merged = session_events
        .iter()
        .filter(|e| e.event == "task_auto_merged" || e.event == "task_manual_merged")
        .count() as u32;

    let runtime_secs = now_ts.saturating_sub(start_ts);

    Some(SessionSummary {
        tasks_completed,
        tasks_merged,
        runtime_secs,
    })
}

pub fn stop_team(project_root: &Path) -> Result<()> {
    // Compute session summary before shutting down (events log is still available).
    let summary = compute_session_summary(project_root);

    // Write resume marker before tearing down — agents have sessions to continue
    let marker = resume_marker_path(project_root);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&marker, "").ok();

    // Ask the daemon to persist a final clean snapshot before the tmux session is torn down.
    if !request_graceful_daemon_shutdown(project_root, DAEMON_SHUTDOWN_GRACE_PERIOD) {
        warn!("daemon did not stop gracefully; forcing shutdown");
        force_kill_daemon(project_root);
    }

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

    // Print session summary after teardown.
    if let Some(summary) = summary {
        println!();
        println!("{}", summary.display());
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
        let mut sessions = tmux::list_sessions_with_prefix("batty-");
        match sessions.len() {
            0 => bail!("no team config found and no batty sessions running"),
            1 => sessions.swap_remove(0),
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
        match status::list_runtime_member_statuses(&session) {
            Ok(statuses) => statuses,
            Err(error) => {
                warn!(session = %session, error = %error, "failed to read live runtime statuses");
                std::collections::HashMap::new()
            }
        }
    } else {
        std::collections::HashMap::new()
    };
    let pending_inbox_counts = status::pending_inbox_counts(project_root, &members);
    let triage_backlog_counts = status::triage_backlog_counts(project_root, &members);
    let owned_task_buckets = status::owned_task_buckets(project_root, &members);
    let agent_health = status::agent_health_by_member(project_root, &members);
    let paused = pause_marker_path(project_root).exists();
    let mut rows = status::build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &pending_inbox_counts,
        &triage_backlog_counts,
        &owned_task_buckets,
        &agent_health,
    );

    // Populate ETA estimates for members with active tasks.
    let active_task_elapsed: Vec<(u32, u64)> = rows
        .iter()
        .filter(|row| !row.active_owned_tasks.is_empty())
        .flat_map(|row| {
            let elapsed = row.health.task_elapsed_secs.unwrap_or(0);
            row.active_owned_tasks
                .iter()
                .map(move |&task_id| (task_id, elapsed))
        })
        .collect();
    let etas = estimation::compute_etas(project_root, &active_task_elapsed);
    for row in &mut rows {
        if let Some(&task_id) = row.active_owned_tasks.first() {
            if let Some(eta) = etas.get(&task_id) {
                row.eta = eta.clone();
            }
        }
    }

    let workflow_metrics = status::workflow_metrics_section(project_root, &members);
    let (active_tasks, review_queue) = match status::board_status_task_queues(project_root) {
        Ok(queues) => queues,
        Err(error) => {
            warn!(error = %error, "failed to load board task queues for status json");
            (Vec::new(), Vec::new())
        }
    };

    if json {
        let report = status::build_team_status_json_report(status::TeamStatusJsonReportInput {
            team: team_config.name.clone(),
            session: session.clone(),
            session_running,
            paused,
            workflow_metrics: workflow_metrics
                .as_ref()
                .map(|(_, metrics)| metrics.clone()),
            active_tasks,
            review_queue,
            members: rows,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
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
            "{:<20} {:<12} {:<10} {:<12} {:>5} {:>6} {:<14} {:<14} {:<16} {:<18} {:<24} {:<20}",
            "MEMBER",
            "ROLE",
            "AGENT",
            "STATE",
            "INBOX",
            "TRIAGE",
            "ACTIVE",
            "REVIEW",
            "ETA",
            "HEALTH",
            "SIGNAL",
            "REPORTS TO"
        );
        println!("{}", "-".repeat(195));
        for row in &rows {
            println!(
                "{:<20} {:<12} {:<10} {:<12} {:>5} {:>6} {:<14} {:<14} {:<16} {:<18} {:<24} {:<20}",
                row.name,
                row.role,
                row.agent.as_deref().unwrap_or("-"),
                row.state,
                row.pending_inbox,
                row.triage_backlog,
                status::format_owned_tasks_summary(&row.active_owned_tasks),
                status::format_owned_tasks_summary(&row.review_owned_tasks),
                row.eta,
                row.health_summary,
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

/// Validate team config without launching.
pub fn validate_team(project_root: &Path, verbose: bool) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;

    if verbose {
        let checks = team_config.validate_verbose();
        let mut any_failed = false;
        for check in &checks {
            let status = if check.passed { "PASS" } else { "FAIL" };
            println!("[{status}] {}: {}", check.name, check.detail);
            if !check.passed {
                any_failed = true;
            }
        }
        if any_failed {
            bail!("validation failed — see FAIL checks above");
        }
    } else {
        team_config.validate()?;
    }

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

#[cfg(test)]
mod tests {
    use super::status;
    use super::*;
    use crate::team::config::RoleType;
    use serial_test::serial;
    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.original.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

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
    fn nudge_disable_creates_marker_and_enable_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();

        let marker = nudge_disabled_marker_path(tmp.path(), "triage");
        assert!(!marker.exists());

        disable_nudge(tmp.path(), "triage").unwrap();
        assert!(marker.exists());

        // Double-disable should fail
        assert!(disable_nudge(tmp.path(), "triage").is_err());

        enable_nudge(tmp.path(), "triage").unwrap();
        assert!(!marker.exists());

        // Double-enable should fail
        assert!(enable_nudge(tmp.path(), "triage").is_err());
    }

    #[test]
    fn nudge_marker_path_uses_intervention_name() {
        let root = std::path::Path::new("/tmp/test-project");
        assert_eq!(
            nudge_disabled_marker_path(root, "replenish"),
            root.join(".batty").join("nudge_replenish_disabled")
        );
        assert_eq!(
            nudge_disabled_marker_path(root, "owned-task"),
            root.join(".batty").join("nudge_owned-task_disabled")
        );
    }

    #[test]
    fn nudge_multiple_interventions_independent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();

        disable_nudge(tmp.path(), "triage").unwrap();
        disable_nudge(tmp.path(), "review").unwrap();

        assert!(nudge_disabled_marker_path(tmp.path(), "triage").exists());
        assert!(nudge_disabled_marker_path(tmp.path(), "review").exists());
        assert!(!nudge_disabled_marker_path(tmp.path(), "dispatch").exists());

        enable_nudge(tmp.path(), "triage").unwrap();
        assert!(!nudge_disabled_marker_path(tmp.path(), "triage").exists());
        assert!(nudge_disabled_marker_path(tmp.path(), "review").exists());
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

    #[cfg(unix)]
    fn write_daemon_script(script_path: &Path, body: &str) {
        std::fs::write(script_path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn graceful_daemon_shutdown_waits_for_clean_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = daemon_state_path(tmp.path());
        let state_dir = state_path.parent().unwrap();
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(&state_path, r#"{"clean_shutdown":false,"saved_at":1}"#).unwrap();

        let state_path_for_thread = state_path.clone();
        let state_dir_for_thread = state_dir.to_path_buf();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            std::fs::create_dir_all(&state_dir_for_thread).unwrap();
            std::fs::write(
                &state_path_for_thread,
                r#"{"clean_shutdown":true,"saved_at":2}"#,
            )
            .unwrap();
        });

        assert!(wait_for_graceful_daemon_shutdown(
            tmp.path(),
            std::process::id(),
            Some(1),
            Duration::from_secs(2)
        ));

        writer.join().unwrap();
        assert!(daemon_state_indicates_clean_shutdown(tmp.path(), Some(1)));
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn graceful_daemon_shutdown_times_out_before_force_kill_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("stubborn-daemon.sh");
        write_daemon_script(
            &script_path,
            "#!/bin/sh\ntrap '' TERM\nwhile :; do :; done\n",
        );

        let mut child = std::process::Command::new(&script_path).spawn().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        std::fs::write(daemon_pid_path(tmp.path()), child.id().to_string()).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        assert!(!request_graceful_daemon_shutdown(
            tmp.path(),
            Duration::from_millis(300)
        ));
        assert!(daemon_process_exists(child.id()));

        force_kill_daemon(tmp.path());
        let _ = child.wait().unwrap();
        assert!(!daemon_pid_path(tmp.path()).exists());
    }

    #[test]
    fn test_rotate_log_shifts_files() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = daemon_log_path(tmp.path());
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, b"current").unwrap();
        std::fs::write(rotated_log_path(&log_path, 1), b"older-1").unwrap();
        std::fs::write(rotated_log_path(&log_path, 2), b"older-2").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&log_path)
            .unwrap()
            .set_len(LOG_ROTATION_BYTES + 1)
            .unwrap();

        rotate_log_if_needed(&log_path).unwrap();

        assert!(!log_path.exists());
        assert_eq!(
            std::fs::read(rotated_log_path(&log_path, 1)).unwrap().len() as u64,
            LOG_ROTATION_BYTES + 1
        );
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&log_path, 2)).unwrap(),
            "older-1"
        );
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&log_path, 3)).unwrap(),
            "older-2"
        );
    }

    #[test]
    fn test_rotate_log_keeps_max_3() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = orchestrator_log_path(tmp.path());
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, b"current").unwrap();
        std::fs::write(rotated_log_path(&log_path, 1), b"older-1").unwrap();
        std::fs::write(rotated_log_path(&log_path, 2), b"older-2").unwrap();
        std::fs::write(rotated_log_path(&log_path, 3), b"older-3").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&log_path)
            .unwrap()
            .set_len(LOG_ROTATION_BYTES + 1)
            .unwrap();

        rotate_log_if_needed(&log_path).unwrap();

        assert_eq!(
            std::fs::read(rotated_log_path(&log_path, 1)).unwrap().len() as u64,
            LOG_ROTATION_BYTES + 1
        );
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&log_path, 2)).unwrap(),
            "older-1"
        );
        assert_eq!(
            std::fs::read_to_string(rotated_log_path(&log_path, 3)).unwrap(),
            "older-2"
        );
        assert!(!rotated_log_path(&log_path, 4).exists());
    }

    #[test]
    fn test_rotate_log_noop_under_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = daemon_log_path(tmp.path());
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, b"small-log").unwrap();

        rotate_log_if_needed(&log_path).unwrap();

        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "small-log");
        assert!(!rotated_log_path(&log_path, 1).exists());
    }

    #[test]
    fn test_daemon_log_append_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = daemon_log_path(tmp.path());

        {
            let mut file = open_log_for_append(&log_path).unwrap();
            use std::io::Write;
            writeln!(file, "first").unwrap();
        }

        {
            let mut file = open_log_for_append(&log_path).unwrap();
            use std::io::Write;
            writeln!(file, "second").unwrap();
        }

        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            "first\nsecond\n"
        );
    }

    #[test]
    fn daemon_spawn_args_include_verbose_and_resume() {
        assert_eq!(
            daemon_spawn_args("/tmp/project", false),
            vec![
                "-v".to_string(),
                "daemon".to_string(),
                "--project-root".to_string(),
                "/tmp/project".to_string()
            ]
        );
        assert_eq!(
            daemon_spawn_args("/tmp/project", true),
            vec![
                "-v".to_string(),
                "daemon".to_string(),
                "--project-root".to_string(),
                "/tmp/project".to_string(),
                "--resume".to_string()
            ]
        );
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
        assert_eq!(status::strip_tmux_style(raw), "idle nudge 1:05");
    }

    #[test]
    fn summarize_runtime_member_status_extracts_state_and_signal() {
        let summary = status::summarize_runtime_member_status(
            "#[fg=cyan]working#[default] #[fg=blue]standup 4:12#[default]",
            false,
        );

        assert_eq!(summary.state, "working");
        assert_eq!(summary.signal.as_deref(), Some("standup"));
        assert_eq!(summary.label.as_deref(), Some("working standup 4:12"));
    }

    #[test]
    fn summarize_runtime_member_status_marks_nudge_and_standup_together() {
        let summary = status::summarize_runtime_member_status(
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
        let summary = status::summarize_runtime_member_status(
            "#[fg=yellow]idle#[default] #[fg=magenta]nudge sent#[default]",
            false,
        );

        assert_eq!(summary.state, "idle");
        assert_eq!(summary.signal.as_deref(), Some("nudged"));
        assert_eq!(summary.label.as_deref(), Some("idle nudge sent"));
    }

    #[test]
    fn summarize_runtime_member_status_tracks_paused_automation() {
        let summary = status::summarize_runtime_member_status(
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
            status::OwnedTaskBuckets {
                active: vec![191u32],
                review: vec![193u32],
            },
        )]);
        let rows = status::build_team_status_rows(
            &[architect.clone(), human.clone()],
            false,
            &Default::default(),
            &pending,
            &triage,
            &owned,
            &Default::default(),
        );
        assert_eq!(rows[0].state, "stopped");
        assert_eq!(rows[0].pending_inbox, 3);
        assert_eq!(rows[0].triage_backlog, 2);
        assert_eq!(rows[0].active_owned_tasks, vec![191]);
        assert_eq!(rows[0].review_owned_tasks, vec![193]);
        assert_eq!(rows[0].health_summary, "-");
        assert_eq!(rows[1].state, "user");
        assert_eq!(rows[1].pending_inbox, 1);
        assert_eq!(rows[1].triage_backlog, 0);
        assert!(rows[1].active_owned_tasks.is_empty());
        assert!(rows[1].review_owned_tasks.is_empty());

        let runtime = std::collections::HashMap::from([(
            architect.name.clone(),
            status::RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: Some("standup".to_string()),
                label: Some("idle standup 2:00".to_string()),
            },
        )]);
        let rows = status::build_team_status_rows(
            &[architect],
            true,
            &runtime,
            &pending,
            &triage,
            &owned,
            &Default::default(),
        );
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

        let triage_state = status::delivered_direct_report_triage_state_at(
            &root,
            "lead",
            &["eng-1".to_string(), "eng-2".to_string()],
            100,
        )
        .unwrap();
        assert_eq!(triage_state.count, 2);
        assert_eq!(triage_state.newest_result_ts, 40);
    }

    #[test]
    fn delivered_direct_report_triage_count_excludes_stale_delivered_results() {
        let tmp = tempfile::tempdir().unwrap();
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();

        let mut stale_result = inbox::InboxMessage::new_send("eng-1", "lead", "stale result");
        stale_result.timestamp = 10;
        let stale_result_id = inbox::deliver_to_inbox(&root, &stale_result).unwrap();
        inbox::mark_delivered(&root, "lead", &stale_result_id).unwrap();

        let triage_state = status::delivered_direct_report_triage_state_at(
            &root,
            "lead",
            &["eng-1".to_string()],
            10 + TRIAGE_RESULT_FRESHNESS_SECONDS + 1,
        )
        .unwrap();

        assert_eq!(triage_state.count, 0);
        assert_eq!(triage_state.newest_result_ts, 0);
    }

    #[test]
    fn delivered_direct_report_triage_count_keeps_fresh_delivered_results() {
        let tmp = tempfile::tempdir().unwrap();
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();

        let mut fresh_result = inbox::InboxMessage::new_send("eng-1", "lead", "fresh result");
        fresh_result.timestamp = 100;
        let fresh_result_id = inbox::deliver_to_inbox(&root, &fresh_result).unwrap();
        inbox::mark_delivered(&root, "lead", &fresh_result_id).unwrap();

        let triage_state = status::delivered_direct_report_triage_state_at(
            &root,
            "lead",
            &["eng-1".to_string()],
            150,
        )
        .unwrap();

        assert_eq!(triage_state.count, 1);
        assert_eq!(triage_state.newest_result_ts, 100);
    }

    #[test]
    fn delivered_direct_report_triage_count_excludes_acked_results() {
        let tmp = tempfile::tempdir().unwrap();
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();

        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "task complete");
        result.timestamp = 100;
        let result_id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &result_id).unwrap();

        let mut lead_reply = inbox::InboxMessage::new_send("lead", "eng-1", "acknowledged");
        lead_reply.timestamp = 110;
        let lead_reply_id = inbox::deliver_to_inbox(&root, &lead_reply).unwrap();
        inbox::mark_delivered(&root, "eng-1", &lead_reply_id).unwrap();

        let triage_state = status::delivered_direct_report_triage_state_at(
            &root,
            "lead",
            &["eng-1".to_string()],
            150,
        )
        .unwrap();

        assert_eq!(triage_state.count, 0);
        assert_eq!(triage_state.newest_result_ts, 0);
    }

    #[test]
    fn format_owned_tasks_summary_compacts_multiple_ids() {
        assert_eq!(status::format_owned_tasks_summary(&[]), "-");
        assert_eq!(status::format_owned_tasks_summary(&[191]), "#191");
        assert_eq!(status::format_owned_tasks_summary(&[191, 192]), "#191,#192");
        assert_eq!(
            status::format_owned_tasks_summary(&[191, 192, 193]),
            "#191,#192,+1"
        );
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

        let owned = status::owned_task_buckets(tmp.path(), &members);
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
        assert!(status::workflow_metrics_enabled(&config_path));

        std::fs::write(
            &config_path,
            "name: test\nworkflow_mode: workflow_first\nroles: []\n",
        )
        .unwrap();
        assert!(status::workflow_metrics_enabled(&config_path));

        std::fs::write(&config_path, "name: test\nroles: []\n").unwrap();
        assert!(!status::workflow_metrics_enabled(&config_path));
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
        let section = status::workflow_metrics_section(tmp.path(), &members).unwrap();

        assert!(section.0.contains("Workflow Metrics"));
        assert_eq!(section.1.runnable_count, 1);
        assert_eq!(section.1.idle_with_runnable, vec!["eng-1-1"]);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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

        let statuses = status::list_runtime_member_statuses(session).unwrap();
        let eng = statuses.get("eng-1").unwrap();
        assert_eq!(eng.state, "idle");
        assert_eq!(eng.signal.as_deref(), Some("waiting for nudge"));
        assert_eq!(eng.label.as_deref(), Some("idle nudge 0:30"));

        crate::tmux::kill_session(session).unwrap();
    }

    /// Count unwrap()/expect() calls in production code (before `#[cfg(test)] mod tests`).
    fn production_unwrap_expect_count(source: &str) -> usize {
        // Split at the test module boundary, not individual #[cfg(test)] items
        let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
            &source[..pos]
        } else {
            source
        };
        prod.lines()
            .filter(|line| {
                let trimmed = line.trim();
                // Skip lines that are themselves cfg(test)-gated items
                !trimmed.starts_with("#[cfg(test)]")
                    && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
            })
            .count()
    }

    #[test]
    fn production_mod_has_no_unwrap_or_expect_calls() {
        let src = include_str!("mod.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production mod.rs should avoid unwrap/expect"
        );
    }

    // --- Session summary tests ---

    #[test]
    fn session_summary_counts_completions_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let events_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&events_dir).unwrap();

        let now = now_unix();
        let events = [
            format!(r#"{{"event":"daemon_started","ts":{}}}"#, now - 3600),
            format!(
                r#"{{"event":"task_completed","role":"eng-1","task":"10","ts":{}}}"#,
                now - 3000
            ),
            format!(
                r#"{{"event":"task_completed","role":"eng-2","task":"11","ts":{}}}"#,
                now - 2000
            ),
            format!(
                r#"{{"event":"task_auto_merged","role":"eng-1","task":"10","ts":{}}}"#,
                now - 2900
            ),
            format!(
                r#"{{"event":"task_manual_merged","role":"eng-2","task":"11","ts":{}}}"#,
                now - 1900
            ),
            format!(
                r#"{{"event":"task_completed","role":"eng-1","task":"12","ts":{}}}"#,
                now - 1000
            ),
        ];
        std::fs::write(events_dir.join("events.jsonl"), events.join("\n")).unwrap();

        let summary = compute_session_summary(tmp.path()).unwrap();
        assert_eq!(summary.tasks_completed, 3);
        assert_eq!(summary.tasks_merged, 2);
        assert!(summary.runtime_secs >= 3599 && summary.runtime_secs <= 3601);
    }

    #[test]
    fn session_summary_calculates_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let events_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&events_dir).unwrap();

        let now = now_unix();
        let events = [format!(
            r#"{{"event":"daemon_started","ts":{}}}"#,
            now - 7200
        )];
        std::fs::write(events_dir.join("events.jsonl"), events.join("\n")).unwrap();

        let summary = compute_session_summary(tmp.path()).unwrap();
        assert_eq!(summary.tasks_completed, 0);
        assert_eq!(summary.tasks_merged, 0);
        assert!(summary.runtime_secs >= 7199 && summary.runtime_secs <= 7201);
    }

    #[test]
    fn session_summary_handles_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let events_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&events_dir).unwrap();

        // No daemon_started event — summary returns None.
        std::fs::write(events_dir.join("events.jsonl"), "").unwrap();
        assert!(compute_session_summary(tmp.path()).is_none());
    }

    #[test]
    fn session_summary_handles_missing_events_file() {
        let tmp = tempfile::tempdir().unwrap();
        // No events.jsonl at all.
        assert!(compute_session_summary(tmp.path()).is_none());
    }

    #[test]
    fn session_summary_display_format() {
        let summary = SessionSummary {
            tasks_completed: 5,
            tasks_merged: 4,
            runtime_secs: 8100, // 2h 15m
        };
        assert_eq!(
            summary.display(),
            "Session summary: 5 tasks completed, 4 merged, runtime 2h 15m"
        );
    }

    #[test]
    fn format_runtime_seconds() {
        assert_eq!(format_runtime(45), "45s");
    }

    #[test]
    fn format_runtime_minutes() {
        assert_eq!(format_runtime(300), "5m");
    }

    #[test]
    fn format_runtime_hours_and_minutes() {
        assert_eq!(format_runtime(5400), "1h 30m");
    }

    #[test]
    fn format_runtime_exact_hours() {
        assert_eq!(format_runtime(7200), "2h");
    }

    #[test]
    fn session_summary_uses_latest_daemon_started() {
        let tmp = tempfile::tempdir().unwrap();
        let events_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&events_dir).unwrap();

        let now = now_unix();
        // First session had 2 completions, second session has 1.
        let events = [
            format!(r#"{{"event":"daemon_started","ts":{}}}"#, now - 7200),
            format!(
                r#"{{"event":"task_completed","role":"eng-1","task":"1","ts":{}}}"#,
                now - 6000
            ),
            format!(
                r#"{{"event":"task_completed","role":"eng-1","task":"2","ts":{}}}"#,
                now - 5000
            ),
            format!(r#"{{"event":"daemon_started","ts":{}}}"#, now - 1800),
            format!(
                r#"{{"event":"task_completed","role":"eng-1","task":"3","ts":{}}}"#,
                now - 1000
            ),
        ];
        std::fs::write(events_dir.join("events.jsonl"), events.join("\n")).unwrap();

        let summary = compute_session_summary(tmp.path()).unwrap();
        // Should only count events from the latest daemon_started.
        assert_eq!(summary.tasks_completed, 1);
        assert!(summary.runtime_secs >= 1799 && summary.runtime_secs <= 1801);
    }

}
