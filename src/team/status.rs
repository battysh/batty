use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::task;

use super::config::{self, RoleType};
use super::daemon::NudgeSchedule;
use super::hierarchy::MemberInstance;
use super::inbox;
use super::standup::MemberState;
use super::{
    TRIAGE_RESULT_FRESHNESS_SECONDS, now_unix, pause_marker_path, team_config_dir, team_config_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeMemberStatus {
    pub(crate) state: String,
    pub(crate) signal: Option<String>,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowMetrics {
    pub runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub in_progress_count: u32,
    pub idle_with_runnable: Vec<String>,
    pub oldest_review_age_secs: Option<u64>,
    pub oldest_assignment_age_secs: Option<u64>,
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

pub(crate) fn update_pane_status_labels(
    project_root: &Path,
    members: &[MemberInstance],
    pane_map: &HashMap<String, String>,
    states: &HashMap<String, MemberState>,
    nudges: &HashMap<String, NudgeSchedule>,
    last_standup: &HashMap<String, Instant>,
    paused_standups: &HashSet<String>,
    standup_interval_for_member: impl Fn(&str) -> Option<Duration>,
) {
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
            compose_pane_status_label(
                state,
                pending_inbox,
                triage_backlog,
                &member_owned_tasks.active,
                &member_owned_tasks.review,
                true,
                "",
                "",
            )
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
            compose_pane_status_label(
                state,
                pending_inbox,
                triage_backlog,
                &member_owned_tasks.active,
                &member_owned_tasks.review,
                false,
                &nudge_str,
                &standup_str,
            )
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

pub(crate) fn compose_pane_status_label(
    state: MemberState,
    pending_inbox: usize,
    triage_backlog: usize,
    active_task_ids: &[u32],
    review_task_ids: &[u32],
    globally_paused: bool,
    nudge_status: &str,
    standup_status: &str,
) -> String {
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
        let label = compose_pane_status_label(
            MemberState::Idle,
            3,
            2,
            &[191],
            &[193, 194],
            false,
            " #[fg=magenta]nudge 0:30#[default]",
            "",
        );
        assert!(label.contains("idle"));
        assert!(label.contains("inbox 3"));
        assert!(label.contains("triage 2"));
        assert!(label.contains("task 191"));
        assert!(label.contains("review 2"));
        assert!(label.contains("nudge 0:30"));
    }

    #[test]
    fn compose_pane_status_label_shows_zero_inbox_and_pause_state() {
        let label = compose_pane_status_label(MemberState::Working, 0, 0, &[], &[], true, "", "");
        assert!(label.contains("working"));
        assert!(label.contains("inbox 0"));
        assert!(label.contains("PAUSED"));
    }
}
