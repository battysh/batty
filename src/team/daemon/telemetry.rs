//! Telemetry helpers for daemon event emission and orchestrator logging.
//!
//! This module keeps `daemon.rs` focused on control flow by centralizing the
//! structured events and append-only orchestrator logging that the daemon emits
//! while it runs.

use std::path::Path;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::events::{QualityMetricsInfo, TeamEvent, VerificationPhaseChangeInfo};
use super::*;

impl TeamDaemon {
    pub(super) fn emit_event(&mut self, event: TeamEvent) {
        self.failure_tracker.push(&event);

        // Dual-write to SQLite telemetry database (best-effort).
        if self.optional_subsystem_ready("telemetry") {
            if let Some(conn) = &self.telemetry_db {
                if let Err(error) = crate::team::telemetry_db::insert_event(conn, &event) {
                    debug!(error = %error, "failed to write telemetry event to SQLite; continuing");
                    if let Err(emit_error) = self.event_sink.emit(TeamEvent::loop_step_error(
                        "telemetry_emit_event",
                        &error.to_string(),
                    )) {
                        warn!(error = %emit_error, "failed to write telemetry budget event");
                    }
                    self.record_optional_subsystem_failure("telemetry", &error.to_string());
                } else {
                    self.record_optional_subsystem_success("telemetry");
                }
            }
        } else {
            debug!("telemetry subsystem disabled by error budget; skipping SQLite write");
        }

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

    pub(super) fn record_agent_handoff(
        &mut self,
        role: &str,
        task: impl Into<String>,
        reason: &str,
        success: bool,
    ) {
        let task = task.into();
        self.emit_event(TeamEvent::agent_handoff(role, &task, reason, success));
    }

    pub(super) fn record_handoff_injected(
        &mut self,
        role: &str,
        task: impl Into<String>,
        reason: &str,
    ) {
        let task = task.into();
        self.emit_event(TeamEvent::handoff_injected(role, &task, reason));
    }

    pub(super) fn record_context_exhausted(
        &mut self,
        role: &str,
        task: Option<u32>,
        session_size_bytes: Option<u64>,
    ) {
        self.emit_event(TeamEvent::context_exhausted(role, task, session_size_bytes));
    }

    pub(super) fn record_context_pressure_warning(
        &mut self,
        role: &str,
        task: Option<u32>,
        pressure_score: u64,
        threshold: u64,
        output_bytes: u64,
    ) {
        self.emit_event(TeamEvent::context_pressure_warning(
            role,
            task,
            pressure_score,
            threshold,
            output_bytes,
        ));
    }

    pub(crate) fn record_delivery_failed(&mut self, role: &str, from: &str, reason: &str) {
        self.emit_event(TeamEvent::delivery_failed(role, from, reason));
    }

    pub(crate) fn record_task_escalated(
        &mut self,
        role: &str,
        task: impl Into<String>,
        reason: Option<&str>,
    ) {
        let task = task.into();
        self.emit_event(TeamEvent::task_escalated(role, &task, reason));
    }

    pub(super) fn record_task_unblocked(&mut self, role: &str, task: impl Into<String>) {
        let task = task.into();
        self.emit_event(TeamEvent::task_unblocked(role, &task));
    }

    pub(super) fn record_state_reconciliation(
        &mut self,
        role: Option<&str>,
        task_id: Option<u32>,
        correction: &str,
    ) {
        let task = task_id.map(|id| id.to_string());
        self.emit_event(TeamEvent::state_reconciliation(
            role,
            task.as_deref(),
            correction,
        ));
    }

    pub(crate) fn record_performance_regression(&mut self, task: impl Into<String>, reason: &str) {
        let task = task.into();
        self.emit_event(TeamEvent::performance_regression(&task, reason));
    }

    pub(crate) fn record_task_completed(&mut self, role: &str, task_id: Option<u32>) {
        if let Some(task_id) = task_id {
            self.record_quality_metrics(role, task_id);
        }
        self.emit_event(TeamEvent::task_completed(
            role,
            task_id.map(|id| id.to_string()).as_deref(),
        ));
    }

    pub(crate) fn record_quality_metrics(&mut self, role: &str, task_id: u32) {
        let Some(backend) = self
            .config
            .members
            .iter()
            .find(|member| member.name == role)
            .and_then(|member| member.agent.as_deref())
        else {
            return;
        };

        let started_at = crate::team::events::read_events(&crate::team::team_events_path(
            &self.config.project_root,
        ))
        .ok()
        .and_then(|events| {
            crate::team::quality_metrics::assignment_started_at(&events, role, task_id)
        });
        let output = self
            .watchers
            .get(role)
            .map(|watcher| watcher.last_lines(200))
            .unwrap_or_default();
        let retries_before_success = self.retry_counts.get(role).copied().unwrap_or(0);
        let commits = crate::team::git_cmd::run_git(
            &self.worktree_dir(role),
            &["rev-list", "--count", "main..HEAD"],
        )
        .ok()
        .and_then(|output| output.stdout.trim().parse::<u32>().ok())
        .unwrap_or(0);

        let metrics = crate::team::quality_metrics::build_completion_quality_metrics(
            backend,
            role,
            task_id,
            &output,
            commits,
            retries_before_success,
            started_at,
            crate::team::now_unix(),
        );
        self.emit_event(TeamEvent::quality_metrics_recorded(&QualityMetricsInfo {
            backend: &metrics.backend,
            role: &metrics.role,
            task: &metrics.task_id,
            narration_ratio: metrics.narration_ratio,
            commit_frequency: metrics.commit_frequency,
            first_pass_test_rate: metrics.first_pass_test_rate,
            retry_rate: metrics.retry_rate,
            time_to_completion_secs: metrics.time_to_completion_secs,
        }));
    }

    pub(crate) fn record_task_auto_merged(
        &mut self,
        engineer: &str,
        task_id: u32,
        confidence: f64,
        files_changed: usize,
        lines_changed: usize,
    ) {
        self.emit_event(TeamEvent::task_auto_merged(
            engineer,
            &task_id.to_string(),
            confidence,
            files_changed,
            lines_changed,
        ));
    }

    pub(crate) fn record_merge_confidence_scored(
        &mut self,
        info: &crate::team::events::MergeConfidenceInfo<'_>,
    ) {
        self.emit_event(TeamEvent::merge_confidence_scored(info));
    }

    pub(crate) fn record_narration_rejection(
        &mut self,
        engineer: &str,
        task_id: u32,
        rejection_count: u32,
    ) {
        self.emit_event(TeamEvent::narration_rejection(
            engineer,
            task_id,
            rejection_count,
        ));
    }

    pub(crate) fn record_verification_phase_changed(
        &mut self,
        engineer: &str,
        task_id: u32,
        from_phase: &str,
        to_phase: &str,
        iteration: u32,
    ) {
        self.emit_event(TeamEvent::verification_phase_changed(
            &VerificationPhaseChangeInfo {
                engineer,
                task: &task_id.to_string(),
                from_phase,
                to_phase,
                iteration,
            },
        ));
    }

    pub(crate) fn record_verification_evidence_collected(
        &mut self,
        engineer: &str,
        task_id: u32,
        evidence_kind: &str,
        detail: &str,
    ) {
        self.emit_event(TeamEvent::verification_evidence_collected(
            engineer,
            &task_id.to_string(),
            evidence_kind,
            detail,
        ));
    }

    pub(crate) fn record_verification_max_iterations_reached(
        &mut self,
        engineer: &str,
        task_id: u32,
        iteration: u32,
        escalated_to: &str,
    ) {
        self.emit_event(TeamEvent::verification_max_iterations_reached(
            engineer,
            &task_id.to_string(),
            iteration,
            escalated_to,
        ));
    }

    #[allow(dead_code)]
    pub(crate) fn record_task_claim_created(
        &mut self,
        engineer: &str,
        task_id: u32,
        ttl_secs: u64,
        expires_at: &str,
    ) {
        self.emit_event(TeamEvent::task_claim_created(
            engineer,
            &task_id.to_string(),
            ttl_secs,
            expires_at,
        ));
    }

    #[allow(dead_code)]
    pub(crate) fn record_task_claim_progress(
        &mut self,
        engineer: &str,
        task_id: u32,
        progress_type: &str,
    ) {
        self.emit_event(TeamEvent::task_claim_progress(
            engineer,
            &task_id.to_string(),
            progress_type,
        ));
    }

    #[allow(dead_code)]
    pub(crate) fn record_task_claim_warning(
        &mut self,
        engineer: &str,
        task_id: u32,
        expires_in_secs: u64,
    ) {
        self.emit_event(TeamEvent::task_claim_warning(
            engineer,
            &task_id.to_string(),
            expires_in_secs,
        ));
    }

    #[allow(dead_code)]
    pub(crate) fn record_task_claim_expired(
        &mut self,
        engineer: &str,
        task_id: u32,
        reclaimed: bool,
        time_held_secs: Option<u64>,
    ) {
        self.emit_event(TeamEvent::task_claim_expired(
            engineer,
            &task_id.to_string(),
            reclaimed,
            time_held_secs,
        ));
    }

    #[allow(dead_code)]
    pub(crate) fn record_task_claim_extended(
        &mut self,
        engineer: &str,
        task_id: u32,
        new_expires_at: &str,
    ) {
        self.emit_event(TeamEvent::task_claim_extended(
            engineer,
            &task_id.to_string(),
            new_expires_at,
        ));
    }

    pub(super) fn record_tact_cycle_triggered(
        &mut self,
        architect: &str,
        idle_engineers: u32,
        board_summary: &str,
    ) {
        self.emit_event(TeamEvent::tact_cycle_triggered(
            architect,
            idle_engineers,
            board_summary,
        ));
    }

    pub(super) fn record_tact_tasks_created(
        &mut self,
        architect: &str,
        tasks_created: u32,
        latency_secs: u64,
        success: bool,
        error: Option<&str>,
    ) {
        self.emit_event(TeamEvent::tact_tasks_created(
            architect,
            tasks_created,
            latency_secs,
            success,
            error,
        ));
    }

    pub(super) fn record_board_task_archived(&mut self, task_id: u32, role: Option<&str>) {
        self.emit_event(TeamEvent::board_task_archived(&task_id.to_string(), role));
    }

    pub(super) fn record_auto_doctor_action(
        &mut self,
        action_type: &str,
        task_id: Option<u32>,
        engineer: Option<&str>,
        details: &str,
    ) {
        self.emit_event(TeamEvent::auto_doctor_action(
            action_type,
            task_id,
            engineer,
            details,
        ));
    }

    pub(super) fn record_standup_generated(&mut self, recipient: &str) {
        self.emit_event(TeamEvent::standup_generated(recipient));
    }

    pub(super) fn record_parity_updated(&mut self, summary: &crate::team::parity::ParitySummary) {
        self.emit_event(TeamEvent::parity_updated(summary));
    }

    pub(super) fn record_retro_generated(&mut self) {
        self.emit_event(TeamEvent::retro_generated());
    }

    pub(crate) fn record_message_routed(&mut self, from: &str, to: &str) {
        self.emit_event(TeamEvent::message_routed(from, to));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn record_barrier_artifact_created(
        &mut self,
        role: &str,
        filename: &str,
        content_hash: &str,
    ) {
        self.emit_event(TeamEvent::barrier_artifact_created(
            role,
            filename,
            content_hash,
        ));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn record_barrier_artifact_read(
        &mut self,
        role: &str,
        filename: &str,
        content_hash: &str,
    ) {
        self.emit_event(TeamEvent::barrier_artifact_read(
            role,
            filename,
            content_hash,
        ));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn record_barrier_violation_attempt(
        &mut self,
        role: &str,
        filename: &str,
        reason: &str,
    ) {
        self.emit_event(TeamEvent::barrier_violation_attempt(role, filename, reason));
    }

    pub(super) fn acknowledge_hot_reload_marker(&mut self) -> bool {
        if !consume_hot_reload_marker(&self.config.project_root) {
            return false;
        }

        self.record_daemon_reloaded();
        self.record_orchestrator_action("runtime: daemon hot-reloaded");
        info!("daemon restarted via hot reload");
        true
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
                agent: None,
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
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
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
                    agent: None,
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
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
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
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            narration_tracker: Default::default(),
            context_pressure_tracker: Default::default(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now(),
            pending_delivery_queue: HashMap::new(),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            shim_handles: HashMap::new(),
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
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
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
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
                ..Default::default()
            },
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("architect".to_string()),
                use_worktrees: false,
                ..Default::default()
            },
        ];

        let mut daemon = TeamDaemon::new(config).unwrap();
        for index in 0..5 {
            let mut event = TeamEvent::task_escalated("eng-1", &format!("{}", 100 + index), None);
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
                agent: None,
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
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
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
    #[cfg_attr(not(feature = "integration"), ignore)]
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
            ..Default::default()
        };
        let events_path = tmp.path().join("events.jsonl");
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    agent: None,
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
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
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
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            narration_tracker: Default::default(),
            context_pressure_tracker: Default::default(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now(),
            pending_delivery_queue: HashMap::new(),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            shim_handles: HashMap::new(),
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
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
}
