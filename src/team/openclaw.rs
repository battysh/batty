use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};

use super::{config, events, hierarchy, messaging, pause_marker_path, status, team_config_path};

const DEFAULT_RECENT_EVENTS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClawProjectConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub project_name: String,
    #[serde(default)]
    pub batty_root: Option<String>,
    #[serde(default)]
    pub status: OpenClawStatusConfig,
    #[serde(default)]
    pub instruction: OpenClawInstructionConfig,
    #[serde(default)]
    pub follow_ups: Vec<OpenClawFollowUp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClawStatusConfig {
    #[serde(default = "default_recent_events")]
    pub recent_events: usize,
}

impl Default for OpenClawStatusConfig {
    fn default() -> Self {
        Self {
            recent_events: default_recent_events(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClawInstructionConfig {
    #[serde(default = "default_instruction_sender")]
    pub sender: String,
    #[serde(default = "default_allowed_roles")]
    pub allowed_roles: Vec<String>,
}

impl Default for OpenClawInstructionConfig {
    fn default() -> Self {
        Self {
            sender: default_instruction_sender(),
            allowed_roles: default_allowed_roles(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClawFollowUp {
    pub name: String,
    pub cron: String,
    pub role: String,
    pub message: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub when: OpenClawFollowUpCondition,
    #[serde(default)]
    pub last_sent_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenClawFollowUpCondition {
    #[default]
    Always,
    ReviewQueueNonEmpty,
    ActiveTasksNonEmpty,
    UnhealthyMembersPresent,
    TriageBacklogPresent,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenClawStatusSummary {
    pub project: String,
    pub team: String,
    pub running: bool,
    pub paused: bool,
    pub active_task_count: usize,
    pub review_queue_count: usize,
    pub unhealthy_members: Vec<String>,
    pub triage_backlog_count: usize,
    pub highlights: Vec<String>,
    pub recent_events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FollowUpDispatch {
    pub name: String,
    pub role: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FollowUpRunSummary {
    pub dispatched: Vec<FollowUpDispatch>,
}

pub trait SupervisorAdapter {
    fn status_report(&self, project_root: &Path) -> Result<status::TeamStatusJsonReport>;
    fn recent_events(&self, project_root: &Path, limit: usize) -> Result<Vec<events::TeamEvent>>;
    fn send_instruction(
        &self,
        project_root: &Path,
        sender: &str,
        role: &str,
        message: &str,
    ) -> Result<()>;
}

pub struct BattySupervisorAdapter;

impl SupervisorAdapter for BattySupervisorAdapter {
    fn status_report(&self, project_root: &Path) -> Result<status::TeamStatusJsonReport> {
        load_status_report(project_root)
    }

    fn recent_events(&self, project_root: &Path, limit: usize) -> Result<Vec<events::TeamEvent>> {
        let mut events = events::read_events(&super::team_events_path(project_root))?;
        if events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
        Ok(events)
    }

    fn send_instruction(
        &self,
        project_root: &Path,
        sender: &str,
        role: &str,
        message: &str,
    ) -> Result<()> {
        messaging::send_message_as(project_root, Some(sender), role, message)
    }
}

pub fn project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("openclaw.yaml")
}

pub fn register_project(project_root: &Path, force: bool) -> Result<PathBuf> {
    let path = project_config_path(project_root);
    if path.exists() && !force {
        bail!(
            "OpenClaw project config already exists at {} (use --force to overwrite)",
            path.display()
        );
    }

    let team_name = load_team_name(project_root).unwrap_or_else(|| "batty".to_string());
    let config = OpenClawProjectConfig {
        version: default_version(),
        project_name: team_name.clone(),
        batty_root: Some(project_root.display().to_string()),
        status: OpenClawStatusConfig::default(),
        instruction: OpenClawInstructionConfig::default(),
        follow_ups: vec![
            OpenClawFollowUp {
                name: "review-queue-reminder".to_string(),
                cron: "*/30 * * * *".to_string(),
                role: "manager".to_string(),
                message: "Review queue still has pending work. Check `batty openclaw status` and move the lane forward.".to_string(),
                summary: Some("Review queue follow-up".to_string()),
                when: OpenClawFollowUpCondition::ReviewQueueNonEmpty,
                last_sent_at: None,
            },
            OpenClawFollowUp {
                name: "architect-utilization-follow-up".to_string(),
                cron: "0 * * * *".to_string(),
                role: "architect".to_string(),
                message: "Please review Batty status and unblock idle capacity with high-level direction if needed.".to_string(),
                summary: Some("Architect utilization follow-up".to_string()),
                when: OpenClawFollowUpCondition::TriageBacklogPresent,
                last_sent_at: None,
            },
        ],
    };
    save_project_config(&path, &config)?;
    Ok(path)
}

pub fn openclaw_status(project_root: &Path, json: bool) -> Result<()> {
    let summary = build_status_summary(project_root, &BattySupervisorAdapter)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("{}", format_status_summary(&summary));
    }
    Ok(())
}

pub fn send_openclaw_instruction(project_root: &Path, role: &str, message: &str) -> Result<()> {
    let config = load_project_config(project_root)?;
    validate_instruction_role(&config, role)?;
    BattySupervisorAdapter.send_instruction(
        project_root,
        &config.instruction.sender,
        role,
        message,
    )?;
    Ok(())
}

pub fn run_follow_ups(project_root: &Path, json: bool) -> Result<()> {
    let summary = run_follow_ups_with_adapter(
        project_root,
        &BattySupervisorAdapter,
        Utc::now(),
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else if summary.dispatched.is_empty() {
        println!("No OpenClaw follow-ups were due.");
    } else {
        println!("OpenClaw follow-ups dispatched:");
        for dispatch in &summary.dispatched {
            println!(
                "- {} -> {} ({})",
                dispatch.name, dispatch.role, dispatch.reason
            );
        }
    }
    Ok(())
}

fn load_project_config(project_root: &Path) -> Result<OpenClawProjectConfig> {
    let path = project_config_path(project_root);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read OpenClaw project config {}", path.display()))?;
    let config = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse OpenClaw project config {}", path.display()))?;
    Ok(config)
}

fn save_project_config(path: &Path, config: &OpenClawProjectConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = serde_yaml::to_string(config)?;
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn build_status_summary<A: SupervisorAdapter>(
    project_root: &Path,
    adapter: &A,
) -> Result<OpenClawStatusSummary> {
    let config = load_project_config(project_root)?;
    let report = adapter.status_report(project_root)?;
    let events = adapter.recent_events(project_root, config.status.recent_events)?;
    Ok(summarize_status_report(&report, &events, &config.project_name))
}

fn summarize_status_report(
    report: &status::TeamStatusJsonReport,
    events: &[events::TeamEvent],
    project_name: &str,
) -> OpenClawStatusSummary {
    let mut highlights = Vec::new();
    if !report.running {
        highlights.push("Batty daemon is not running".to_string());
    }
    if report.paused {
        highlights.push("Batty is paused".to_string());
    }
    if !report.health.unhealthy_members.is_empty() {
        highlights.push(format!(
            "Unhealthy members: {}",
            report.health.unhealthy_members.join(", ")
        ));
    }
    if !report.review_queue.is_empty() {
        highlights.push(format!(
            "Review queue has {} task(s)",
            report.review_queue.len()
        ));
    }
    if report.health.triage_backlog_count > 0 {
        highlights.push(format!(
            "Triage backlog: {} message(s)",
            report.health.triage_backlog_count
        ));
    }

    OpenClawStatusSummary {
        project: project_name.to_string(),
        team: report.team.clone(),
        running: report.running,
        paused: report.paused,
        active_task_count: report.active_tasks.len(),
        review_queue_count: report.review_queue.len(),
        unhealthy_members: report.health.unhealthy_members.clone(),
        triage_backlog_count: report.health.triage_backlog_count,
        highlights,
        recent_events: events.iter().rev().map(format_event_summary).collect(),
    }
}

fn format_status_summary(summary: &OpenClawStatusSummary) -> String {
    let mut lines = vec![
        format!("OpenClaw Project: {}", summary.project),
        format!("Batty Team: {}", summary.team),
        format!(
            "State: {}{}",
            if summary.running { "running" } else { "stopped" },
            if summary.paused { " (paused)" } else { "" }
        ),
        format!(
            "Queues: active={} review={} triage_backlog={}",
            summary.active_task_count, summary.review_queue_count, summary.triage_backlog_count
        ),
    ];

    if summary.highlights.is_empty() {
        lines.push("Highlights: none".to_string());
    } else {
        lines.push("Highlights:".to_string());
        for highlight in &summary.highlights {
            lines.push(format!("- {highlight}"));
        }
    }

    if !summary.recent_events.is_empty() {
        lines.push("Recent events:".to_string());
        for event in &summary.recent_events {
            lines.push(format!("- {event}"));
        }
    }

    lines.join("\n")
}

fn validate_instruction_role(config: &OpenClawProjectConfig, role: &str) -> Result<()> {
    if config.instruction.allowed_roles.iter().any(|item| item == role) {
        return Ok(());
    }

    bail!(
        "OpenClaw instructions may only target these Batty roles: {}",
        config.instruction.allowed_roles.join(", ")
    )
}

fn run_follow_ups_with_adapter<A: SupervisorAdapter>(
    project_root: &Path,
    adapter: &A,
    now: DateTime<Utc>,
) -> Result<FollowUpRunSummary> {
    let path = project_config_path(project_root);
    let mut config = load_project_config(project_root)?;
    let report = adapter.status_report(project_root)?;
    let allowed_roles = config.instruction.allowed_roles.clone();
    let sender = config.instruction.sender.clone();
    let mut dispatched = Vec::new();

    for follow_up in &mut config.follow_ups {
        if !allowed_roles.iter().any(|role| role == &follow_up.role) {
            bail!(
                "OpenClaw instructions may only target these Batty roles: {}",
                allowed_roles.join(", ")
            );
        }
        if !follow_up_condition_matches(follow_up, &report) {
            continue;
        }
        if !is_follow_up_due(follow_up, now)? {
            continue;
        }

        adapter.send_instruction(
            project_root,
            &sender,
            &follow_up.role,
            &follow_up.message,
        )?;
        follow_up.last_sent_at = Some(now.to_rfc3339());
        dispatched.push(FollowUpDispatch {
            name: follow_up.name.clone(),
            role: follow_up.role.clone(),
            reason: follow_up
                .summary
                .clone()
                .unwrap_or_else(|| format!("{:?}", follow_up.when)),
        });
    }

    if !dispatched.is_empty() {
        save_project_config(&path, &config)?;
    }

    Ok(FollowUpRunSummary { dispatched })
}

fn follow_up_condition_matches(
    follow_up: &OpenClawFollowUp,
    report: &status::TeamStatusJsonReport,
) -> bool {
    match follow_up.when {
        OpenClawFollowUpCondition::Always => true,
        OpenClawFollowUpCondition::ReviewQueueNonEmpty => !report.review_queue.is_empty(),
        OpenClawFollowUpCondition::ActiveTasksNonEmpty => !report.active_tasks.is_empty(),
        OpenClawFollowUpCondition::UnhealthyMembersPresent => {
            !report.health.unhealthy_members.is_empty()
        }
        OpenClawFollowUpCondition::TriageBacklogPresent => report.health.triage_backlog_count > 0,
    }
}

fn is_follow_up_due(follow_up: &OpenClawFollowUp, now: DateTime<Utc>) -> Result<bool> {
    let normalized = normalize_cron(&follow_up.cron);
    let schedule = Schedule::from_str(&normalized)
        .with_context(|| format!("invalid OpenClaw follow-up cron '{}'", follow_up.cron))?;
    let reference = follow_up
        .last_sent_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|| now - Duration::days(1));
    let Some(next_run) = schedule.after(&reference).next() else {
        return Ok(false);
    };
    Ok(next_run <= now)
}

fn normalize_cron(expr: &str) -> String {
    let trimmed = expr.trim();
    if trimmed.split_whitespace().count() == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}

fn format_event_summary(event: &events::TeamEvent) -> String {
    match event.event.as_str() {
        "task_completed" => format!(
            "task {} completed by {}",
            event.task.as_deref().unwrap_or("?"),
            event.role.as_deref().unwrap_or("unknown")
        ),
        "task_escalated" => format!(
            "task {} escalated{}",
            event.task.as_deref().unwrap_or("?"),
            event
                .reason
                .as_deref()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        ),
        "task_assigned" => format!(
            "task {} assigned to {}",
            event.task.as_deref().unwrap_or("?"),
            event.role.as_deref().unwrap_or("unknown")
        ),
        "daemon_started" => "daemon started".to_string(),
        "daemon_stopped" => "daemon stopped".to_string(),
        other => {
            let mut parts = vec![other.replace('_', " ")];
            if let Some(task) = event.task.as_deref() {
                parts.push(format!("#{task}"));
            }
            if let Some(role) = event.role.as_deref() {
                parts.push(format!("({role})"));
            }
            parts.join(" ")
        }
    }
}

fn default_version() -> u32 {
    1
}

fn default_recent_events() -> usize {
    DEFAULT_RECENT_EVENTS
}

fn default_instruction_sender() -> String {
    "daemon".to_string()
}

fn default_allowed_roles() -> Vec<String> {
    vec!["architect".to_string(), "manager".to_string()]
}

fn load_team_name(project_root: &Path) -> Option<String> {
    let config_path = team_config_path(project_root);
    config::TeamConfig::load(&config_path).ok().map(|config| config.name)
}

fn load_status_report(project_root: &Path) -> Result<status::TeamStatusJsonReport> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let session_name = format!("batty-{}", team_config.name);
    let session_running = crate::tmux::session_exists(&session_name);
    let runtime_statuses = if session_running {
        status::list_runtime_member_statuses(&session_name).unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let pending_inbox_counts = status::pending_inbox_counts(project_root, &members);
    let triage_backlog_counts = status::triage_backlog_counts(project_root, &members);
    let owned_task_buckets = status::owned_task_buckets(project_root, &members);
    let worktree_staleness = status::worktree_staleness_by_member(project_root, &members);
    let agent_health = status::agent_health_by_member(project_root, &members);
    let paused = pause_marker_path(project_root).exists();
    let rows = status::build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &pending_inbox_counts,
        &triage_backlog_counts,
        &owned_task_buckets,
        &worktree_staleness,
        &agent_health,
    );
    let workflow_metrics = status::workflow_metrics_section(project_root, &members)
        .map(|(_, metrics)| metrics);
    let watchdog = status::load_watchdog_status(project_root, session_running);
    let (active_tasks, review_queue) =
        status::board_status_task_queues(project_root).unwrap_or_default();

    Ok(status::build_team_status_json_report(
        status::TeamStatusJsonReportInput {
            team: team_config.name,
            session: session_name,
            session_running,
            paused,
            watchdog,
            workflow_metrics,
            active_tasks,
            review_queue,
            engineer_profiles: None,
            members: rows,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter {
        report: status::TeamStatusJsonReport,
        events: Vec<events::TeamEvent>,
        sent: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl SupervisorAdapter for FakeAdapter {
        fn status_report(&self, _project_root: &Path) -> Result<status::TeamStatusJsonReport> {
            Ok(self.report.clone())
        }

        fn recent_events(
            &self,
            _project_root: &Path,
            limit: usize,
        ) -> Result<Vec<events::TeamEvent>> {
            Ok(self
                .events
                .iter()
                .rev()
                .take(limit)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect())
        }

        fn send_instruction(
            &self,
            _project_root: &Path,
            sender: &str,
            role: &str,
            message: &str,
        ) -> Result<()> {
            self.sent.lock().unwrap().push((
                sender.to_string(),
                role.to_string(),
                message.to_string(),
            ));
            Ok(())
        }
    }

    fn sample_report() -> status::TeamStatusJsonReport {
        status::TeamStatusJsonReport {
            team: "batty".to_string(),
            session: "batty-batty".to_string(),
            running: true,
            paused: false,
            watchdog: status::WatchdogStatus {
                state: "running".to_string(),
                restart_count: 0,
                current_backoff_secs: None,
                last_exit_reason: None,
            },
            health: status::TeamStatusHealth {
                session_running: true,
                paused: false,
                member_count: 3,
                active_member_count: 2,
                pending_inbox_count: 0,
                triage_backlog_count: 1,
                unhealthy_members: vec!["eng-1-2".to_string()],
            },
            workflow_metrics: None,
            active_tasks: vec![status::StatusTaskEntry {
                id: 12,
                title: "Active".to_string(),
                status: "in-progress".to_string(),
                priority: "high".to_string(),
                claimed_by: Some("eng-1-1".to_string()),
                review_owner: None,
                blocked_on: None,
                branch: None,
                worktree_path: None,
                commit: None,
                next_action: None,
                test_summary: None,
            }],
            review_queue: vec![status::StatusTaskEntry {
                id: 13,
                title: "Review".to_string(),
                status: "review".to_string(),
                priority: "medium".to_string(),
                claimed_by: None,
                review_owner: Some("manager".to_string()),
                blocked_on: None,
                branch: None,
                worktree_path: None,
                commit: None,
                next_action: None,
                test_summary: None,
            }],
            engineer_profiles: None,
            members: Vec::new(),
        }
    }

    fn sample_config() -> OpenClawProjectConfig {
        OpenClawProjectConfig {
            version: 1,
            project_name: "batty".to_string(),
            batty_root: Some("/tmp/batty".to_string()),
            status: OpenClawStatusConfig::default(),
            instruction: OpenClawInstructionConfig::default(),
            follow_ups: vec![OpenClawFollowUp {
                name: "review".to_string(),
                cron: "*/30 * * * *".to_string(),
                role: "manager".to_string(),
                message: "Review queue still has pending work.".to_string(),
                summary: Some("Review queue follow-up".to_string()),
                when: OpenClawFollowUpCondition::ReviewQueueNonEmpty,
                last_sent_at: None,
            }],
        }
    }

    #[test]
    fn register_project_writes_skeleton_config() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        fs::write(
            tmp.path().join(".batty").join("team_config").join("team.yaml"),
            "name: demo\nroles: []\n",
        )
        .unwrap();

        let path = register_project(tmp.path(), false).unwrap();
        let loaded = load_project_config(tmp.path()).unwrap();

        assert_eq!(path, project_config_path(tmp.path()));
        assert_eq!(loaded.project_name, "demo");
        assert_eq!(loaded.instruction.allowed_roles, vec!["architect", "manager"]);
        assert_eq!(loaded.follow_ups.len(), 2);
    }

    #[test]
    fn summarize_status_report_returns_operator_friendly_highlights() {
        let summary = summarize_status_report(
            &sample_report(),
            &[events::TeamEvent::task_completed("eng-1-2", Some("471"))],
            "batty",
        );

        assert_eq!(summary.project, "batty");
        assert_eq!(summary.active_task_count, 1);
        assert_eq!(summary.review_queue_count, 1);
        assert!(summary.highlights.iter().any(|item| item.contains("Unhealthy members")));
        assert!(summary.recent_events[0].contains("task 471 completed"));
    }

    #[test]
    fn validate_instruction_role_rejects_out_of_scope_roles() {
        let error = validate_instruction_role(&sample_config(), "eng-1-1")
            .unwrap_err()
            .to_string();
        assert!(error.contains("architect, manager"));
    }

    #[test]
    fn follow_up_due_when_condition_matches_and_cron_elapsed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = project_config_path(tmp.path());
        save_project_config(&path, &sample_config()).unwrap();

        let adapter = FakeAdapter {
            report: sample_report(),
            events: Vec::new(),
            sent: std::sync::Mutex::new(Vec::new()),
        };
        let now = DateTime::parse_from_rfc3339("2026-04-06T13:30:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let summary = run_follow_ups_with_adapter(tmp.path(), &adapter, now).unwrap();

        assert_eq!(summary.dispatched.len(), 1);
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent[0].0, "daemon");
        assert_eq!(sent[0].1, "manager");
        assert!(sent[0].2.contains("Review queue"));
    }

    #[test]
    fn follow_up_skips_when_condition_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = sample_config();
        config.follow_ups[0].when = OpenClawFollowUpCondition::UnhealthyMembersPresent;
        let path = project_config_path(tmp.path());
        save_project_config(&path, &config).unwrap();

        let mut report = sample_report();
        report.health.unhealthy_members.clear();
        let adapter = FakeAdapter {
            report,
            events: Vec::new(),
            sent: std::sync::Mutex::new(Vec::new()),
        };
        let now = DateTime::parse_from_rfc3339("2026-04-06T13:30:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let summary = run_follow_ups_with_adapter(tmp.path(), &adapter, now).unwrap();

        assert!(summary.dispatched.is_empty());
        assert!(adapter.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn format_status_summary_includes_highlights_and_events() {
        let rendered = format_status_summary(&OpenClawStatusSummary {
            project: "batty".to_string(),
            team: "batty".to_string(),
            running: true,
            paused: false,
            active_task_count: 1,
            review_queue_count: 2,
            unhealthy_members: vec!["eng-1-2".to_string()],
            triage_backlog_count: 1,
            highlights: vec!["Review queue has 2 task(s)".to_string()],
            recent_events: vec!["task 471 completed by eng-1-2".to_string()],
        });

        assert!(rendered.contains("OpenClaw Project: batty"));
        assert!(rendered.contains("Review queue has 2 task(s)"));
        assert!(rendered.contains("task 471 completed by eng-1-2"));
    }
}
