use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::task;

use super::config::{self, RoleType};
use super::daemon::NudgeSchedule;
use super::events;
use super::hierarchy::MemberInstance;
use super::inbox;
use super::standup::MemberState;
use super::{
    TRIAGE_RESULT_FRESHNESS_SECONDS, daemon_state_path, now_unix, pause_marker_path,
    team_config_dir, team_config_path, team_events_path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RuntimeMemberStatus {
    pub(crate) state: String,
    pub(crate) signal: Option<String>,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamStatusRow {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) role_type: String,
    pub(crate) agent: Option<String>,
    pub(crate) reports_to: Option<String>,
    pub(crate) state: String,
    pub(crate) pending_inbox: usize,
    pub(crate) triage_backlog: usize,
    pub(crate) active_owned_tasks: Vec<u32>,
    pub(crate) review_owned_tasks: Vec<u32>,
    pub(crate) signal: Option<String>,
    pub(crate) runtime_label: Option<String>,
    pub(crate) health: AgentHealthSummary,
    pub(crate) health_summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TriageBacklogState {
    pub(crate) count: usize,
    pub(crate) newest_result_ts: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OwnedTaskBuckets {
    pub(crate) active: Vec<u32>,
    pub(crate) review: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AgentHealthSummary {
    pub(crate) restart_count: u32,
    pub(crate) context_exhaustion_count: u32,
    pub(crate) delivery_failure_count: u32,
    pub(crate) task_elapsed_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PersistedDaemonHealthState {
    #[serde(default)]
    active_tasks: HashMap<String, u32>,
    #[serde(default)]
    retry_counts: HashMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct WorkflowMetrics {
    pub runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub in_progress_count: u32,
    pub idle_with_runnable: Vec<String>,
    pub oldest_review_age_secs: Option<u64>,
    pub oldest_assignment_age_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusTaskEntry {
    pub(crate) id: u32,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) priority: String,
    pub(crate) claimed_by: Option<String>,
    pub(crate) review_owner: Option<String>,
    pub(crate) blocked_on: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamStatusHealth {
    pub(crate) session_running: bool,
    pub(crate) paused: bool,
    pub(crate) member_count: usize,
    pub(crate) active_member_count: usize,
    pub(crate) pending_inbox_count: usize,
    pub(crate) triage_backlog_count: usize,
    pub(crate) unhealthy_members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamStatusJsonReport {
    pub(crate) team: String,
    pub(crate) session: String,
    pub(crate) running: bool,
    pub(crate) paused: bool,
    pub(crate) health: TeamStatusHealth,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    pub(crate) members: Vec<TeamStatusRow>,
}

pub(crate) fn list_runtime_member_statuses(
    session: &str,
) -> Result<HashMap<String, RuntimeMemberStatus>> {
    let output = Command::new("tmux")
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

    let mut statuses = HashMap::new();
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

pub(crate) fn summarize_runtime_member_status(
    raw_status: &str,
    pane_dead: bool,
) -> RuntimeMemberStatus {
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

pub(crate) fn strip_tmux_style(input: &str) -> String {
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

pub(crate) fn build_team_status_rows(
    members: &[MemberInstance],
    session_running: bool,
    runtime_statuses: &HashMap<String, RuntimeMemberStatus>,
    pending_inbox_counts: &HashMap<String, usize>,
    triage_backlog_counts: &HashMap<String, usize>,
    owned_task_buckets: &HashMap<String, OwnedTaskBuckets>,
    agent_health: &HashMap<String, AgentHealthSummary>,
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
            let health = agent_health.get(&member.name).cloned().unwrap_or_default();
            let health_summary = format_agent_health_summary(&health);

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
                health,
                health_summary,
            }
        })
        .collect()
}

pub(crate) fn agent_health_by_member(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, AgentHealthSummary> {
    let mut health_by_member = members
        .iter()
        .map(|member| (member.name.clone(), AgentHealthSummary::default()))
        .collect::<HashMap<_, _>>();

    let daemon_state = match load_persisted_daemon_health_state(&daemon_state_path(project_root)) {
        Ok(state) => state.unwrap_or_default(),
        Err(error) => {
            warn!(error = %error, "failed to load daemon health state");
            PersistedDaemonHealthState::default()
        }
    };

    for (member, retry_count) in &daemon_state.retry_counts {
        health_by_member
            .entry(member.clone())
            .or_default()
            .restart_count = health_by_member
            .get(member)
            .map(|health| health.restart_count.max(*retry_count))
            .unwrap_or(*retry_count);
    }

    let mut restart_events = HashMap::<String, u32>::new();
    let mut latest_assignment_ts = HashMap::<String, u64>::new();
    let mut latest_assignment_ts_by_task = HashMap::<(String, u32), u64>::new();
    match events::read_events(&team_events_path(project_root)) {
        Ok(events) => {
            for event in events {
                let Some(role) = event.role.as_deref() else {
                    continue;
                };

                match event.event.as_str() {
                    "agent_restarted" => {
                        *restart_events.entry(role.to_string()).or_insert(0) += 1;
                        if let Some(restart_count) = event.restart_count {
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .restart_count = health_by_member
                                .get(role)
                                .map(|health| health.restart_count.max(restart_count))
                                .unwrap_or(restart_count);
                        }
                    }
                    "context_exhausted" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .context_exhaustion_count += 1;
                    }
                    "delivery_failed" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .delivery_failure_count += 1;
                    }
                    "task_assigned" => {
                        latest_assignment_ts.insert(role.to_string(), event.ts);
                        if let Some(task_id) =
                            event.task.as_deref().and_then(parse_assigned_task_id)
                        {
                            latest_assignment_ts_by_task
                                .insert((role.to_string(), task_id), event.ts);
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(error) => {
            warn!(error = %error, "failed to read team events for status health summary");
        }
    }

    for (member, event_count) in restart_events {
        let health = health_by_member.entry(member).or_default();
        health.restart_count = health.restart_count.max(event_count);
    }

    let now = now_unix();
    for (member, task_id) in daemon_state.active_tasks {
        let assigned_ts = latest_assignment_ts_by_task
            .get(&(member.clone(), task_id))
            .copied()
            .or_else(|| latest_assignment_ts.get(&member).copied());
        if let Some(assigned_ts) = assigned_ts {
            health_by_member
                .entry(member)
                .or_default()
                .task_elapsed_secs = Some(now.saturating_sub(assigned_ts));
        }
    }

    health_by_member
}

fn load_persisted_daemon_health_state(path: &Path) -> Result<Option<PersistedDaemonHealthState>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let state = serde_json::from_str::<PersistedDaemonHealthState>(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

fn parse_assigned_task_id(task: &str) -> Option<u32> {
    let trimmed = task.trim();
    trimmed
        .parse::<u32>()
        .ok()
        .or_else(|| {
            trimmed
                .split("Task #")
                .nth(1)
                .and_then(parse_leading_task_id)
        })
        .or_else(|| {
            trimmed
                .find('#')
                .and_then(|idx| parse_leading_task_id(&trimmed[idx + 1..]))
        })
}

fn parse_leading_task_id(value: &str) -> Option<u32> {
    let digits = value
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

pub(crate) fn format_agent_health_summary(health: &AgentHealthSummary) -> String {
    let mut parts = Vec::new();
    if health.restart_count > 0 {
        parts.push(format!("r{}", health.restart_count));
    }
    if health.context_exhaustion_count > 0 {
        parts.push(format!("c{}", health.context_exhaustion_count));
    }
    if health.delivery_failure_count > 0 {
        parts.push(format!("d{}", health.delivery_failure_count));
    }
    if let Some(task_elapsed_secs) = health.task_elapsed_secs {
        parts.push(format!("t{}", format_health_duration(task_elapsed_secs)));
    }

    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

fn format_health_duration(task_elapsed_secs: u64) -> String {
    if task_elapsed_secs < 60 {
        format!("{task_elapsed_secs}s")
    } else if task_elapsed_secs < 3_600 {
        format!("{}m", task_elapsed_secs / 60)
    } else if task_elapsed_secs < 86_400 {
        format!("{}h", task_elapsed_secs / 3_600)
    } else {
        format!("{}d", task_elapsed_secs / 86_400)
    }
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

pub(crate) fn triage_backlog_counts(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, usize> {
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

pub(crate) fn direct_reports_by_member(members: &[MemberInstance]) -> HashMap<String, Vec<String>> {
    let mut direct_reports: HashMap<String, Vec<String>> = HashMap::new();
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

pub(crate) fn delivered_direct_report_triage_count(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
) -> Result<usize> {
    Ok(delivered_direct_report_triage_state(inbox_root, member_name, direct_reports)?.count)
}

pub(crate) fn delivered_direct_report_triage_state(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
) -> Result<TriageBacklogState> {
    delivered_direct_report_triage_state_at(inbox_root, member_name, direct_reports, now_unix())
}

pub(crate) fn delivered_direct_report_triage_state_at(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
    now_ts: u64,
) -> Result<TriageBacklogState> {
    if direct_reports.is_empty() {
        return Ok(TriageBacklogState {
            count: 0,
            newest_result_ts: 0,
        });
    }

    let mut latest_outbound_by_report = HashMap::new();
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
        let is_fresh = now_ts.saturating_sub(msg.timestamp) <= TRIAGE_RESULT_FRESHNESS_SECONDS;
        let needs_triage = *delivered
            && is_fresh
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

pub(crate) fn pending_inbox_counts(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, usize> {
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

pub(crate) fn owned_task_buckets(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, OwnedTaskBuckets> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return HashMap::new();
    }

    let member_names: HashSet<&str> = members.iter().map(|member| member.name.as_str()).collect();
    let mut owned = HashMap::<String, OwnedTaskBuckets>::new();
    let tasks = match crate::task::load_tasks_from_dir(&tasks_dir) {
        Ok(tasks) => tasks,
        Err(error) => {
            warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for status");
            return HashMap::new();
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

pub(crate) fn format_owned_tasks_summary(task_ids: &[u32]) -> String {
    match task_ids {
        [] => "-".to_string(),
        [task_id] => format!("#{task_id}"),
        [first, second] => format!("#{first},#{second}"),
        [first, second, rest @ ..] => format!("#{first},#{second},+{}", rest.len()),
    }
}

pub(crate) fn board_status_task_queues(
    project_root: &Path,
) -> Result<(Vec<StatusTaskEntry>, Vec<StatusTaskEntry>)> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut active_tasks = Vec::new();
    let mut review_queue = Vec::new();
    for task in task::load_tasks_from_dir(&tasks_dir)? {
        let entry = StatusTaskEntry {
            id: task.id,
            title: task.title,
            status: task.status.clone(),
            priority: task.priority,
            claimed_by: task.claimed_by,
            review_owner: task.review_owner,
            blocked_on: task.blocked_on,
            branch: task.branch,
            worktree_path: task.worktree_path,
            commit: task.commit,
            next_action: task.next_action,
        };

        match task.status.as_str() {
            "in-progress" | "in_progress" => active_tasks.push(entry),
            "review" => review_queue.push(entry),
            _ => {}
        }
    }

    Ok((active_tasks, review_queue))
}

pub(crate) fn build_team_status_health(
    rows: &[TeamStatusRow],
    session_running: bool,
    paused: bool,
) -> TeamStatusHealth {
    let member_rows: Vec<&TeamStatusRow> =
        rows.iter().filter(|row| row.role_type != "User").collect();
    let mut unhealthy_members = member_rows
        .iter()
        .filter(|row| {
            row.health.restart_count > 0
                || row.health.context_exhaustion_count > 0
                || row.health.delivery_failure_count > 0
        })
        .map(|row| row.name.clone())
        .collect::<Vec<_>>();
    unhealthy_members.sort();

    TeamStatusHealth {
        session_running,
        paused,
        member_count: member_rows.len(),
        active_member_count: member_rows
            .iter()
            .filter(|row| matches!(row.state.as_str(), "working" | "triaging" | "reviewing"))
            .count(),
        pending_inbox_count: member_rows.iter().map(|row| row.pending_inbox).sum(),
        triage_backlog_count: member_rows.iter().map(|row| row.triage_backlog).sum(),
        unhealthy_members,
    }
}

pub(crate) struct TeamStatusJsonReportInput {
    pub(crate) team: String,
    pub(crate) session: String,
    pub(crate) session_running: bool,
    pub(crate) paused: bool,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    pub(crate) members: Vec<TeamStatusRow>,
}

pub(crate) fn build_team_status_json_report(
    input: TeamStatusJsonReportInput,
) -> TeamStatusJsonReport {
    let TeamStatusJsonReportInput {
        team,
        session,
        session_running,
        paused,
        workflow_metrics,
        active_tasks,
        review_queue,
        members,
    } = input;
    let health = build_team_status_health(&members, session_running, paused);
    TeamStatusJsonReport {
        team,
        session,
        running: session_running,
        paused,
        health,
        workflow_metrics,
        active_tasks,
        review_queue,
        members,
    }
}

pub fn compute_metrics(board_dir: &Path, members: &[MemberInstance]) -> Result<WorkflowMetrics> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(WorkflowMetrics::default());
    }

    let tasks = task::load_tasks_from_dir(&tasks_dir)?;
    if tasks.is_empty() {
        return Ok(WorkflowMetrics::default());
    }

    let task_status_by_id: HashMap<u32, String> = tasks
        .iter()
        .map(|task| (task.id, task.status.clone()))
        .collect();

    let now = SystemTime::now();
    let runnable_count = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
        .filter(|task| task.claimed_by.is_none())
        .filter(|task| task.blocked.is_none())
        .filter(|task| {
            task.depends_on.iter().all(|dep_id| {
                task_status_by_id
                    .get(dep_id)
                    .is_none_or(|status| status == "done")
            })
        })
        .count() as u32;

    let blocked_count = tasks
        .iter()
        .filter(|task| task.status == "blocked" || task.blocked.is_some())
        .count() as u32;
    let in_review_count = tasks.iter().filter(|task| task.status == "review").count() as u32;
    let in_progress_count = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "in_progress"))
        .count() as u32;

    let oldest_review_age_secs = tasks
        .iter()
        .filter(|task| task.status == "review")
        .filter_map(|task| file_age_secs(&task.source_path, now))
        .max();
    let oldest_assignment_age_secs = tasks
        .iter()
        .filter(|task| task.claimed_by.is_some())
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
        .filter_map(|task| file_age_secs(&task.source_path, now))
        .max();

    let idle_with_runnable = compute_idle_with_runnable(board_dir, members, &tasks, runnable_count);

    Ok(WorkflowMetrics {
        runnable_count,
        blocked_count,
        in_review_count,
        in_progress_count,
        idle_with_runnable,
        oldest_review_age_secs,
        oldest_assignment_age_secs,
    })
}

pub fn format_metrics(metrics: &WorkflowMetrics) -> String {
    let idle = if metrics.idle_with_runnable.is_empty() {
        "-".to_string()
    } else {
        metrics.idle_with_runnable.join(", ")
    };

    format!(
        "Workflow Metrics\n\
Runnable: {}\n\
Blocked: {}\n\
In Review: {}\n\
In Progress: {}\n\
Idle With Runnable: {}\n\
Oldest Review Age: {}\n\
Oldest Assignment Age: {}",
        metrics.runnable_count,
        metrics.blocked_count,
        metrics.in_review_count,
        metrics.in_progress_count,
        idle,
        format_age(metrics.oldest_review_age_secs),
        format_age(metrics.oldest_assignment_age_secs),
    )
}

fn compute_idle_with_runnable(
    board_dir: &Path,
    members: &[MemberInstance],
    tasks: &[task::Task],
    runnable_count: u32,
) -> Vec<String> {
    if runnable_count == 0 {
        return Vec::new();
    }

    let busy_engineers: HashSet<&str> = tasks
        .iter()
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
        .filter_map(|task| task.claimed_by.as_deref())
        .collect();

    let pending_root = project_root_from_board_dir(board_dir).map(inbox::inboxes_root);
    let mut idle = members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .filter(|member| !busy_engineers.contains(member.name.as_str()))
        .filter(|member| {
            pending_root
                .as_ref()
                .and_then(|root| inbox::pending_message_count(root, &member.name).ok())
                .unwrap_or(0)
                == 0
        })
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    idle.sort();
    idle
}

fn project_root_from_board_dir(board_dir: &Path) -> Option<&Path> {
    board_dir.parent()?.parent()?.parent()
}

fn file_age_secs(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified)
        .ok()
        .map(|duration| duration.as_secs())
}

fn format_age(age_secs: Option<u64>) -> String {
    age_secs
        .map(|secs| format!("{secs}s"))
        .unwrap_or_else(|| "n/a".to_string())
}

pub(crate) fn workflow_metrics_section(
    project_root: &Path,
    members: &[MemberInstance],
) -> Option<(String, WorkflowMetrics)> {
    let config_path = team_config_path(project_root);
    if !workflow_metrics_enabled(&config_path) {
        return None;
    }

    let board_dir = team_config_dir(project_root).join("board");
    match compute_metrics(&board_dir, members) {
        Ok(metrics) => {
            let formatted = format_metrics(&metrics);
            Some((formatted, metrics))
        }
        Err(error) => {
            warn!(path = %board_dir.display(), error = %error, "failed to compute workflow metrics");
            None
        }
    }
}

pub(crate) fn workflow_metrics_enabled(config_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };

    content.lines().any(|line| {
        let line = line.trim();
        matches!(
            line,
            "workflow_mode: hybrid" | "workflow_mode: workflow_first"
        )
    })
}

pub(crate) struct PaneStatusLabelUpdateContext<'a, F>
where
    F: Fn(&str) -> Option<Duration>,
{
    pub(crate) project_root: &'a Path,
    pub(crate) members: &'a [MemberInstance],
    pub(crate) pane_map: &'a HashMap<String, String>,
    pub(crate) states: &'a HashMap<String, MemberState>,
    pub(crate) nudges: &'a HashMap<String, NudgeSchedule>,
    pub(crate) last_standup: &'a HashMap<String, Instant>,
    pub(crate) paused_standups: &'a HashSet<String>,
    pub(crate) standup_interval_for_member: F,
}

pub(crate) fn update_pane_status_labels<F>(context: PaneStatusLabelUpdateContext<'_, F>)
where
    F: Fn(&str) -> Option<Duration>,
{
    let PaneStatusLabelUpdateContext {
        project_root,
        members,
        pane_map,
        states,
        nudges,
        last_standup,
        paused_standups,
        standup_interval_for_member,
    } = context;
    let globally_paused = pause_marker_path(project_root).exists();
    let inbox_root = inbox::inboxes_root(project_root);
    let direct_reports = direct_reports_by_member(members);
    let owned_task_buckets = owned_task_buckets(project_root, members);

    for member in members {
        if member.role_type == RoleType::User {
            continue;
        }
        let Some(pane_id) = pane_map.get(&member.name) else {
            continue;
        };

        let state = states
            .get(&member.name)
            .copied()
            .unwrap_or(MemberState::Idle);

        let pending_inbox = match inbox::pending_message_count(&inbox_root, &member.name) {
            Ok(count) => count,
            Err(error) => {
                warn!(member = %member.name, error = %error, "failed to count pending inbox messages");
                0
            }
        };
        let triage_backlog = match direct_reports.get(&member.name) {
            Some(reports) => {
                match delivered_direct_report_triage_count(&inbox_root, &member.name, reports) {
                    Ok(count) => count,
                    Err(error) => {
                        warn!(member = %member.name, error = %error, "failed to compute triage backlog");
                        0
                    }
                }
            }
            None => 0,
        };
        let member_owned_tasks = owned_task_buckets
            .get(&member.name)
            .cloned()
            .unwrap_or_default();

        let label = if globally_paused {
            compose_pane_status_label(PaneStatusLabelArgs {
                state,
                pending_inbox,
                triage_backlog,
                active_task_ids: &member_owned_tasks.active,
                review_task_ids: &member_owned_tasks.review,
                globally_paused: true,
                nudge_status: "",
                standup_status: "",
            })
        } else {
            let nudge_str = format_nudge_status(nudges.get(&member.name));
            let standup_str = standup_interval_for_member(&member.name)
                .map(|standup_interval| {
                    format_standup_status(
                        last_standup.get(&member.name).copied(),
                        standup_interval,
                        paused_standups.contains(&member.name),
                    )
                })
                .unwrap_or_default();
            compose_pane_status_label(PaneStatusLabelArgs {
                state,
                pending_inbox,
                triage_backlog,
                active_task_ids: &member_owned_tasks.active,
                review_task_ids: &member_owned_tasks.review,
                globally_paused: false,
                nudge_status: &nudge_str,
                standup_status: &standup_str,
            })
        };

        let _ = Command::new("tmux")
            .args(["set-option", "-p", "-t", pane_id, "@batty_status", &label])
            .output();
    }
}

pub(crate) fn format_nudge_status(schedule: Option<&NudgeSchedule>) -> String {
    let Some(schedule) = schedule else {
        return String::new();
    };

    if schedule.fired_this_idle {
        return " #[fg=magenta]nudge sent#[default]".to_string();
    }

    if schedule.paused {
        return " #[fg=244]nudge paused#[default]".to_string();
    }

    let Some(idle_since) = schedule.idle_since else {
        return String::new();
    };

    let elapsed = idle_since.elapsed();
    if elapsed < schedule.interval {
        let remaining = schedule.interval - elapsed;
        let mins = remaining.as_secs() / 60;
        let secs = remaining.as_secs() % 60;
        format!(" #[fg=magenta]nudge {mins}:{secs:02}#[default]")
    } else {
        " #[fg=magenta]nudge now#[default]".to_string()
    }
}

fn format_inbox_status(pending_count: usize) -> String {
    if pending_count == 0 {
        " #[fg=244]inbox 0#[default]".to_string()
    } else {
        format!(" #[fg=colour214,bold]inbox {pending_count}#[default]")
    }
}

fn format_active_task_status(active_task_ids: &[u32]) -> String {
    match active_task_ids {
        [] => String::new(),
        [task_id] => format!(" #[fg=green,bold]task {task_id}#[default]"),
        _ => format!(" #[fg=green,bold]tasks {}#[default]", active_task_ids.len()),
    }
}

fn format_review_task_status(review_task_ids: &[u32]) -> String {
    match review_task_ids {
        [] => String::new(),
        [task_id] => format!(" #[fg=blue,bold]review {task_id}#[default]"),
        _ => format!(" #[fg=blue,bold]review {}#[default]", review_task_ids.len()),
    }
}

pub(crate) struct PaneStatusLabelArgs<'a> {
    pub(crate) state: MemberState,
    pub(crate) pending_inbox: usize,
    pub(crate) triage_backlog: usize,
    pub(crate) active_task_ids: &'a [u32],
    pub(crate) review_task_ids: &'a [u32],
    pub(crate) globally_paused: bool,
    pub(crate) nudge_status: &'a str,
    pub(crate) standup_status: &'a str,
}

pub(crate) fn compose_pane_status_label(args: PaneStatusLabelArgs<'_>) -> String {
    let PaneStatusLabelArgs {
        state,
        pending_inbox,
        triage_backlog,
        active_task_ids,
        review_task_ids,
        globally_paused,
        nudge_status,
        standup_status,
    } = args;
    let state_str = match state {
        MemberState::Idle => "#[fg=yellow]idle#[default]",
        MemberState::Working => "#[fg=cyan]working#[default]",
    };
    let inbox_str = format_inbox_status(pending_inbox);
    let triage_str = if triage_backlog > 0 {
        format!(" #[fg=red,bold]triage {triage_backlog}#[default]")
    } else {
        String::new()
    };
    let active_task_str = format_active_task_status(active_task_ids);
    let review_task_str = format_review_task_status(review_task_ids);

    if globally_paused {
        return format!(
            "{state_str}{inbox_str}{triage_str}{active_task_str}{review_task_str} #[fg=red]PAUSED#[default]"
        );
    }

    format!(
        "{state_str}{inbox_str}{triage_str}{active_task_str}{review_task_str}{nudge_status}{standup_status}"
    )
}

pub(crate) fn format_standup_status(
    last_standup: Option<Instant>,
    interval: Duration,
    paused: bool,
) -> String {
    if paused {
        return " #[fg=244]standup paused#[default]".to_string();
    }

    let Some(last_standup) = last_standup else {
        return String::new();
    };

    let elapsed = last_standup.elapsed();
    if elapsed < interval {
        let remaining = interval - elapsed;
        let mins = remaining.as_secs() / 60;
        let secs = remaining.as_secs() % 60;
        format!(" #[fg=blue]standup {mins}:{secs:02}#[default]")
    } else {
        " #[fg=blue]standup now#[default]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::team::config::RoleType;
    use crate::team::events::{EventSink, TeamEvent};
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox::InboxMessage;

    fn engineer(name: &str) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        }
    }

    fn manager(name: &str) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type: RoleType::Manager,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        }
    }

    fn user_member(name: &str) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }
    }

    fn board_dir(project_root: &Path) -> std::path::PathBuf {
        project_root
            .join(".batty")
            .join("team_config")
            .join("board")
    }

    fn write_board_task(project_root: &Path, filename: &str, frontmatter: &str) {
        let tasks_dir = board_dir(project_root).join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join(filename),
            format!("---\n{frontmatter}class: standard\n---\n"),
        )
        .unwrap();
    }

    #[test]
    fn build_team_status_rows_marks_user_and_stopped_session() {
        let members = vec![engineer("eng-1"), user_member("human")];
        let rows = build_team_status_rows(
            &members,
            false,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "stopped");
        assert_eq!(rows[0].runtime_label, None);
        assert_eq!(rows[1].state, "user");
        assert_eq!(rows[1].role_type, "User");
        assert_eq!(rows[1].agent, None);
    }

    #[test]
    fn build_team_status_rows_promotes_idle_member_with_triage_backlog() {
        let members = vec![manager("manager")];
        let runtime_statuses = HashMap::from([(
            "manager".to_string(),
            RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: None,
                label: Some("idle".to_string()),
            },
        )]);
        let triage_backlog_counts = HashMap::from([("manager".to_string(), 2usize)]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &triage_backlog_counts,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "triaging");
        assert_eq!(rows[0].signal.as_deref(), Some("needs triage (2)"));
    }

    #[test]
    fn build_team_status_rows_promotes_idle_member_with_review_backlog() {
        let members = vec![manager("manager")];
        let runtime_statuses = HashMap::from([(
            "manager".to_string(),
            RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: Some("nudge paused".to_string()),
                label: Some("idle".to_string()),
            },
        )]);
        let owned_task_buckets = HashMap::from([(
            "manager".to_string(),
            OwnedTaskBuckets {
                active: Vec::new(),
                review: vec![41, 42],
            },
        )]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &owned_task_buckets,
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "reviewing");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("nudge paused, needs review (2)")
        );
    }

    #[test]
    fn build_team_status_rows_defaults_to_starting_when_runtime_missing() {
        let members = vec![engineer("eng-1")];
        let rows = build_team_status_rows(
            &members,
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "starting");
        assert_eq!(rows[0].runtime_label, None);
    }

    #[test]
    fn build_team_status_health_counts_non_user_members_and_sorts_unhealthy() {
        let rows = vec![
            TeamStatusRow {
                name: "human".to_string(),
                role: "user".to_string(),
                role_type: "User".to_string(),
                agent: None,
                reports_to: None,
                state: "user".to_string(),
                pending_inbox: 9,
                triage_backlog: 9,
                active_owned_tasks: Vec::new(),
                review_owned_tasks: Vec::new(),
                signal: None,
                runtime_label: None,
                health: AgentHealthSummary::default(),
                health_summary: "-".to_string(),
            },
            TeamStatusRow {
                name: "eng-2".to_string(),
                role: "engineer".to_string(),
                role_type: "Engineer".to_string(),
                agent: Some("codex".to_string()),
                reports_to: Some("manager".to_string()),
                state: "working".to_string(),
                pending_inbox: 1,
                triage_backlog: 2,
                active_owned_tasks: vec![2],
                review_owned_tasks: Vec::new(),
                signal: None,
                runtime_label: Some("working".to_string()),
                health: AgentHealthSummary {
                    restart_count: 1,
                    context_exhaustion_count: 0,
                    delivery_failure_count: 0,
                    task_elapsed_secs: None,
                },
                health_summary: "r1".to_string(),
            },
            TeamStatusRow {
                name: "eng-1".to_string(),
                role: "engineer".to_string(),
                role_type: "Engineer".to_string(),
                agent: Some("codex".to_string()),
                reports_to: Some("manager".to_string()),
                state: "reviewing".to_string(),
                pending_inbox: 3,
                triage_backlog: 1,
                active_owned_tasks: Vec::new(),
                review_owned_tasks: vec![1],
                signal: None,
                runtime_label: Some("idle".to_string()),
                health: AgentHealthSummary {
                    restart_count: 0,
                    context_exhaustion_count: 1,
                    delivery_failure_count: 1,
                    task_elapsed_secs: None,
                },
                health_summary: "c1 d1".to_string(),
            },
        ];

        let health = build_team_status_health(&rows, true, false);
        assert_eq!(health.member_count, 2);
        assert_eq!(health.active_member_count, 2);
        assert_eq!(health.pending_inbox_count, 4);
        assert_eq!(health.triage_backlog_count, 3);
        assert_eq!(
            health.unhealthy_members,
            vec!["eng-1".to_string(), "eng-2".to_string()]
        );
    }

    #[test]
    fn board_status_task_queues_returns_empty_when_board_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert!(active_tasks.is_empty());
        assert!(review_queue.is_empty());
    }

    #[test]
    fn owned_task_buckets_routes_review_items_to_manager() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "003-review.md",
            "id: 3\ntitle: Review one\nstatus: review\npriority: high\nclaimed_by: eng-2\n",
        );
        write_board_task(
            tmp.path(),
            "004-review.md",
            "id: 4\ntitle: Review two\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
        );
        write_board_task(
            tmp.path(),
            "005-active.md",
            "id: 5\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-2\n",
        );

        let buckets = owned_task_buckets(
            tmp.path(),
            &[manager("manager"), engineer("eng-1"), engineer("eng-2")],
        );

        assert_eq!(
            buckets.get("manager"),
            Some(&OwnedTaskBuckets {
                active: Vec::new(),
                review: vec![3, 4],
            })
        );
        assert_eq!(
            buckets.get("eng-2"),
            Some(&OwnedTaskBuckets {
                active: vec![5],
                review: Vec::new(),
            })
        );
    }

    #[test]
    fn compute_metrics_returns_default_when_board_is_missing() {
        let metrics =
            compute_metrics(&tempfile::tempdir().unwrap().path().join("board"), &[]).unwrap();

        assert_eq!(metrics, WorkflowMetrics::default());
    }

    #[test]
    fn compute_metrics_returns_default_when_board_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(board_dir(tmp.path()).join("tasks")).unwrap();

        let metrics = compute_metrics(&board_dir(tmp.path()), &[]).unwrap();
        assert_eq!(metrics, WorkflowMetrics::default());
    }

    #[test]
    fn compute_metrics_counts_workflow_states_and_idle_runnable() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "001-runnable.md",
            "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
        );
        write_board_task(
            tmp.path(),
            "002-blocked.md",
            "id: 2\ntitle: Blocked\nstatus: blocked\npriority: medium\n",
        );
        write_board_task(
            tmp.path(),
            "003-review.md",
            "id: 3\ntitle: Review\nstatus: review\npriority: medium\nclaimed_by: eng-1\n",
        );
        write_board_task(
            tmp.path(),
            "004-in-progress.md",
            "id: 4\ntitle: In progress\nstatus: in-progress\npriority: medium\nclaimed_by: eng-1\n",
        );
        write_board_task(
            tmp.path(),
            "005-claimed.md",
            "id: 5\ntitle: Claimed todo\nstatus: todo\npriority: low\nclaimed_by: eng-3\n",
        );
        write_board_task(
            tmp.path(),
            "006-waiting.md",
            "id: 6\ntitle: Waiting\nstatus: todo\npriority: low\ndepends_on:\n  - 7\n",
        );
        write_board_task(
            tmp.path(),
            "007-parent.md",
            "id: 7\ntitle: Parent\nstatus: in-progress\npriority: low\nclaimed_by: eng-3\n",
        );

        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::deliver_to_inbox(
            &inbox_root,
            &InboxMessage::new_send("manager", "eng-2", "please pick this up"),
        )
        .unwrap();

        let metrics = compute_metrics(
            &board_dir(tmp.path()),
            &[
                engineer("eng-1"),
                engineer("eng-2"),
                engineer("eng-3"),
                engineer("eng-4"),
            ],
        )
        .unwrap();

        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.blocked_count, 1);
        assert_eq!(metrics.in_review_count, 1);
        assert_eq!(metrics.in_progress_count, 2);
        assert_eq!(metrics.idle_with_runnable, vec!["eng-4".to_string()]);
        assert!(metrics.oldest_review_age_secs.is_some());
        assert!(metrics.oldest_assignment_age_secs.is_some());
    }

    #[test]
    fn workflow_metrics_section_returns_none_when_mode_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        fs::create_dir_all(&team_config_dir).unwrap();
        fs::write(team_config_dir.join("team.yaml"), "team: test\n").unwrap();

        assert!(workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).is_none());
    }

    #[test]
    fn workflow_metrics_section_returns_formatted_metrics_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        fs::create_dir_all(&team_config_dir).unwrap();
        fs::write(
            team_config_dir.join("team.yaml"),
            "team: test\nworkflow_mode: hybrid\n",
        )
        .unwrap();
        write_board_task(
            tmp.path(),
            "001-runnable.md",
            "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
        );

        let (formatted, metrics) =
            workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

        assert!(formatted.contains("Workflow Metrics"));
        assert!(formatted.contains("Runnable: 1"));
        assert_eq!(metrics.runnable_count, 1);
    }

    #[test]
    fn build_team_status_json_report_serializes_machine_readable_json() {
        let report = build_team_status_json_report(TeamStatusJsonReportInput {
            team: "test".to_string(),
            session: "batty-test".to_string(),
            session_running: true,
            paused: false,
            workflow_metrics: Some(WorkflowMetrics {
                runnable_count: 1,
                ..WorkflowMetrics::default()
            }),
            active_tasks: Vec::new(),
            review_queue: Vec::new(),
            members: vec![TeamStatusRow {
                name: "eng-1".to_string(),
                role: "engineer".to_string(),
                role_type: "Engineer".to_string(),
                agent: Some("codex".to_string()),
                reports_to: Some("manager".to_string()),
                state: "idle".to_string(),
                pending_inbox: 0,
                triage_backlog: 0,
                active_owned_tasks: Vec::new(),
                review_owned_tasks: Vec::new(),
                signal: None,
                runtime_label: Some("idle".to_string()),
                health: AgentHealthSummary::default(),
                health_summary: "-".to_string(),
            }],
        });

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["team"], "test");
        assert_eq!(json["running"], true);
        assert_eq!(json["health"]["member_count"], 1);
        assert_eq!(json["workflow_metrics"]["runnable_count"], 1);
        assert!(json["members"].is_array());
    }

    #[test]
    fn parse_assigned_task_id_accepts_plain_numeric_values() {
        assert_eq!(parse_assigned_task_id("42"), Some(42));
    }

    #[test]
    fn parse_assigned_task_id_extracts_task_hash_values() {
        assert_eq!(
            parse_assigned_task_id("Task #119: expand coverage"),
            Some(119)
        );
        assert_eq!(parse_assigned_task_id("working on #508 next"), Some(508));
    }

    #[test]
    fn parse_assigned_task_id_rejects_values_without_leading_digits() {
        assert_eq!(parse_assigned_task_id("Task #abc"), None);
        assert_eq!(parse_assigned_task_id("no task here"), None);
    }

    #[test]
    fn format_health_duration_formats_seconds() {
        assert_eq!(format_health_duration(59), "59s");
    }

    #[test]
    fn format_health_duration_formats_minutes() {
        assert_eq!(format_health_duration(60), "1m");
    }

    #[test]
    fn format_health_duration_formats_hours() {
        assert_eq!(format_health_duration(3_600), "1h");
    }

    #[test]
    fn format_health_duration_formats_days() {
        assert_eq!(format_health_duration(86_400), "1d");
    }

    #[test]
    fn merge_status_signal_combines_existing_triage_and_review_signals() {
        assert_eq!(
            merge_status_signal(Some("nudged".to_string()), 2, 1),
            Some("nudged, needs triage (2), needs review (1)".to_string())
        );
    }

    #[test]
    fn merge_status_signal_returns_none_when_no_signals_exist() {
        assert_eq!(merge_status_signal(None, 0, 0), None);
    }

    #[test]
    fn agent_health_by_member_defaults_without_events_or_state() {
        let tmp = tempfile::tempdir().unwrap();
        let health = agent_health_by_member(tmp.path(), &[engineer("eng-1"), engineer("eng-2")]);

        assert_eq!(health.get("eng-1"), Some(&AgentHealthSummary::default()));
        assert_eq!(health.get("eng-2"), Some(&AgentHealthSummary::default()));
    }

    #[test]
    fn board_status_task_queues_split_active_and_review_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("041-active.md"),
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/task-41\nclass: standard\n---\n",
        )
        .unwrap();
        fs::write(
            tasks_dir.join("042-review.md"),
            "---\nid: 42\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-2\nreview_owner: manager\nnext_action: review now\nclass: standard\n---\n",
        )
        .unwrap();
        fs::write(
            tasks_dir.join("043-done.md"),
            "---\nid: 43\ntitle: Done task\nstatus: done\npriority: low\nclass: standard\n---\n",
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert_eq!(active_tasks.len(), 1);
        assert_eq!(active_tasks[0].id, 41);
        assert_eq!(active_tasks[0].branch.as_deref(), Some("eng-1/task-41"));
        assert_eq!(review_queue.len(), 1);
        assert_eq!(review_queue[0].id, 42);
        assert_eq!(review_queue[0].review_owner.as_deref(), Some("manager"));
        assert_eq!(review_queue[0].next_action.as_deref(), Some("review now"));
    }

    #[test]
    fn build_team_status_json_report_includes_health_and_queues() {
        let report = build_team_status_json_report(TeamStatusJsonReportInput {
            team: "test".to_string(),
            session: "batty-test".to_string(),
            session_running: true,
            paused: true,
            workflow_metrics: Some(WorkflowMetrics {
                runnable_count: 2,
                blocked_count: 1,
                in_review_count: 1,
                in_progress_count: 3,
                idle_with_runnable: vec!["eng-2".to_string()],
                oldest_review_age_secs: Some(60),
                oldest_assignment_age_secs: Some(120),
            }),
            active_tasks: vec![StatusTaskEntry {
                id: 41,
                title: "Active task".to_string(),
                status: "in-progress".to_string(),
                priority: "high".to_string(),
                claimed_by: Some("eng-1".to_string()),
                review_owner: None,
                blocked_on: None,
                branch: Some("eng-1/task-41".to_string()),
                worktree_path: None,
                commit: None,
                next_action: None,
            }],
            review_queue: vec![StatusTaskEntry {
                id: 42,
                title: "Review task".to_string(),
                status: "review".to_string(),
                priority: "medium".to_string(),
                claimed_by: Some("eng-2".to_string()),
                review_owner: Some("manager".to_string()),
                blocked_on: None,
                branch: None,
                worktree_path: None,
                commit: None,
                next_action: Some("review now".to_string()),
            }],
            members: vec![
                TeamStatusRow {
                    name: "eng-1".to_string(),
                    role: "engineer".to_string(),
                    role_type: "Engineer".to_string(),
                    agent: Some("codex".to_string()),
                    reports_to: Some("manager".to_string()),
                    state: "working".to_string(),
                    pending_inbox: 2,
                    triage_backlog: 0,
                    active_owned_tasks: vec![41],
                    review_owned_tasks: vec![],
                    signal: None,
                    runtime_label: Some("working".to_string()),
                    health: AgentHealthSummary {
                        restart_count: 1,
                        context_exhaustion_count: 0,
                        delivery_failure_count: 0,
                        task_elapsed_secs: Some(30),
                    },
                    health_summary: "r1 t30s".to_string(),
                },
                TeamStatusRow {
                    name: "eng-2".to_string(),
                    role: "engineer".to_string(),
                    role_type: "Engineer".to_string(),
                    agent: Some("codex".to_string()),
                    reports_to: Some("manager".to_string()),
                    state: "idle".to_string(),
                    pending_inbox: 1,
                    triage_backlog: 2,
                    active_owned_tasks: vec![],
                    review_owned_tasks: vec![42],
                    signal: Some("needs review (1)".to_string()),
                    runtime_label: Some("idle".to_string()),
                    health: AgentHealthSummary::default(),
                    health_summary: "-".to_string(),
                },
            ],
        });

        assert_eq!(report.team, "test");
        assert_eq!(report.active_tasks.len(), 1);
        assert_eq!(report.review_queue.len(), 1);
        assert!(report.paused);
        assert_eq!(report.health.member_count, 2);
        assert_eq!(report.health.active_member_count, 1);
        assert_eq!(report.health.pending_inbox_count, 3);
        assert_eq!(report.health.triage_backlog_count, 2);
        assert_eq!(report.health.unhealthy_members, vec!["eng-1".to_string()]);
        assert_eq!(report.workflow_metrics.unwrap().runnable_count, 2);
    }

    #[test]
    fn format_standup_status_marks_paused_while_member_is_working() {
        assert_eq!(
            format_standup_status(Some(Instant::now()), Duration::from_secs(600), true),
            " #[fg=244]standup paused#[default]"
        );
    }

    #[test]
    fn format_nudge_status_marks_paused_while_member_is_working() {
        let schedule = NudgeSchedule {
            text: "check in".to_string(),
            interval: Duration::from_secs(600),
            idle_since: None,
            fired_this_idle: false,
            paused: true,
        };

        assert_eq!(
            format_nudge_status(Some(&schedule)),
            " #[fg=244]nudge paused#[default]"
        );
    }

    #[test]
    fn compose_pane_status_label_shows_pending_inbox_count() {
        let label = compose_pane_status_label(PaneStatusLabelArgs {
            state: MemberState::Idle,
            pending_inbox: 3,
            triage_backlog: 2,
            active_task_ids: &[191],
            review_task_ids: &[193, 194],
            globally_paused: false,
            nudge_status: " #[fg=magenta]nudge 0:30#[default]",
            standup_status: "",
        });
        assert!(label.contains("idle"));
        assert!(label.contains("inbox 3"));
        assert!(label.contains("triage 2"));
        assert!(label.contains("task 191"));
        assert!(label.contains("review 2"));
        assert!(label.contains("nudge 0:30"));
    }

    #[test]
    fn compose_pane_status_label_shows_zero_inbox_and_pause_state() {
        let label = compose_pane_status_label(PaneStatusLabelArgs {
            state: MemberState::Working,
            pending_inbox: 0,
            triage_backlog: 0,
            active_task_ids: &[],
            review_task_ids: &[],
            globally_paused: true,
            nudge_status: "",
            standup_status: "",
        });
        assert!(label.contains("working"));
        assert!(label.contains("inbox 0"));
        assert!(label.contains("PAUSED"));
    }

    #[test]
    fn format_agent_health_summary_compacts_metrics() {
        let summary = format_agent_health_summary(&AgentHealthSummary {
            restart_count: 2,
            context_exhaustion_count: 1,
            delivery_failure_count: 3,
            task_elapsed_secs: Some(750),
        });

        assert_eq!(summary, "r2 c1 d3 t12m");
        assert_eq!(
            format_agent_health_summary(&AgentHealthSummary::default()),
            "-"
        );
    }

    #[test]
    fn agent_health_by_member_aggregates_events_and_active_task_elapsed() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();

        let mut assigned = TeamEvent::task_assigned("eng-1", "Task #42: fix it");
        assigned.ts = now_unix().saturating_sub(600);
        sink.emit(assigned).unwrap();

        let mut restarted = TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 2);
        restarted.ts = now_unix().saturating_sub(590);
        sink.emit(restarted).unwrap();

        let mut exhausted = TeamEvent::context_exhausted("eng-1", Some(42), Some(4_096));
        exhausted.ts = now_unix().saturating_sub(580);
        sink.emit(exhausted).unwrap();

        let mut delivery_failed =
            TeamEvent::delivery_failed("eng-1", "manager", "message delivery failed after retries");
        delivery_failed.ts = now_unix().saturating_sub(570);
        sink.emit(delivery_failed).unwrap();

        let daemon_state = serde_json::json!({
            "active_tasks": {"eng-1": 42},
            "retry_counts": {"eng-1": 1}
        });
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            daemon_state_path(tmp.path()),
            serde_json::to_vec_pretty(&daemon_state).unwrap(),
        )
        .unwrap();

        let health = agent_health_by_member(tmp.path(), &[engineer("eng-1"), engineer("eng-2")]);
        let eng_1 = health.get("eng-1").unwrap();
        assert_eq!(eng_1.restart_count, 2);
        assert_eq!(eng_1.context_exhaustion_count, 1);
        assert_eq!(eng_1.delivery_failure_count, 1);
        assert!(eng_1.task_elapsed_secs.unwrap() >= 600);
        assert_eq!(health.get("eng-2").unwrap(), &AgentHealthSummary::default());
    }
}
