//! Telemetry helpers for daemon event emission and orchestrator logging.
//!
//! This module keeps `daemon.rs` focused on control flow by centralizing the
//! structured events and append-only orchestrator logging that the daemon emits
//! while it runs.

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use super::super::events::TeamEvent;
use super::*;

impl TeamDaemon {
    /// Run a critical subsystem step. Errors are logged but no consecutive-failure
    /// tracking is applied — critical subsystems must not be silently degraded.
    pub(super) fn run_loop_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        if let Err(error) = action(self) {
            self.record_loop_step_error(step, &error.to_string());
        }
    }

    /// Run a recoverable subsystem step. Errors are logged, and consecutive
    /// failures are tracked. After 3+ consecutive failures an escalation WARN
    /// is emitted each cycle.
    pub(super) fn run_recoverable_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        match action(self) {
            Ok(()) => {
                self.subsystem_error_counts.remove(step);
            }
            Err(error) => {
                self.record_loop_step_error(step, &error.to_string());
                self.increment_subsystem_error(step);
            }
        }
    }

    /// Run a recoverable subsystem step with panic protection via
    /// `catch_unwind`. Used for subsystems (standup, retro, telegram) where a
    /// panic must not crash the daemon.
    pub(super) fn run_recoverable_step_with_catch_unwind<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(self)));
        match result {
            Ok(Ok(())) => {
                self.subsystem_error_counts.remove(step);
            }
            Ok(Err(error)) => {
                self.record_loop_step_error(step, &error.to_string());
                self.increment_subsystem_error(step);
            }
            Err(panic_payload) => {
                let msg = panic_payload_to_string(&panic_payload);
                self.record_loop_step_error(step, &format!("panic: {msg}"));
                self.increment_subsystem_error(step);
            }
        }
    }

    fn increment_subsystem_error(&mut self, step: &str) {
        let count = self
            .subsystem_error_counts
            .entry(step.to_string())
            .or_insert(0);
        *count += 1;
        if *count >= 3 {
            warn!(
                subsystem = step,
                consecutive_failures = *count,
                "subsystem {step} failing repeatedly"
            );
        }
    }

    pub(super) fn emit_event(&mut self, event: TeamEvent) {
        self.failure_tracker.push(&event);
        if let Err(error) = self.event_sink.emit(event) {
            warn!(error = %error, "failed to write daemon event; continuing");
        }
    }

    pub(super) fn record_daemon_started(&mut self) {
        self.emit_event(TeamEvent::daemon_started());
    }

    pub(super) fn record_daemon_heartbeat(&mut self, uptime_secs: u64) {
        self.emit_event(TeamEvent::daemon_heartbeat(uptime_secs));
    }

    pub(super) fn record_daemon_stopped(&mut self, reason: &str, uptime_secs: u64) {
        self.emit_event(TeamEvent::daemon_stopped_with_reason(reason, uptime_secs));
        info!(reason = reason, uptime_secs = uptime_secs, "daemon stopped");
    }

    pub(super) fn record_loop_step_error(&mut self, step: &str, error: &str) {
        warn!(step, error = %error, "daemon loop step failed; continuing");
        self.emit_event(TeamEvent::loop_step_error(step, error));
    }

    pub(super) fn record_daemon_reloading(&mut self) {
        self.emit_event(TeamEvent::daemon_reloading());
    }

    pub(super) fn record_daemon_reloaded(&mut self) {
        self.emit_event(TeamEvent::daemon_reloaded());
    }

    pub(super) fn record_pattern_detected(&mut self, pattern_type: &str, frequency: u32) {
        self.emit_event(TeamEvent::pattern_detected(pattern_type, frequency));
    }

    pub(super) fn record_agent_spawned(&mut self, role: &str) {
        self.emit_event(TeamEvent::agent_spawned(role));
    }

    pub(super) fn record_member_crashed(&mut self, role: &str, restart: bool) {
        self.emit_event(TeamEvent::member_crashed(role, restart));
    }

    pub(super) fn record_agent_restarted(
        &mut self,
        role: &str,
        task: impl Into<String>,
        reason: &str,
        restart_count: u32,
    ) {
        let task = task.into();
        self.emit_event(TeamEvent::agent_restarted(
            role,
            &task,
            reason,
            restart_count,
        ));
    }

    pub(super) fn record_context_exhausted(
        &mut self,
        role: &str,
        task: Option<u32>,
        session_size_bytes: Option<u64>,
    ) {
        self.emit_event(TeamEvent::context_exhausted(role, task, session_size_bytes));
    }

    pub(crate) fn record_delivery_failed(&mut self, role: &str, from: &str, reason: &str) {
        self.emit_event(TeamEvent::delivery_failed(role, from, reason));
    }

    pub(crate) fn record_task_escalated(&mut self, role: &str, task: impl Into<String>) {
        let task = task.into();
        self.emit_event(TeamEvent::task_escalated(role, &task));
    }

    pub(super) fn record_task_unblocked(&mut self, role: &str, task: impl Into<String>) {
        let task = task.into();
        self.emit_event(TeamEvent::task_unblocked(role, &task));
    }

    pub(crate) fn record_performance_regression(&mut self, task: impl Into<String>, reason: &str) {
        let task = task.into();
        self.emit_event(TeamEvent::performance_regression(&task, reason));
    }

    pub(crate) fn record_task_completed(&mut self, role: &str) {
        self.emit_event(TeamEvent::task_completed(role));
    }

    pub(super) fn record_standup_generated(&mut self, recipient: &str) {
        self.emit_event(TeamEvent::standup_generated(recipient));
    }

    pub(super) fn record_retro_generated(&mut self) {
        self.emit_event(TeamEvent::retro_generated());
    }

    pub(crate) fn record_message_routed(&mut self, from: &str, to: &str) {
        self.emit_event(TeamEvent::message_routed(from, to));
    }

    pub(super) fn acknowledge_hot_reload_marker(&mut self) {
        if !consume_hot_reload_marker(&self.config.project_root) {
            return;
        }

        self.record_daemon_reloaded();
        self.record_orchestrator_action("runtime: daemon hot-reloaded");
        info!("daemon restarted via hot reload");
    }

    pub(super) fn maybe_notify_failure_patterns(&mut self) -> Result<()> {
        if !self.config.team_config.automation.failure_pattern_detection {
            return Ok(());
        }

        for notification in self.failure_tracker.pattern_notifications(3, 5) {
            let managers: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| member.role_type == RoleType::Manager)
                .map(|member| member.name.clone())
                .collect();
            let architects: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| member.role_type == RoleType::Architect)
                .map(|member| member.name.clone())
                .collect();

            self.record_pattern_detected(
                notification.pattern_type.as_str(),
                notification.frequency,
            );

            if notification.notify_manager {
                for recipient in &managers {
                    self.queue_daemon_message(recipient, &notification.message)?;
                }
            }

            if notification.notify_architect {
                for recipient in &architects {
                    self.queue_daemon_message(recipient, &notification.message)?;
                }
            }
        }

        Ok(())
    }

    fn orchestrator_enabled(&self) -> bool {
        self.config.team_config.orchestrator_enabled()
    }

    pub(super) fn record_orchestrator_action(&self, action: impl AsRef<str>) {
        if !self.orchestrator_enabled() {
            return;
        }
        let plain_path = super::super::orchestrator_log_path(&self.config.project_root);
        let ansi_path = super::super::orchestrator_ansi_log_path(&self.config.project_root);
        if let Err(error) = append_orchestrator_log_line(&plain_path, &ansi_path, action.as_ref()) {
            warn!(log = %plain_path.display(), error = %error, "failed to append orchestrator log");
        }
    }
}

fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

pub(super) fn append_orchestrator_log_line(
    plain_path: &Path,
    ansi_path: &Path,
    message: &str,
) -> Result<()> {
    use std::io::Write;

    let timestamp = now_local_datetime();
    let plain_line = format_orchestrator_line(&timestamp, message, false);
    let ansi_line = format_orchestrator_line(&timestamp, message, true);

    let mut plain_file = super::super::open_log_for_append(plain_path)?;
    writeln!(plain_file, "{plain_line}")?;
    plain_file.flush()?;

    let mut ansi_file = super::super::open_log_for_append(ansi_path)?;
    writeln!(ansi_file, "{ansi_line}")?;
    ansi_file.flush()?;
    Ok(())
}

fn format_orchestrator_line(timestamp: &str, message: &str, ansi: bool) -> String {
    if !ansi {
        return format!("[{timestamp}] {message}");
    }

    const RESET: &str = "\x1b[0m";
    const DIM: &str = "\x1b[2;90m";
    const BOLD: &str = "\x1b[1m";
    const AMBER: &str = "\x1b[1;33m";
    const CYAN: &str = "\x1b[1;36m";
    const GREEN: &str = "\x1b[1;32m";
    const RED: &str = "\x1b[1;31m";
    const MAGENTA: &str = "\x1b[1;35m";
    const BLUE: &str = "\x1b[1;34m";
    const HILITE: &str = "\x1b[4;97m";

    let lower = message.to_ascii_lowercase();
    let (label, color) = if lower.contains("triage") {
        ("TRIAGE", AMBER)
    } else if lower.contains("review") {
        ("REVIEW", CYAN)
    } else if lower.contains("dispatch") {
        ("DISPATCH", GREEN)
    } else if lower.contains("escalat") {
        ("ESCALATION", RED)
    } else if lower.contains("utilization") {
        ("UTILIZATION", MAGENTA)
    } else if lower.contains("replenishment") {
        ("REPLENISH", BLUE)
    } else if lower.contains("runtime") || lower.contains("restart") || lower.contains("resume") {
        ("RUNTIME", BOLD)
    } else {
        ("ACTION", BOLD)
    };

    let highlighted = highlight_references(message, HILITE, RESET);
    format!("{DIM}[{timestamp}]{RESET} {color}{label:>10}{RESET}  {highlighted}")
}

