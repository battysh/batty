//! Session lifecycle: pause/resume, nudge management, stop/attach/status/validate.
//!
//! Extracted from `lifecycle.rs` — pure refactor, zero logic changes.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tracing::{info, warn};

use super::daemon_mgmt::{
    DAEMON_SHUTDOWN_GRACE_PERIOD, force_kill_daemon, request_graceful_daemon_shutdown,
    resume_marker_path,
};
use super::{
    config, estimation, events, hierarchy, now_unix, status, team_config_path, team_events_path,
};
use crate::tmux;

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
            "Session summary: {} tasks completed, {} merged, runtime {}\nBatty v{} — https://github.com/battysh/batty",
            self.tasks_completed,
            self.tasks_merged,
            format_runtime(self.runtime_secs),
            env!("CARGO_PKG_VERSION"),
        )
    }
}

pub(crate) fn format_runtime(secs: u64) -> String {
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

#[derive(Debug, Clone, Serialize)]
struct ResumeMarkerState {
    discord_event_cursor: Option<usize>,
}

fn write_resume_marker(project_root: &Path, discord_event_cursor: Option<usize>) {
    let marker = resume_marker_path(project_root);
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = ResumeMarkerState {
        discord_event_cursor,
    };
    if let Ok(rendered) = serde_json::to_string(&payload) {
        let _ = std::fs::write(&marker, rendered);
    } else {
        let _ = std::fs::write(&marker, "");
    }
}

fn graceful_shutdown_wait(team_config: &config::TeamConfig) -> Duration {
    let requested = Duration::from_secs(
        team_config.workflow_policy.graceful_shutdown_timeout_secs
            + u64::from(team_config.shim_shutdown_timeout_secs)
            + 5,
    );
    std::cmp::max(DAEMON_SHUTDOWN_GRACE_PERIOD, requested)
}

#[derive(Debug, serde::Deserialize)]
struct PersistedDiscordCursor {
    #[serde(default)]
    discord_event_cursor: usize,
}

fn persisted_discord_event_cursor(project_root: &Path) -> Option<usize> {
    let path = super::daemon_state_path(project_root);
    let content = std::fs::read_to_string(path).ok()?;
    let state: PersistedDiscordCursor = serde_json::from_str(&content).ok()?;
    Some(state.discord_event_cursor)
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
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }
    let team_config = config::TeamConfig::load(&config_path)?;
    let primary_session = format!("batty-{}", team_config.name);

    let pre_summary = compute_session_summary(project_root);
    let pre_snapshot = super::daemon::build_shutdown_snapshot(project_root, pre_summary.as_ref());
    if let Err(error) = super::daemon::send_discord_shutdown_notice(&team_config, &pre_snapshot) {
        warn!(error = %error, "failed to send Discord shutdown notice");
    }

    // Write resume marker before tearing down — agents have sessions to continue
    write_resume_marker(project_root, None);

    // Ask the daemon to persist a final clean snapshot before the tmux session is torn down.
    if !request_graceful_daemon_shutdown(project_root, graceful_shutdown_wait(&team_config)) {
        warn!("daemon did not stop gracefully; forcing shutdown");
        force_kill_daemon(project_root);
    }

    // Kill only the session belonging to this project
    if tmux::session_exists(&primary_session) {
        tmux::kill_session(&primary_session)?;
        info!(session = %primary_session, "team session stopped");
    } else {
        info!(session = %primary_session, "no running session to stop");
    }

    let final_summary = compute_session_summary(project_root);
    let final_snapshot =
        super::daemon::build_shutdown_snapshot(project_root, final_summary.as_ref());
    if let Err(error) = super::daemon::send_discord_shutdown_summary(&team_config, &final_snapshot)
    {
        warn!(error = %error, "failed to send Discord shutdown summary");
    }
    write_resume_marker(project_root, persisted_discord_event_cursor(project_root));

    // Print session summary after teardown.
    if let Some(summary) = final_summary {
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
pub fn team_status(project_root: &Path, json: bool, detail: bool, health: bool) -> Result<()> {
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
    let supervisory_pressures = status::supervisory_status_pressure(
        project_root,
        &members,
        session_running,
        &runtime_statuses,
    );
    let branch_mismatches = status::branch_mismatch_by_member(project_root, &members);
    let worktree_staleness = status::worktree_staleness_by_member(project_root, &members);
    let agent_health = status::agent_health_by_member(project_root, &members);
    let paused = pause_marker_path(project_root).exists();
    let mut rows = status::build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &pending_inbox_counts,
        &triage_backlog_counts,
        &owned_task_buckets,
        &supervisory_pressures,
        &branch_mismatches,
        &worktree_staleness,
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
    let watchdog = status::load_watchdog_status(project_root, session_running);
    let main_smoke = status::load_main_smoke_state(project_root);
    let bench_state = match crate::team::bench::load_bench_state(project_root) {
        Ok(state) => state,
        Err(error) => {
            warn!(error = %error, "failed to load bench state for status");
            crate::team::bench::BenchState::default()
        }
    };
    let (active_tasks, review_queue) = match status::board_status_task_queues(project_root) {
        Ok(queues) => queues,
        Err(error) => {
            warn!(error = %error, "failed to load board task queues for status json");
            (Vec::new(), Vec::new())
        }
    };

    let engineer_profiles = if detail {
        crate::team::telemetry_db::open(project_root)
            .ok()
            .and_then(|conn| {
                crate::team::telemetry_db::query_engineer_performance_profiles(&conn).ok()
            })
            .filter(|rows| !rows.is_empty())
    } else {
        None
    };
    let optional_subsystems =
        health.then(|| status::load_optional_subsystem_statuses(project_root));

    if json {
        let report = status::build_team_status_json_report(status::TeamStatusJsonReportInput {
            team: team_config.name.clone(),
            session: session.clone(),
            session_running,
            paused,
            main_smoke: main_smoke.clone(),
            watchdog,
            workflow_metrics: workflow_metrics
                .as_ref()
                .map(|(_, metrics)| metrics.clone()),
            active_tasks,
            review_queue,
            optional_subsystems,
            engineer_profiles,
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
        println!("Watchdog: {}", status::format_watchdog_summary(&watchdog));
        if let Some(main_smoke) = main_smoke.as_ref() {
            println!(
                "Main smoke: {}",
                status::format_main_smoke_summary(main_smoke)
            );
        }
        println!();
        println!(
            "{:<20} {:<12} {:<10} {:<12} {:>5} {:>6} {:>7} {:<14} {:<14} {:<16} {:<18} {:<24} {:<20}",
            "MEMBER",
            "ROLE",
            "AGENT",
            "STATE",
            "INBOX",
            "TRIAGE",
            "STALE",
            "ACTIVE",
            "REVIEW",
            "ETA",
            "HEALTH",
            "SIGNAL",
            "REPORTS TO"
        );
        println!("{}", "-".repeat(203));
        for row in &rows {
            println!(
                "{:<20} {:<12} {:<10} {:<12} {:>5} {:>6} {:>7} {:<14} {:<14} {:<16} {:<18} {:<24} {:<20}",
                row.name,
                row.role,
                row.agent.as_deref().unwrap_or("-"),
                row.state,
                row.pending_inbox,
                row.triage_backlog,
                row.worktree_staleness
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "-".to_string()),
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
        let failed_test_tasks = active_tasks
            .iter()
            .chain(review_queue.iter())
            .filter_map(|task| {
                task.test_summary
                    .as_ref()
                    .map(|summary| format!("#{} {}: {}", task.id, task.title, summary))
            })
            .collect::<Vec<_>>();
        if !failed_test_tasks.is_empty() {
            println!();
            println!("Failed Tests");
            for line in failed_test_tasks {
                println!("- {line}");
            }
        }
        if let Some(optional_subsystems) = optional_subsystems {
            println!();
            println!(
                "{}",
                status::format_optional_subsystem_statuses(&optional_subsystems)
            );
        }
        if detail {
            if let Some(profiles) = engineer_profiles {
                println!();
                println!("{}", status::format_engineer_profiles(&profiles));
            } else {
                println!();
                println!("Engineer Profiles\nNo engineer performance telemetry recorded yet.");
            }
        }
        if let Some(formatted) = status::format_benched_engineers(&bench_state) {
            println!();
            println!("{formatted}");
        }
    }

    Ok(())
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
        if team_config.orchestrator_pane
            && matches!(team_config.workflow_mode, config::WorkflowMode::Hybrid)
        {
            return vec![
                "Migration: workflow_mode omitted; orchestrator_pane=true promotes the team to hybrid mode so the orchestrator surface is active.".to_string(),
            ];
        }
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
        config::WorkflowMode::BoardFirst => vec![
            "Migration: board_first mode selected; the board becomes the primary coordination surface while manager relay stays reserved for review, blockers, and escalation.".to_string(),
        ],
    }
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
            config::WorkflowMode::BoardFirst => "board_first",
        }
    );
    println!("Roles: {}", team_config.roles.len());
    println!("Total members: {}", members.len());

    // Backend health checks — warn about missing binaries but don't fail validation.
    let backend_warnings = team_config.check_backend_health();
    for warning in &backend_warnings {
        println!("[WARN] {warning}");
    }

    for note in migration_validation_notes(&team_config, workflow_mode_is_explicit) {
        println!("{note}");
    }
    println!("Valid.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::TRIAGE_RESULT_FRESHNESS_SECONDS;
    use crate::team::config::RoleType;
    use crate::team::hierarchy;
    use crate::team::inbox;
    use crate::team::status;
    use crate::team::team_config_dir;
    use crate::team::team_config_path;
    use serial_test::serial;

    #[test]
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

    fn write_team_config(project_root: &std::path::Path, yaml: &str) {
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
            config::TeamConfig::load(std::path::Path::new("src/team/templates/team_pair.yaml"))
                .unwrap();
        let notes = migration_validation_notes(&config, false);

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("workflow_mode omitted"));
        // team_pair.yaml has orchestrator_pane: true, so it gets promoted to hybrid
        assert!(notes[0].contains("promotes the team to hybrid"));
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
    fn migration_validation_notes_describe_board_first_manager_relay_policy() {
        let config: config::TeamConfig = serde_yaml::from_str(
            r#"
name: test
workflow_mode: board_first
roles:
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();
        let notes = migration_validation_notes(&config, true);

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("board_first mode selected"));
        assert!(notes[0].contains("board becomes the primary coordination surface"));
        assert!(notes[0].contains("manager relay"));
    }

    fn make_member(name: &str, role_name: &str, role_type: RoleType) -> hierarchy::MemberInstance {
        hierarchy::MemberInstance {
            name: name.to_string(),
            role_name: role_name.to_string(),
            role_type,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
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
                stale_review: Vec::new(),
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
            &Default::default(),
            &Default::default(),
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
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(rows[0].state, "reviewing");
        assert_eq!(rows[0].pending_inbox, 3);
        assert_eq!(rows[0].triage_backlog, 2);
        assert_eq!(rows[0].active_owned_tasks, vec![191]);
        assert_eq!(rows[0].review_owned_tasks, vec![193]);
        assert_eq!(rows[0].signal.as_deref(), Some("standup"));
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
                ..Default::default()
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
        assert!(review_buckets.review.is_empty());
        assert_eq!(review_buckets.stale_review, vec![193]);
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

    // --- Session summary tests ---

    #[test]
    fn session_summary_counts_completions_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let events_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&events_dir).unwrap();

        let now = crate::team::now_unix();
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

        let now = crate::team::now_unix();
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
            format!(
                "Session summary: 5 tasks completed, 4 merged, runtime 2h 15m\nBatty v{} — https://github.com/battysh/batty",
                env!("CARGO_PKG_VERSION")
            )
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

        let now = crate::team::now_unix();
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

    #[test]
    fn write_resume_marker_persists_discord_cursor() {
        let tmp = tempfile::tempdir().unwrap();

        write_resume_marker(tmp.path(), Some(17));

        let marker = resume_marker_path(tmp.path());
        let payload = std::fs::read_to_string(marker).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "discord_event_cursor": 17 }).to_string()
        );
    }

    #[test]
    fn persisted_discord_event_cursor_reads_saved_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = crate::team::daemon_state_path(tmp.path());
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(
            &state_path,
            serde_json::json!({
                "clean_shutdown": true,
                "saved_at": 123,
                "discord_event_cursor": 29,
                "states": {},
                "active_tasks": {},
                "retry_counts": {},
                "dispatch_queue": [],
                "paused_standups": [],
                "last_standup_elapsed_secs": {},
                "nudge_state": {},
                "pipeline_starvation_fired": false,
                "optional_subsystem_backoff": {},
                "optional_subsystem_disabled_remaining_secs": {}
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(persisted_discord_event_cursor(tmp.path()), Some(29));
    }

    #[test]
    fn graceful_shutdown_wait_includes_commit_window_and_shim_timeout() {
        let mut team_config: config::TeamConfig = serde_yaml::from_str(
            r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [manager]
"#,
        )
        .unwrap();
        team_config.workflow_policy.graceful_shutdown_timeout_secs = 30;
        team_config.shim_shutdown_timeout_secs = 10;

        assert_eq!(
            graceful_shutdown_wait(&team_config),
            Duration::from_secs(45)
        );
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
    fn production_daemon_mgmt_has_limited_unwrap_or_expect_calls() {
        let src = include_str!("daemon_mgmt.rs");
        // spawn_daemon uses unwrap_or_else for canonicalize — this is acceptable
        assert!(
            production_unwrap_expect_count(src) <= 1,
            "daemon_mgmt.rs should minimize unwrap/expect in production code"
        );
    }

    #[test]
    fn production_session_has_no_unwrap_or_expect_calls() {
        let src = include_str!("session.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "session.rs should avoid unwrap/expect"
        );
    }
}
