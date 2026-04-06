//! Daemon management: spawning, PID tracking, signal handling, log rotation,
//! graceful shutdown, and team start / daemon entry point.
//!
//! Extracted from `lifecycle.rs` — pure refactor, zero logic changes.

use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{config, daemon, events, hierarchy, inbox, layout, team_config_path};
use crate::tmux;

pub(crate) const LOG_ROTATION_BYTES: u64 = 5 * 1024 * 1024;
const LOG_ROTATION_KEEP: usize = 3;
pub(super) const DAEMON_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const DAEMON_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(200);
const WATCHDOG_INITIAL_BACKOFF_SECS: u64 = 1;
const WATCHDOG_MAX_BACKOFF_SECS: u64 = 30;
const WATCHDOG_CIRCUIT_BREAKER_THRESHOLD: usize = 5;
const WATCHDOG_CIRCUIT_BREAKER_WINDOW_SECS: u64 = 60;
const DAEMON_CHILD_PID_FILE: &str = "daemon-child.pid";

#[cfg(unix)]
static WATCHDOG_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

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

fn watchdog_spawn_args(root_str: &str, resume: bool) -> Vec<String> {
    let mut args = vec![
        "-v".to_string(),
        "watchdog".to_string(),
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

pub(crate) fn watchdog_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("watchdog-state.json")
}

fn daemon_child_pid_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join(DAEMON_CHILD_PID_FILE)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PersistedWatchdogState {
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub crash_timestamps: Vec<u64>,
    #[serde(default)]
    pub circuit_breaker_tripped: bool,
    #[serde(default)]
    pub child_pid: Option<u32>,
    #[serde(default)]
    pub current_backoff_secs: Option<u64>,
    #[serde(default)]
    pub last_exit_reason: Option<String>,
}

fn load_watchdog_state(project_root: &Path) -> Result<PersistedWatchdogState> {
    let path = watchdog_state_path(project_root);
    if !path.exists() {
        return Ok(PersistedWatchdogState::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_watchdog_state(project_root: &Path, state: &PersistedWatchdogState) -> Result<()> {
    let path = watchdog_state_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize watchdog state")?;
    std::fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
}

/// Spawn the daemon as a detached background process.
///
/// The daemon runs in its own process group with stdio redirected to a log
/// file, so it survives terminal closure. PID is saved to `.batty/daemon.pid`.
fn spawn_detached_process(
    project_root: &Path,
    args: &[String],
    pid_path: &Path,
    process_name: &str,
) -> Result<u32> {
    use std::process::{Command, Stdio};

    let log_path = daemon_log_path(project_root);

    // Ensure .batty/ exists
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let log_file = open_log_for_append(&log_path)?;
    let log_err = log_file
        .try_clone()
        .context("failed to clone log file handle")?;

    let exe = std::env::current_exe().context("failed to resolve current executable")?;

    let mut cmd = Command::new(exe);
    cmd.args(args)
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
                    "{process_name} process exited immediately with {status}\n\n\
                     {detail}\n\n\
                     see full log: {log}",
                    log = log_path.display(),
                ),
                None => bail!(
                    "{process_name} process exited immediately with {status}; \
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

    info!(pid, log = %log_path.display(), process = process_name, "background process spawned");
    Ok(pid)
}

fn spawn_watchdog(project_root: &Path, resume: bool) -> Result<u32> {
    let root_str = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    let args = watchdog_spawn_args(&root_str, resume);
    spawn_detached_process(
        project_root,
        &args,
        &daemon_pid_path(project_root),
        "watchdog",
    )
}

fn spawn_daemon_child(project_root: &Path, resume: bool) -> Result<std::process::Child> {
    use std::process::Command;

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let root_str = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();

    let mut cmd = Command::new(exe);
    cmd.args(daemon_spawn_args(&root_str, resume));
    cmd.spawn().context("failed to spawn daemon child")
}

#[cfg(unix)]
extern "C" fn handle_watchdog_shutdown_signal(_signal: libc::c_int) {
    WATCHDOG_SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_watchdog_signal_handlers() -> Result<()> {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_watchdog_shutdown_signal as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_watchdog_shutdown_signal as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            handle_watchdog_shutdown_signal as libc::sighandler_t,
        );
    }
    WATCHDOG_SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    Ok(())
}

#[cfg(not(unix))]
fn install_watchdog_signal_handlers() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn watchdog_shutdown_requested() -> bool {
    WATCHDOG_SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
fn watchdog_shutdown_requested() -> bool {
    false
}

fn record_watchdog_crash(
    project_root: &Path,
    state: &mut PersistedWatchdogState,
    reason: String,
) -> Result<Option<u64>> {
    let now = super::now_unix();
    state.restart_count += 1;
    state.last_exit_reason = Some(reason);
    state.child_pid = None;
    state
        .crash_timestamps
        .retain(|ts| now.saturating_sub(*ts) < WATCHDOG_CIRCUIT_BREAKER_WINDOW_SECS);
    state.crash_timestamps.push(now);

    if state.crash_timestamps.len() >= WATCHDOG_CIRCUIT_BREAKER_THRESHOLD {
        state.circuit_breaker_tripped = true;
        state.current_backoff_secs = None;
        save_watchdog_state(project_root, state)?;
        return Ok(None);
    }

    let exponent = state.crash_timestamps.len().saturating_sub(1) as u32;
    let backoff_secs = (WATCHDOG_INITIAL_BACKOFF_SECS
        .saturating_mul(2u64.saturating_pow(exponent)))
    .min(WATCHDOG_MAX_BACKOFF_SECS);
    state.current_backoff_secs = Some(backoff_secs);
    save_watchdog_state(project_root, state)?;
    Ok(Some(backoff_secs))
}

fn clear_watchdog_child_pid(project_root: &Path, state: &mut PersistedWatchdogState) -> Result<()> {
    state.child_pid = None;
    let _ = std::fs::remove_file(daemon_child_pid_path(project_root));
    save_watchdog_state(project_root, state)
}

fn terminate_daemon_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = send_unix_signal(child.id(), libc::SIGTERM);
    }

    let deadline = std::time::Instant::now() + DAEMON_SHUTDOWN_GRACE_PERIOD;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(DAEMON_SHUTDOWN_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                #[cfg(unix)]
                {
                    let _ = send_unix_signal(child.id(), libc::SIGKILL);
                }
                let _ = child.wait();
                return;
            }
        }
    }
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

pub(super) fn request_graceful_daemon_shutdown(project_root: &Path, timeout: Duration) -> bool {
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

pub(super) fn force_kill_daemon(project_root: &Path) {
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

    // Spawn watchdog as a detached background process. It supervises the daemon
    // child and handles crash backoff/restart policy.
    let pid = spawn_watchdog(project_root, resume)?;
    info!(pid, "watchdog process launched");

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

pub fn run_watchdog(project_root: &Path, resume: bool) -> Result<()> {
    install_watchdog_signal_handlers()?;

    let mut state = load_watchdog_state(project_root).unwrap_or_default();
    state.circuit_breaker_tripped = false;
    state.current_backoff_secs = None;
    state.last_exit_reason = None;
    state.child_pid = None;
    save_watchdog_state(project_root, &state)?;

    let mut resume_on_launch = resume;

    loop {
        if watchdog_shutdown_requested() {
            let _ = std::fs::remove_file(daemon_pid_path(project_root));
            let _ = std::fs::remove_file(daemon_child_pid_path(project_root));
            state.child_pid = None;
            state.current_backoff_secs = None;
            save_watchdog_state(project_root, &state)?;
            return Ok(());
        }

        let mut child = spawn_daemon_child(project_root, resume_on_launch)?;
        resume_on_launch = true;

        state.child_pid = Some(child.id());
        state.current_backoff_secs = None;
        save_watchdog_state(project_root, &state)?;
        std::fs::write(daemon_child_pid_path(project_root), child.id().to_string()).with_context(
            || {
                format!(
                    "failed to write child PID file: {}",
                    daemon_child_pid_path(project_root).display()
                )
            },
        )?;

        loop {
            if watchdog_shutdown_requested() {
                terminate_daemon_child(&mut child);
                let _ = std::fs::remove_file(daemon_pid_path(project_root));
                clear_watchdog_child_pid(project_root, &mut state)?;
                return Ok(());
            }

            match child.try_wait() {
                Ok(Some(exit_status)) => {
                    clear_watchdog_child_pid(project_root, &mut state)?;
                    let reason = if let Some(code) = exit_status.code() {
                        format!("daemon exited with status {code}")
                    } else {
                        "daemon exited from signal".to_string()
                    };
                    if let Some(backoff_secs) =
                        record_watchdog_crash(project_root, &mut state, reason.clone())?
                    {
                        warn!(backoff_secs, reason = %reason, "daemon crashed; watchdog restarting with backoff");
                        std::thread::sleep(Duration::from_secs(backoff_secs));
                        break;
                    }

                    warn!(
                        reason = %reason,
                        threshold = WATCHDOG_CIRCUIT_BREAKER_THRESHOLD,
                        window_secs = WATCHDOG_CIRCUIT_BREAKER_WINDOW_SECS,
                        "watchdog circuit breaker tripped; daemon will not be restarted"
                    );
                    let _ = std::fs::remove_file(daemon_pid_path(project_root));
                    return Ok(());
                }
                Ok(None) => std::thread::sleep(WATCHDOG_POLL_INTERVAL),
                Err(error) => {
                    clear_watchdog_child_pid(project_root, &mut state)?;
                    let reason = format!("failed to poll daemon child: {error}");
                    if let Some(backoff_secs) =
                        record_watchdog_crash(project_root, &mut state, reason.clone())?
                    {
                        warn!(backoff_secs, reason = %reason, "watchdog poll failed; retrying daemon launch");
                        std::thread::sleep(Duration::from_secs(backoff_secs));
                        break;
                    }
                    warn!(
                        reason = %reason,
                        threshold = WATCHDOG_CIRCUIT_BREAKER_THRESHOLD,
                        window_secs = WATCHDOG_CIRCUIT_BREAKER_WINDOW_SECS,
                        "watchdog circuit breaker tripped after daemon poll failures"
                    );
                    let _ = std::fs::remove_file(daemon_pid_path(project_root));
                    return Ok(());
                }
            }
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
pub(super) fn resume_marker_path(project_root: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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
        let log_path = crate::team::orchestrator_log_path(tmp.path());
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

    #[test]
    fn watchdog_spawn_args_include_verbose_and_resume() {
        assert_eq!(
            watchdog_spawn_args("/tmp/project", false),
            vec![
                "-v".to_string(),
                "watchdog".to_string(),
                "--project-root".to_string(),
                "/tmp/project".to_string()
            ]
        );
        assert_eq!(
            watchdog_spawn_args("/tmp/project", true),
            vec![
                "-v".to_string(),
                "watchdog".to_string(),
                "--project-root".to_string(),
                "/tmp/project".to_string(),
                "--resume".to_string()
            ]
        );
    }

    #[test]
    fn record_watchdog_crash_applies_exponential_backoff_until_circuit_breaker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = PersistedWatchdogState::default();

        assert_eq!(
            record_watchdog_crash(tmp.path(), &mut state, "boom-1".to_string()).unwrap(),
            Some(1)
        );
        assert_eq!(
            record_watchdog_crash(tmp.path(), &mut state, "boom-2".to_string()).unwrap(),
            Some(2)
        );
        assert_eq!(
            record_watchdog_crash(tmp.path(), &mut state, "boom-3".to_string()).unwrap(),
            Some(4)
        );
        assert_eq!(
            record_watchdog_crash(tmp.path(), &mut state, "boom-4".to_string()).unwrap(),
            Some(8)
        );
        assert_eq!(
            record_watchdog_crash(tmp.path(), &mut state, "boom-5".to_string()).unwrap(),
            None
        );
        assert!(state.circuit_breaker_tripped);
        assert_eq!(state.restart_count, 5);
    }
}