fn highlight_references(message: &str, start: &str, reset: &str) -> String {
    let mut result = String::with_capacity(message.len() + 32);
    for token in message.split(' ') {
        if !result.is_empty() {
            result.push(' ');
        }
        let trimmed = token.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | ')' | '('));
        let should_highlight = trimmed.starts_with("Task")
            || trimmed.starts_with('#')
            || trimmed.contains("eng-")
            || trimmed.contains("manager")
            || trimmed.contains("architect");
        if should_highlight {
            result.push_str(start);
            result.push_str(token);
            result.push_str(reset);
        } else {
            result.push_str(token);
        }
    }
    result
}

/// Format current local time as `YYYY-MM-DD HH:MM:SS` for human-readable logs.
fn now_local_datetime() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Convert to local time using libc
    #[cfg(unix)]
    {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
        )
    }
    #[cfg(not(unix))]
    {
        // Fallback to UTC on non-unix
        let total_secs = secs;
        let days = total_secs / 86400;
        let day_secs = total_secs % 86400;
        let hours = day_secs / 3600;
        let mins = (day_secs % 3600) / 60;
        let secs_rem = day_secs % 60;
        // Approximate date from epoch days (good enough for log display)
        let _ = days; // suppress unused
        format!("{:02}:{:02}:{:02}Z", hours, mins, secs_rem)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::team::LOG_ROTATION_BYTES;
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleDef, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::{EventSink, read_events};
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::test_helpers::{RecordingChannel, daemon_config_with_roles};
    use regex::Regex;
    use serial_test::serial;

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic event sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic event sink failure"))
        }
    }

    fn daemon_for_orchestrator_logging(
        project_root: &std::path::Path,
        workflow_mode: WorkflowMode,
        orchestrator_pane: bool,
    ) -> TeamDaemon {
        TeamDaemon::new(DaemonConfig {
            project_root: project_root.to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap()
    }

    #[test]
    fn queue_daemon_message_ignores_event_sink_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::from_writer(
                tmp.path().join("broken-events.jsonl").as_path(),
                FailingWriter,
            ),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            subsystem_error_counts: HashMap::new(),
        };

        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Event sink can fail without breaking delivery.")
            .unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Event sink can fail without breaking delivery."]
        );
    }

    #[test]
    fn maybe_notify_failure_patterns_routes_severe_patterns_to_manager_and_architect() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("human".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("architect".to_string()),
                use_worktrees: false,
            },
        ];

        let mut daemon = TeamDaemon::new(config).unwrap();
        for index in 0..5 {
            let mut event = TeamEvent::task_escalated("eng-1", &format!("{}", 100 + index));
            event.ts = index as u64 + 1;
            daemon.emit_event(event);
        }

        daemon.maybe_notify_failure_patterns().unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let manager_messages = inbox::pending_messages(&root, "manager").unwrap();
        let architect_messages = inbox::pending_messages(&root, "architect").unwrap();

        assert_eq!(manager_messages.len(), 1);
        assert_eq!(architect_messages.len(), 1);
        assert!(manager_messages[0].body.contains("Review blockers"));
        assert!(architect_messages[0].body.contains("Review blockers"));
    }

    #[test]
    fn append_orchestrator_log_line_writes_timestamped_activity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".batty").join("orchestrator.log");
        let ansi_path = tmp.path().join(".batty").join("orchestrator.ansi.log");
        append_orchestrator_log_line(&path, &ansi_path, "dispatch: assigned task #18").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let line = content.trim_end();
        let format =
            Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\] dispatch: assigned task #18$")
                .unwrap();
        assert!(format.is_match(line), "unexpected log line: {line}");
    }

    #[test]
    fn record_orchestrator_action_is_noop_when_orchestrator_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_for_orchestrator_logging(tmp.path(), WorkflowMode::Legacy, true);

        daemon.record_orchestrator_action("dispatch: no-op");

        assert!(!tmp.path().join(".batty").join("orchestrator.log").exists());
    }

    #[test]
    fn record_orchestrator_action_writes_when_orchestrator_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_for_orchestrator_logging(tmp.path(), WorkflowMode::Hybrid, true);

        daemon.record_orchestrator_action("dispatch: active");

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("dispatch: active"));
        let ansi =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.ansi.log")).unwrap();
        assert!(ansi.contains("\u{1b}["));
        assert!(ansi.contains("DISPATCH"));
    }

    #[test]
    fn append_orchestrator_log_line_writes_to_fresh_log_after_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".batty").join("orchestrator.log");
        let ansi_path = tmp.path().join(".batty").join("orchestrator.ansi.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "stale log entry\n").unwrap();
        fs::write(&ansi_path, "stale ansi entry\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(LOG_ROTATION_BYTES + 1)
            .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&ansi_path)
            .unwrap()
            .set_len(LOG_ROTATION_BYTES + 1)
            .unwrap();

        append_orchestrator_log_line(&path, &ansi_path, "dispatch: after rotation").unwrap();

        let rotated = fs::read_to_string(format!("{}.1", path.display())).unwrap();
        assert!(rotated.contains("stale log entry"));
        let ansi_rotated = fs::read_to_string(format!("{}.1", ansi_path.display())).unwrap();
        assert!(ansi_rotated.contains("stale ansi entry"));

        let fresh = fs::read_to_string(&path).unwrap();
        assert!(fresh.contains("dispatch: after rotation"));
        assert!(!fresh.contains("stale log entry"));
        let ansi_fresh = fs::read_to_string(&ansi_path).unwrap();
        assert!(ansi_fresh.contains("\u{1b}["));
        assert!(!ansi_fresh.contains("stale ansi entry"));
    }

    #[test]
    fn record_orchestrator_action_handles_malformed_log_path_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let malformed_path = tmp.path().join(".batty").join("orchestrator.log");
        fs::create_dir_all(&malformed_path).unwrap();
        let malformed_ansi_path = tmp.path().join(".batty").join("orchestrator.ansi.log");
        fs::create_dir_all(&malformed_ansi_path).unwrap();
        let daemon = daemon_for_orchestrator_logging(tmp.path(), WorkflowMode::Hybrid, true);

        daemon.record_orchestrator_action("dispatch: malformed");

        assert!(malformed_path.is_dir());
        assert!(malformed_ansi_path.is_dir());
        assert!(
            !tmp.path()
                .join(".batty")
                .join("orchestrator.log.1")
                .exists()
        );
    }

    #[test]
    fn record_orchestrator_action_appends_multiple_entries_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_for_orchestrator_logging(tmp.path(), WorkflowMode::Hybrid, true);

        daemon.record_orchestrator_action("dispatch: first");
        daemon.record_orchestrator_action("dispatch: second");

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let format =
            Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\] dispatch: (first|second)$")
                .unwrap();
        assert!(
            format.is_match(lines[0]),
            "unexpected first line: {}",
            lines[0]
        );
        assert!(
            format.is_match(lines[1]),
            "unexpected second line: {}",
            lines[1]
        );
        assert!(lines[0].ends_with("dispatch: first"));
        assert!(lines[1].ends_with("dispatch: second"));
    }

    #[test]
    fn format_orchestrator_line_includes_ansi_codes_for_triage() {
        let formatted = format_orchestrator_line(
            "2026-03-21 18:46:00",
            "recovery: triage intervention for eng-1 with 2 pending direct-report result(s)",
            true,
        );
        assert!(formatted.contains("\u{1b}["));
        assert!(formatted.contains("TRIAGE"));
        assert!(formatted.contains("eng-1"));
    }

    #[test]
    fn format_orchestrator_line_keeps_plain_text_fallback() {
        let formatted =
            format_orchestrator_line("2026-03-21 18:46:00", "dispatch: assigned task #18", false);
        assert_eq!(
            formatted,
            "[2026-03-21 18:46:00] dispatch: assigned task #18"
        );
        assert!(!formatted.contains("\u{1b}["));
    }

    #[test]
    fn hot_reload_acknowledgement_emits_event_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        write_hot_reload_marker(tmp.path()).unwrap();

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Hybrid,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon.acknowledge_hot_reload_marker();

        let events = read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| event.event == "daemon_reloaded"));

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("daemon hot-reloaded"));
        assert!(!hot_reload_marker_path(tmp.path()).exists());
    }

    #[test]
    #[serial]
    fn poll_watchers_emits_context_exhausted_event() {
        let session = format!("batty-test-context-exhausted-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        tmux::send_keys(&pane_id, "Conversation is too long to continue.", true).unwrap();
        tmux::send_keys(&pane_id, "prompt is too long", true).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let events_path = tmp.path().join("events.jsonl");
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: session.clone(),
                members: vec![engineer],
                pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
            },
            watchers: HashMap::from([(
                "eng-1".to_string(),
                SessionWatcher::new(&pane_id, "eng-1", 300, None),
            )]),
            states: HashMap::from([("eng-1".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::from([("eng-1".to_string(), 42)]),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            subsystem_error_counts: HashMap::new(),
        };

        daemon.poll_watchers().unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
        let events = read_events(&events_path).unwrap();
        let event = events
            .iter()
            .find(|event| event.event == "context_exhausted")
            .unwrap();
        assert_eq!(event.role.as_deref(), Some("eng-1"));
        assert_eq!(event.task.as_deref(), Some("42"));

        let _ = crate::tmux::kill_session(&session);
    }

    #[test]
    fn recoverable_subsystem_failure_does_not_crash_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Simulate a recoverable subsystem that always fails
        daemon.run_recoverable_step("maybe_generate_standup", |_daemon| {
            anyhow::bail!("standup generation failed: board file missing")
        });

        // Daemon should still be alive — verify by running another step successfully
        let mut ran = false;
        daemon.run_recoverable_step("poll_watchers", |_daemon| {
            ran = true;
            Ok(())
        });
        assert!(ran, "daemon should continue after recoverable failure");

        // The failed step should have recorded one error
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            Some(&1)
        );
        // The successful step should have no error count
        assert_eq!(daemon.subsystem_error_counts.get("poll_watchers"), None);
    }

    #[test]
    fn subsystem_consecutive_failures_escalate() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Fail the same recoverable subsystem 3 times
        for i in 0..3 {
            daemon.run_recoverable_step("maybe_generate_standup", |_daemon| {
                anyhow::bail!("standup failure #{}", i + 1)
            });
        }

        // Verify escalation: consecutive count should be 3
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            Some(&3)
        );

        // A 4th failure should keep incrementing
        daemon.run_recoverable_step("maybe_generate_standup", |_daemon| {
            anyhow::bail!("standup failure #4")
        });
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            Some(&4)
        );

        // A success resets the counter
        daemon.run_recoverable_step("maybe_generate_standup", |_daemon| Ok(()));
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            None
        );
    }

    #[test]
    fn critical_subsystem_errors_propagate() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Critical steps use run_loop_step which logs errors but does NOT track them
        daemon.run_loop_step("deliver_inbox_messages", |_daemon| {
            anyhow::bail!("delivery failed: network error")
        });

        // Critical subsystem errors should NOT appear in subsystem_error_counts
        assert_eq!(
            daemon.subsystem_error_counts.get("deliver_inbox_messages"),
            None
        );

        // But the error should still be logged as a loop step error (via events)
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let step_error = events.iter().find(|event| event.event == "loop_step_error");
        assert!(
            step_error.is_some(),
            "critical subsystem error should still be logged as event"
        );
    }

    #[test]
    fn catch_unwind_recovers_from_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // A panic inside a catch_unwind-wrapped step should not crash
        daemon.run_recoverable_step_with_catch_unwind("process_telegram_queue", |_daemon| {
            panic!("telegram thread panicked")
        });

        // Daemon survived — verify error was tracked
        assert_eq!(
            daemon.subsystem_error_counts.get("process_telegram_queue"),
            Some(&1)
        );

        // Verify the error event was logged
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let step_error = events
            .iter()
            .find(|event| event.event == "loop_step_error")
            .expect("panic should be logged as loop_step_error");
        assert!(
            step_error.error.as_deref().unwrap_or("").contains("panic"),
            "error field should mention panic"
        );
    }

    #[test]
    fn consecutive_error_counter_resets_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Fail twice
        for _ in 0..2 {
            daemon.run_recoverable_step("maybe_rotate_board", |_daemon| {
                anyhow::bail!("board rotation failed")
            });
        }
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            Some(&2)
        );

        // One success should reset the counter
        daemon.run_recoverable_step("maybe_rotate_board", |_daemon| Ok(()));
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            None
        );

        // Next failure starts from 1 again
        daemon.run_recoverable_step("maybe_rotate_board", |_daemon| {
            anyhow::bail!("board rotation failed again")
        });
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            Some(&1)
        );
    }
}
