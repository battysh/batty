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
use super::daemon_mgmt::{PersistedWatchdogState, watchdog_state_path};
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
    pub(crate) worktree_staleness: Option<u32>,
    pub(crate) health: AgentHealthSummary,
    pub(crate) health_summary: String,
    pub(crate) eta: String,
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
    pub(crate) supervisory_digest_count: u32,
    pub(crate) dispatch_fallback_count: u32,
    pub(crate) dispatch_fallback_reason: Option<String>,
    pub(crate) task_elapsed_secs: Option<u64>,
    pub(crate) stall_summary: Option<String>,
    pub(crate) stall_reason: Option<String>,
    pub(crate) backend_health: crate::agent::BackendHealth,
}

impl AgentHealthSummary {
    pub(crate) fn record_supervisory_digest(&mut self) {
        self.supervisory_digest_count += 1;
    }

    pub(crate) fn record_dispatch_fallback(&mut self, reason: Option<&str>) {
        self.dispatch_fallback_count += 1;
        self.dispatch_fallback_reason = reason.map(str::to_string);
    }

    pub(crate) fn record_supervisory_stall(&mut self, reason: Option<&str>, summary: Option<&str>) {
        self.stall_reason = reason.map(str::to_string);
        self.stall_summary = summary.map(str::to_string);
    }

    pub(crate) fn has_supervisory_warning(&self) -> bool {
        self.stall_reason.is_some() || self.stall_summary.is_some()
    }

    pub(crate) fn has_operator_warning(&self) -> bool {
        self.restart_count > 0
            || self.context_exhaustion_count > 0
            || self.delivery_failure_count > 0
            || self.has_supervisory_warning()
            || !self.backend_health.is_healthy()
    }

    #[allow(dead_code)]
    pub(crate) fn supervisory_status_token(&self) -> Option<String> {
        self.supervisory_status_token_for_role(None)
    }

    pub(crate) fn supervisory_status_token_for_role(
        &self,
        role_type: Option<RoleType>,
    ) -> Option<String> {
        if !self.has_supervisory_warning() {
            return None;
        }

        let role_label = role_type.and_then(supervisory_role_label).or_else(|| {
            self.stall_reason
                .as_deref()
                .and_then(supervisory_role_label_from_reason)
        });
        Some(match self.stall_reason.as_deref() {
            Some(reason) => supervisory_status_token(reason, role_label),
            None => role_label
                .map(|label| format!("stall:{label}"))
                .unwrap_or_else(|| "stall".to_string()),
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PersistedDaemonHealthState {
    #[serde(default)]
    active_tasks: HashMap<String, u32>,
    #[serde(default)]
    retry_counts: HashMap<String, u32>,
    #[serde(default)]
    optional_subsystem_backoff: HashMap<String, u32>,
    #[serde(default)]
    optional_subsystem_disabled_remaining_secs: HashMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OptionalSubsystemStatus {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) recent_errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled_remaining_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backoff_stage: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct WorkflowMetrics {
    pub runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub in_progress_count: u32,
    pub stale_in_progress_count: u32,
    pub aged_todo_count: u32,
    pub stale_review_count: u32,
    pub idle_with_runnable: Vec<String>,
    pub top_runnable_tasks: Vec<String>,
    pub oldest_review_age_secs: Option<u64>,
    pub oldest_assignment_age_secs: Option<u64>,
    // Review pipeline metrics (computed from event log)
    pub auto_merge_count: u32,
    pub manual_merge_count: u32,
    pub auto_merge_rate: Option<f64>,
    pub rework_count: u32,
    pub rework_rate: Option<f64>,
    pub review_nudge_count: u32,
    pub review_escalation_count: u32,
    pub avg_review_latency_secs: Option<f64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch_mismatch: Option<String>,
    pub(crate) next_action: Option<String>,
    pub(crate) test_summary: Option<String>,
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
pub(crate) struct WatchdogStatus {
    pub(crate) state: String,
    pub(crate) restart_count: u32,
    pub(crate) current_backoff_secs: Option<u64>,
    pub(crate) last_exit_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct TeamStatusJsonReport {
    pub(crate) team: String,
    pub(crate) session: String,
    pub(crate) running: bool,
    pub(crate) paused: bool,
    pub(crate) watchdog: WatchdogStatus,
    pub(crate) health: TeamStatusHealth,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) optional_subsystems: Option<Vec<OptionalSubsystemStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) engineer_profiles:
        Option<Vec<crate::team::telemetry_db::EngineerPerformanceProfileRow>>,
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_team_status_rows(
    members: &[MemberInstance],
    session_running: bool,
    runtime_statuses: &HashMap<String, RuntimeMemberStatus>,
    pending_inbox_counts: &HashMap<String, usize>,
    triage_backlog_counts: &HashMap<String, usize>,
    owned_task_buckets: &HashMap<String, OwnedTaskBuckets>,
    branch_mismatches: &HashMap<String, String>,
    worktree_staleness: &HashMap<String, u32>,
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

            let health = agent_health.get(&member.name).cloned().unwrap_or_default();
            let signal = merge_status_signal(
                signal,
                branch_mismatches.get(&member.name).cloned(),
                health.stall_summary.clone(),
                triage_backlog,
                review_backlog,
            );
            let health_summary =
                format_agent_health_summary_for_role(&health, Some(member.role_type));

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
                worktree_staleness: worktree_staleness.get(&member.name).copied(),
                health,
                health_summary,
                eta: "-".to_string(),
            }
        })
        .collect()
}

fn task_has_active_claim(task: &task::Task, member_name: &str) -> bool {
    task.claimed_by.as_deref() == Some(member_name)
        && classify_owned_task_status(task.status.as_str()) == Some(true)
}

fn managed_task_branch(member_name: &str, task: &task::Task) -> String {
    task.branch
        .clone()
        .unwrap_or_else(|| format!("{member_name}/{}", task.id))
}

fn format_branch_mismatch_signal(
    task_id: u32,
    current_branch: &str,
    expected_branch: &str,
) -> String {
    if current_branch == "HEAD" {
        format!(
            "branch mismatch (#{} detached HEAD; expected {})",
            task_id, expected_branch
        )
    } else {
        format!(
            "branch mismatch (#{} on {}; expected {})",
            task_id, current_branch, expected_branch
        )
    }
}

fn select_authoritative_claimed_task<'a>(
    member_name: &str,
    current_branch: &str,
    claimed_tasks: &[&'a task::Task],
) -> Option<&'a task::Task> {
    let mut branch_matches = claimed_tasks
        .iter()
        .copied()
        .filter(|task| managed_task_branch(member_name, task) == current_branch);
    if let Some(task) = branch_matches.next()
        && branch_matches.next().is_none()
    {
        return Some(task);
    }

    claimed_tasks.iter().copied().min_by_key(|task| task.id)
}

pub(crate) fn branch_mismatch_by_member(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return HashMap::new();
    }

    let tasks = match task::load_tasks_from_dir(&tasks_dir) {
        Ok(tasks) => tasks,
        Err(error) => {
            warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for branch mismatch status");
            return HashMap::new();
        }
    };

    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer && member.use_worktrees)
        .filter_map(|member| {
            let worktree_dir = project_root
                .join(".batty")
                .join("worktrees")
                .join(&member.name);
            if !worktree_dir.is_dir() {
                return None;
            }

            let current_branch =
                git_stdout_raw(&worktree_dir, ["rev-parse", "--abbrev-ref", "HEAD"])?;
            let claimed_tasks = tasks
                .iter()
                .filter(|task| task_has_active_claim(task, &member.name))
                .collect::<Vec<_>>();
            let task =
                select_authoritative_claimed_task(&member.name, &current_branch, &claimed_tasks)?;
            let expected_branch = managed_task_branch(&member.name, task);
            (current_branch != expected_branch).then(|| {
                (
                    member.name.clone(),
                    format_branch_mismatch_signal(task.id, &current_branch, &expected_branch),
                )
            })
        })
        .collect()
}

pub(crate) fn worktree_staleness_by_member(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, u32> {
    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer && member.use_worktrees)
        .filter_map(|member| {
            let worktree_dir = project_root
                .join(".batty")
                .join("worktrees")
                .join(&member.name);
            if !worktree_dir.exists() {
                return None;
            }

            match super::task_loop::worktree_commits_behind_main(&worktree_dir) {
                Ok(count) => Some((member.name.clone(), count)),
                Err(error) => {
                    warn!(
                        member = %member.name,
                        error = %error,
                        "failed to measure engineer worktree staleness for status"
                    );
                    None
                }
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
                    "supervisory_digest_emitted" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .record_supervisory_digest();
                    }
                    "dispatch_fallback_used" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .record_dispatch_fallback(event.reason.as_deref());
                    }
                    "stall_detected" => {
                        let role_type = members
                            .iter()
                            .find(|member| member.name == role)
                            .map(|member| member.role_type);
                        let is_supervisory = event
                            .task
                            .as_deref()
                            .is_some_and(|task| task.starts_with("supervisory::"))
                            || event
                                .reason
                                .as_deref()
                                .is_some_and(is_supervisory_stall_reason)
                            || role_type.is_some_and(|role_type| {
                                matches!(role_type, RoleType::Architect | RoleType::Manager)
                            });
                        if is_supervisory {
                            let fallback_summary = fallback_supervisory_stall_summary(
                                event.reason.as_deref(),
                                event.uptime_secs,
                                role_type,
                            );
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .record_supervisory_stall(
                                    event.reason.as_deref(),
                                    event.details.as_deref().or(fallback_summary.as_deref()),
                                );
                        }
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
                    "health_changed" => {
                        // Parse the latest backend health from the transition reason.
                        // Format is "prev→new", e.g. "healthy→unreachable".
                        if let Some(reason) = event.reason.as_deref() {
                            let new_state = reason.split('→').next_back().unwrap_or("healthy");
                            let health_val = match new_state {
                                "degraded" => crate::agent::BackendHealth::Degraded,
                                "unreachable" => crate::agent::BackendHealth::Unreachable,
                                _ => crate::agent::BackendHealth::Healthy,
                            };
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .backend_health = health_val;
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

pub(crate) fn load_optional_subsystem_statuses(
    project_root: &Path,
) -> Vec<OptionalSubsystemStatus> {
    let daemon_state = load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .ok()
        .flatten()
        .unwrap_or_default();
    let recent_errors = recent_optional_subsystem_errors(project_root);

    crate::team::daemon::optional_subsystem_names()
        .into_iter()
        .map(|name| {
            let disabled_remaining_secs = daemon_state
                .optional_subsystem_disabled_remaining_secs
                .get(name)
                .copied()
                .filter(|secs| *secs > 0);
            let recent_errors = recent_errors.get(name).copied().unwrap_or(0);
            let state = if disabled_remaining_secs.is_some() {
                "disabled"
            } else if recent_errors > 0 {
                "degraded"
            } else {
                "healthy"
            };
            OptionalSubsystemStatus {
                name: name.to_string(),
                state: state.to_string(),
                recent_errors,
                disabled_remaining_secs,
                backoff_stage: daemon_state.optional_subsystem_backoff.get(name).copied(),
                last_error: latest_optional_subsystem_error(project_root, name),
            }
        })
        .collect()
}

pub(crate) fn format_optional_subsystem_statuses(statuses: &[OptionalSubsystemStatus]) -> String {
    let mut lines = vec![
        "Optional Subsystems".to_string(),
        format!(
            "{:<12} {:<10} {:<12} {:<12} {}",
            "NAME", "STATE", "ERRORS/10M", "RETRY", "LAST ERROR"
        ),
    ];

    for status in statuses {
        let retry = status
            .disabled_remaining_secs
            .map(format_health_duration)
            .unwrap_or_else(|| "-".to_string());
        let last_error = status.last_error.as_deref().unwrap_or("-");
        lines.push(format!(
            "{:<12} {:<10} {:<12} {:<12} {}",
            status.name, status.state, status.recent_errors, retry, last_error
        ));
    }

    lines.join("\n")
}

fn recent_optional_subsystem_errors(project_root: &Path) -> HashMap<&'static str, usize> {
    let cutoff = now_unix().saturating_sub(600);
    let mut counts = HashMap::new();
    let Ok(events) = events::read_events(&team_events_path(project_root)) else {
        return counts;
    };

    for event in events {
        if event.event != "loop_step_error" || event.ts < cutoff {
            continue;
        }
        let Some(subsystem) = event
            .step
            .as_deref()
            .and_then(crate::team::daemon::optional_subsystem_for_step)
        else {
            continue;
        };
        *counts.entry(subsystem).or_insert(0) += 1;
    }

    counts
}

fn latest_optional_subsystem_error(project_root: &Path, subsystem: &str) -> Option<String> {
    events::read_events(&team_events_path(project_root))
        .ok()?
        .into_iter()
        .rev()
        .find(|event| {
            event.event == "loop_step_error"
                && event
                    .step
                    .as_deref()
                    .and_then(crate::team::daemon::optional_subsystem_for_step)
                    == Some(subsystem)
        })
        .and_then(|event| event.error)
}

pub(crate) fn load_watchdog_status(project_root: &Path, session_running: bool) -> WatchdogStatus {
    let path = watchdog_state_path(project_root);
    let persisted = if !path.exists() {
        PersistedWatchdogState::default()
    } else {
        match std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .and_then(|content| {
                serde_json::from_str::<PersistedWatchdogState>(&content)
                    .with_context(|| format!("failed to parse {}", path.display()))
            }) {
            Ok(state) => state,
            Err(error) => {
                warn!(error = %error, "failed to load watchdog state");
                PersistedWatchdogState::default()
            }
        }
    };

    let state = if !session_running {
        "stopped".to_string()
    } else if persisted.circuit_breaker_tripped {
        "circuit-open".to_string()
    } else if persisted.current_backoff_secs.is_some() {
        "restarting".to_string()
    } else {
        "running".to_string()
    };

    WatchdogStatus {
        state,
        restart_count: persisted.restart_count,
        current_backoff_secs: persisted.current_backoff_secs,
        last_exit_reason: persisted.last_exit_reason,
    }
}

pub(crate) fn format_watchdog_summary(watchdog: &WatchdogStatus) -> String {
    let mut parts = vec![
        watchdog.state.clone(),
        format!("r{}", watchdog.restart_count),
    ];
    if let Some(backoff_secs) = watchdog.current_backoff_secs {
        parts.push(format!("backoff={}s", backoff_secs));
    }
    if let Some(reason) = &watchdog.last_exit_reason {
        parts.push(reason.clone());
    }
    parts.join(" | ")
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

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn format_agent_health_summary(health: &AgentHealthSummary) -> String {
    format_agent_health_summary_for_role(health, None)
}

pub(crate) fn format_agent_health_summary_for_role(
    health: &AgentHealthSummary,
    role_type: Option<RoleType>,
) -> String {
    let mut parts = Vec::new();
    if !health.backend_health.is_healthy() {
        parts.push(format!("B:{}", health.backend_health.as_str()));
    }
    if health.restart_count > 0 {
        parts.push(format!("r{}", health.restart_count));
    }
    if health.context_exhaustion_count > 0 {
        parts.push(format!("c{}", health.context_exhaustion_count));
    }
    if health.delivery_failure_count > 0 {
        parts.push(format!("d{}", health.delivery_failure_count));
    }
    if health.supervisory_digest_count > 0 {
        parts.push(format!("sd{}", health.supervisory_digest_count));
    }
    if health.dispatch_fallback_count > 0 {
        let token = match health.dispatch_fallback_reason.as_deref() {
            Some(reason) => format!("fd{}:{reason}", health.dispatch_fallback_count),
            None => format!("fd{}", health.dispatch_fallback_count),
        };
        parts.push(token);
    }
    if let Some(supervisory_token) = health.supervisory_status_token_for_role(role_type) {
        parts.push(supervisory_token);
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

pub(crate) fn format_health_duration(task_elapsed_secs: u64) -> String {
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

fn fallback_supervisory_stall_summary(
    reason: Option<&str>,
    stall_secs: Option<u64>,
    role_type: Option<RoleType>,
) -> Option<String> {
    let role_label = role_type
        .and_then(supervisory_role_label)
        .or_else(|| reason.and_then(supervisory_role_label_from_reason))
        .unwrap_or("supervisory");
    match (reason, stall_secs) {
        (Some(reason), Some(stall_secs)) => Some(format!(
            "{role_label} stall: {} after {}",
            supervisory_reason_label(reason),
            format_health_duration(stall_secs)
        )),
        (Some(reason), None) => Some(format!(
            "{role_label} stall: {}",
            supervisory_reason_label(reason)
        )),
        (None, Some(stall_secs)) => Some(format!(
            "{role_label} stall after {}",
            format_health_duration(stall_secs)
        )),
        (None, None) => None,
    }
}

fn merge_status_signal(
    signal: Option<String>,
    branch_mismatch_signal: Option<String>,
    stall_signal: Option<String>,
    triage_backlog: usize,
    review_backlog: usize,
) -> Option<String> {
    let triage_signal = (triage_backlog > 0).then(|| format!("needs triage ({triage_backlog})"));
    let review_signal = (review_backlog > 0).then(|| format!("needs review ({review_backlog})"));
    let mut signals = Vec::new();
    if let Some(existing) = signal {
        signals.push(existing);
    }
    if let Some(branch_mismatch) = branch_mismatch_signal {
        signals.push(branch_mismatch);
    }
    if let Some(stall) = stall_signal {
        signals.push(stall);
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

fn supervisory_role_label(role_type: RoleType) -> Option<&'static str> {
    match role_type {
        RoleType::Architect => Some("architect"),
        RoleType::Manager => Some("manager"),
        _ => None,
    }
}

fn supervisory_role_label_from_reason(reason: &str) -> Option<&'static str> {
    if reason.contains("architect") {
        Some("architect")
    } else if reason.contains("manager") {
        Some("manager")
    } else {
        None
    }
}

fn is_supervisory_stall_reason(reason: &str) -> bool {
    reason.starts_with("supervisory_") || reason.contains("architect") || reason.contains("manager")
}

fn supervisory_status_token(reason: &str, role_label: Option<&str>) -> String {
    let role_token = match role_label {
        Some("architect") => "stall:architect",
        Some("manager") => "stall:manager",
        _ => "stall",
    };
    let detail_token = if reason.ends_with("inbox_batching") {
        "inbox"
    } else if reason.ends_with("review_waiting") {
        "review"
    } else if reason.ends_with("shim_activity_only") {
        "shim"
    } else if reason.ends_with("status_only_output") {
        "status"
    } else if reason.ends_with("no_actionable_progress") {
        "no-progress"
    } else {
        "working-timeout"
    };
    format!("{role_token}:{detail_token}")
}

fn supervisory_reason_label(reason: &str) -> &'static str {
    if reason.ends_with("inbox_batching") {
        "inbox batching"
    } else if reason.ends_with("review_waiting") {
        "review waiting"
    } else if reason.ends_with("shim_activity_only") {
        "shim activity only"
    } else if reason.ends_with("status_only_output") {
        "status-only output"
    } else if reason.ends_with("stalled") {
        "working timeout"
    } else {
        "no actionable progress"
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
        let inferred = infer_runtime_task_metadata(project_root, &task);
        let branch_mismatch = task_branch_mismatch(&task, &inferred);
        let test_summary = crate::team::board::read_workflow_metadata(&task.source_path)
            .ok()
            .and_then(|metadata| metadata.test_results)
            .filter(|results| results.failed > 0)
            .map(|results| results.failure_summary());
        let entry = StatusTaskEntry {
            id: task.id,
            title: task.title,
            status: task.status.clone(),
            priority: task.priority,
            claimed_by: task.claimed_by,
            review_owner: task.review_owner,
            blocked_on: task.blocked_on,
            branch: task.branch.or_else(|| inferred.branch.clone()),
            worktree_path: task
                .worktree_path
                .or_else(|| inferred.worktree_path.clone()),
            commit: task.commit.or_else(|| inferred.commit.clone()),
            branch_mismatch,
            next_action: task.next_action,
            test_summary,
        };

        match task.status.as_str() {
            "in-progress" | "in_progress" => active_tasks.push(entry),
            "review" => review_queue.push(entry),
            _ => {}
        }
    }

    Ok((active_tasks, review_queue))
}

#[derive(Default)]
struct InferredTaskMetadata {
    branch: Option<String>,
    worktree_path: Option<String>,
    commit: Option<String>,
}

fn infer_runtime_task_metadata(project_root: &Path, task: &task::Task) -> InferredTaskMetadata {
    let Some(claimed_by) = task.claimed_by.as_deref() else {
        return InferredTaskMetadata::default();
    };
    if !claimed_by.starts_with("eng-") {
        return InferredTaskMetadata::default();
    }

    let worktree_path = project_root
        .join(".batty")
        .join("worktrees")
        .join(claimed_by);
    if !worktree_path.is_dir() {
        return InferredTaskMetadata::default();
    }

    InferredTaskMetadata {
        branch: git_stdout_raw(&worktree_path, ["rev-parse", "--abbrev-ref", "HEAD"]),
        worktree_path: relative_to_project_root(project_root, &worktree_path),
        commit: git_stdout(&worktree_path, ["rev-parse", "--short", "HEAD"]),
    }
}

fn task_branch_mismatch(task: &task::Task, inferred: &InferredTaskMetadata) -> Option<String> {
    let member_name = task.claimed_by.as_deref()?;
    if !member_name.starts_with("eng-")
        || classify_owned_task_status(task.status.as_str()) != Some(true)
    {
        return None;
    }

    let current_branch = inferred.branch.as_deref()?;
    let expected_branch = managed_task_branch(member_name, task);
    (current_branch != expected_branch)
        .then(|| format_branch_mismatch_signal(task.id, current_branch, &expected_branch))
}

fn git_stdout_raw<const N: usize>(cwd: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> Option<String> {
    git_stdout_raw(cwd, args).filter(|value| value != "HEAD")
}

fn relative_to_project_root(project_root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(project_root)
        .ok()
        .map(|relative| relative.display().to_string())
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
        .filter(|row| row.health.has_operator_warning())
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
    pub(crate) watchdog: WatchdogStatus,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    pub(crate) optional_subsystems: Option<Vec<OptionalSubsystemStatus>>,
    pub(crate) engineer_profiles:
        Option<Vec<crate::team::telemetry_db::EngineerPerformanceProfileRow>>,
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
        watchdog,
        workflow_metrics,
        active_tasks,
        review_queue,
        optional_subsystems,
        engineer_profiles,
        members,
    } = input;
    let health = build_team_status_health(&members, session_running, paused);
    TeamStatusJsonReport {
        team,
        session,
        running: session_running,
        paused,
        watchdog,
        health,
        workflow_metrics,
        active_tasks,
        review_queue,
        optional_subsystems,
        engineer_profiles,
        members,
    }
}

pub(crate) fn format_engineer_profiles(
    profiles: &[crate::team::telemetry_db::EngineerPerformanceProfileRow],
) -> String {
    let mut lines = vec![
        "Engineer Profiles".to_string(),
        format!(
            "{:<16} {:>5} {:>10} {:>10} {:>10} {:>10}",
            "ROLE", "TASKS", "AVG_TIME", "LOC/HR", "FIRST_PASS", "CTX_FREQ"
        ),
    ];

    for profile in profiles {
        lines.push(format!(
            "{:<16} {:>5} {:>10} {:>10} {:>10} {:>10}",
            profile.role,
            profile.completed_tasks,
            format_age_compact(profile.avg_task_completion_secs),
            format_rate_1(profile.lines_per_hour),
            format_pct_0(profile.first_pass_test_rate),
            format_pct_0(profile.context_exhaustion_frequency),
        ));
    }

    lines.join("\n")
}

pub(crate) fn format_benched_engineers(state: &crate::team::bench::BenchState) -> Option<String> {
    if state.benched.is_empty() {
        return None;
    }

    let mut lines = vec![
        "Benched Engineers".to_string(),
        format!("{:<20} {:<28} {}", "ENGINEER", "SINCE", "REASON"),
    ];
    for (engineer, entry) in &state.benched {
        lines.push(format!(
            "{:<20} {:<28} {}",
            engineer,
            entry.timestamp,
            entry.reason.as_deref().unwrap_or("-"),
        ));
    }
    Some(lines.join("\n"))
}

fn format_age_compact(secs: Option<f64>) -> String {
    let Some(secs) = secs else {
        return "-".to_string();
    };
    let secs = secs.round() as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3_600)
    }
}

fn format_rate_1(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_pct_0(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.0}%", value * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

pub fn compute_metrics(board_dir: &Path, members: &[MemberInstance]) -> Result<WorkflowMetrics> {
    compute_metrics_with_aging(
        board_dir,
        members,
        crate::team::board::AgingThresholds::default(),
    )
}

/// Compute workflow metrics, preferring SQLite telemetry DB over JSONL events.
///
/// If a `db` connection is provided, review pipeline metrics come from SQLite.
/// Otherwise falls back to `events_path` (JSONL), or returns zero review metrics.
pub fn compute_metrics_with_telemetry(
    board_dir: &Path,
    members: &[MemberInstance],
    db: Option<&rusqlite::Connection>,
    events_path: Option<&Path>,
) -> Result<WorkflowMetrics> {
    compute_metrics_with_telemetry_and_aging(
        board_dir,
        members,
        crate::team::board::AgingThresholds::default(),
        db,
        events_path,
    )
}

fn compute_metrics_with_aging(
    board_dir: &Path,
    members: &[MemberInstance],
    thresholds: crate::team::board::AgingThresholds,
) -> Result<WorkflowMetrics> {
    compute_metrics_with_telemetry_and_aging(board_dir, members, thresholds, None, None)
}

fn compute_metrics_with_telemetry_and_aging(
    board_dir: &Path,
    members: &[MemberInstance],
    thresholds: crate::team::board::AgingThresholds,
    db: Option<&rusqlite::Connection>,
    events_path: Option<&Path>,
) -> Result<WorkflowMetrics> {
    let board_metrics = compute_board_metrics(board_dir, members, thresholds)?;

    let review = if let Some(conn) = db {
        compute_review_metrics_from_db(conn)
    } else {
        compute_review_metrics(events_path)
    };

    Ok(WorkflowMetrics {
        runnable_count: board_metrics.runnable_count,
        blocked_count: board_metrics.blocked_count,
        in_review_count: board_metrics.in_review_count,
        in_progress_count: board_metrics.in_progress_count,
        stale_in_progress_count: board_metrics.stale_in_progress_count,
        aged_todo_count: board_metrics.aged_todo_count,
        stale_review_count: board_metrics.stale_review_count,
        idle_with_runnable: board_metrics.idle_with_runnable,
        top_runnable_tasks: board_metrics.top_runnable_tasks,
        oldest_review_age_secs: board_metrics.oldest_review_age_secs,
        oldest_assignment_age_secs: board_metrics.oldest_assignment_age_secs,
        auto_merge_count: review.auto_merge_count,
        manual_merge_count: review.manual_merge_count,
        auto_merge_rate: review.auto_merge_rate,
        rework_count: review.rework_count,
        rework_rate: review.rework_rate,
        review_nudge_count: review.review_nudge_count,
        review_escalation_count: review.review_escalation_count,
        avg_review_latency_secs: review.avg_review_latency_secs,
    })
}

pub fn compute_metrics_with_events(
    board_dir: &Path,
    members: &[MemberInstance],
    events_path: Option<&Path>,
) -> Result<WorkflowMetrics> {
    compute_metrics_with_telemetry(board_dir, members, None, events_path)
}

/// Board-only metrics (no event/review data).
struct BoardMetrics {
    runnable_count: u32,
    blocked_count: u32,
    in_review_count: u32,
    in_progress_count: u32,
    stale_in_progress_count: u32,
    aged_todo_count: u32,
    stale_review_count: u32,
    idle_with_runnable: Vec<String>,
    top_runnable_tasks: Vec<String>,
    oldest_review_age_secs: Option<u64>,
    oldest_assignment_age_secs: Option<u64>,
}

fn compute_board_metrics(
    board_dir: &Path,
    members: &[MemberInstance],
    thresholds: crate::team::board::AgingThresholds,
) -> Result<BoardMetrics> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(BoardMetrics {
            runnable_count: 0,
            blocked_count: 0,
            in_review_count: 0,
            in_progress_count: 0,
            stale_in_progress_count: 0,
            aged_todo_count: 0,
            stale_review_count: 0,
            idle_with_runnable: Vec::new(),
            top_runnable_tasks: Vec::new(),
            oldest_review_age_secs: None,
            oldest_assignment_age_secs: None,
        });
    }

    let tasks = task::load_tasks_from_dir(&tasks_dir)?;
    if tasks.is_empty() {
        return Ok(BoardMetrics {
            runnable_count: 0,
            blocked_count: 0,
            in_review_count: 0,
            in_progress_count: 0,
            stale_in_progress_count: 0,
            aged_todo_count: 0,
            stale_review_count: 0,
            idle_with_runnable: Vec::new(),
            top_runnable_tasks: Vec::new(),
            oldest_review_age_secs: None,
            oldest_assignment_age_secs: None,
        });
    }

    let dispatchable_tasks = crate::team::resolver::dispatchable_tasks(board_dir)?;
    let dispatchable_task_ids: HashSet<u32> =
        dispatchable_tasks.iter().map(|task| task.id).collect();

    let now = SystemTime::now();
    let runnable_count = tasks
        .iter()
        .filter(|task| dispatchable_task_ids.contains(&task.id))
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
    let top_runnable_tasks = top_runnable_task_summaries(&dispatchable_tasks, 3);
    let aging = project_root_from_board_dir(board_dir)
        .and_then(|project_root| {
            crate::team::board::compute_task_aging(board_dir, project_root, thresholds).ok()
        })
        .unwrap_or_default();

    Ok(BoardMetrics {
        runnable_count,
        blocked_count,
        in_review_count,
        in_progress_count,
        stale_in_progress_count: aging.stale_in_progress.len() as u32,
        aged_todo_count: aging.aged_todo.len() as u32,
        stale_review_count: aging.stale_review.len() as u32,
        idle_with_runnable,
        top_runnable_tasks,
        oldest_review_age_secs,
        oldest_assignment_age_secs,
    })
}

#[derive(Default)]
struct ReviewMetrics {
    auto_merge_count: u32,
    manual_merge_count: u32,
    auto_merge_rate: Option<f64>,
    rework_count: u32,
    rework_rate: Option<f64>,
    review_nudge_count: u32,
    review_escalation_count: u32,
    avg_review_latency_secs: Option<f64>,
}

fn compute_review_metrics(events_path: Option<&Path>) -> ReviewMetrics {
    let events = events_path
        .and_then(|path| events::read_events(path).ok())
        .unwrap_or_default();

    let mut auto_merge_count: u32 = 0;
    let mut manual_merge_count: u32 = 0;
    let mut rework_count: u32 = 0;
    let mut review_nudge_count: u32 = 0;
    let mut review_escalation_count: u32 = 0;

    // Track review enter/exit times per task for latency computation.
    // "task_completed" with a task field entering review; merged events exiting.
    let mut review_enter_ts: HashMap<String, u64> = HashMap::new();
    let mut review_latencies: Vec<f64> = Vec::new();

    for event in &events {
        match event.event.as_str() {
            "task_auto_merged" => {
                auto_merge_count += 1;
                if let Some(task_id) = &event.task {
                    if let Some(enter_ts) = review_enter_ts.remove(task_id) {
                        review_latencies.push((event.ts - enter_ts) as f64);
                    }
                }
            }
            "task_manual_merged" => {
                manual_merge_count += 1;
                if let Some(task_id) = &event.task {
                    if let Some(enter_ts) = review_enter_ts.remove(task_id) {
                        review_latencies.push((event.ts - enter_ts) as f64);
                    }
                }
            }
            "task_reworked" => {
                rework_count += 1;
            }
            "review_nudge_sent" => {
                review_nudge_count += 1;
            }
            "review_escalated" => {
                review_escalation_count += 1;
            }
            "task_completed" => {
                if let Some(task_id) = &event.task {
                    review_enter_ts.insert(task_id.clone(), event.ts);
                }
            }
            _ => {}
        }
    }

    let total_merges = auto_merge_count + manual_merge_count;
    let auto_merge_rate = if total_merges > 0 {
        Some(auto_merge_count as f64 / total_merges as f64)
    } else {
        None
    };
    let total_reviewed = total_merges + rework_count;
    let rework_rate = if total_reviewed > 0 {
        Some(rework_count as f64 / total_reviewed as f64)
    } else {
        None
    };
    let avg_review_latency_secs = if review_latencies.is_empty() {
        None
    } else {
        Some(review_latencies.iter().sum::<f64>() / review_latencies.len() as f64)
    };

    ReviewMetrics {
        auto_merge_count,
        manual_merge_count,
        auto_merge_rate,
        rework_count,
        rework_rate,
        review_nudge_count,
        review_escalation_count,
        avg_review_latency_secs,
    }
}

/// Compute review pipeline metrics from the SQLite telemetry database.
fn compute_review_metrics_from_db(conn: &rusqlite::Connection) -> ReviewMetrics {
    let row = match crate::team::telemetry_db::query_review_metrics(conn) {
        Ok(row) => row,
        Err(error) => {
            warn!(error = %error, "failed to query review metrics from telemetry DB; returning zeros");
            return ReviewMetrics::default();
        }
    };

    let auto_merge_count = row.auto_merge_count as u32;
    let manual_merge_count = row.manual_merge_count as u32;
    let rework_count = row.rework_count as u32;
    let total_merges = auto_merge_count + manual_merge_count;
    let auto_merge_rate = if total_merges > 0 {
        Some(auto_merge_count as f64 / total_merges as f64)
    } else {
        None
    };
    let total_reviewed = total_merges + rework_count;
    let rework_rate = if total_reviewed > 0 {
        Some(rework_count as f64 / total_reviewed as f64)
    } else {
        None
    };

    ReviewMetrics {
        auto_merge_count,
        manual_merge_count,
        auto_merge_rate,
        rework_count,
        rework_rate,
        review_nudge_count: row.review_nudge_count as u32,
        review_escalation_count: row.review_escalation_count as u32,
        avg_review_latency_secs: row.avg_review_latency_secs,
    }
}

pub fn format_metrics(metrics: &WorkflowMetrics) -> String {
    let idle = if metrics.idle_with_runnable.is_empty() {
        "-".to_string()
    } else {
        metrics.idle_with_runnable.join(", ")
    };
    let top_runnable = if metrics.top_runnable_tasks.is_empty() {
        "-".to_string()
    } else {
        metrics.top_runnable_tasks.join("; ")
    };

    let auto_merge_rate_str = metrics
        .auto_merge_rate
        .map(|r| format!("{:.0}%", r * 100.0))
        .unwrap_or_else(|| "-".to_string());
    let rework_rate_str = metrics
        .rework_rate
        .map(|r| format!("{:.0}%", r * 100.0))
        .unwrap_or_else(|| "-".to_string());
    let avg_latency_str = metrics
        .avg_review_latency_secs
        .map(|secs| format_age(Some(secs as u64)))
        .unwrap_or_else(|| "-".to_string());

    format!(
        "Workflow Metrics\n\
Runnable: {}\n\
Blocked: {}\n\
In Review: {}\n\
In Progress: {}\n\
Aging Alerts: stale in-progress {} | aged todo {} | stale review {}\n\
Idle With Runnable: {}\n\
Top Runnable: {}\n\
Oldest Review Age: {}\n\
Oldest Assignment Age: {}\n\n\
Review Pipeline\n\
Queue: {} | Avg Latency: {} | Auto-merge Rate: {} | Rework Rate: {}\n\
Auto: {} | Manual: {} | Rework: {} | Nudges: {} | Escalations: {}",
        metrics.runnable_count,
        metrics.blocked_count,
        metrics.in_review_count,
        metrics.in_progress_count,
        metrics.stale_in_progress_count,
        metrics.aged_todo_count,
        metrics.stale_review_count,
        idle,
        top_runnable,
        format_age(metrics.oldest_review_age_secs),
        format_age(metrics.oldest_assignment_age_secs),
        metrics.in_review_count,
        avg_latency_str,
        auto_merge_rate_str,
        rework_rate_str,
        metrics.auto_merge_count,
        metrics.manual_merge_count,
        metrics.rework_count,
        metrics.review_nudge_count,
        metrics.review_escalation_count,
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

fn task_priority_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn top_runnable_task_summaries(tasks: &[task::Task], limit: usize) -> Vec<String> {
    let mut runnable = tasks.iter().collect::<Vec<_>>();
    runnable.sort_by_key(|task| (task_priority_rank(&task.priority), task.id));
    runnable
        .into_iter()
        .take(limit)
        .map(|task| format!("#{} ({}) {}", task.id, task.priority, task.title))
        .collect()
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
    let events_path = team_events_path(project_root);

    // Try SQLite telemetry DB first, fall back to JSONL events.
    let db = crate::team::telemetry_db::open(project_root).ok();
    let events_fallback = if db.is_none() && events_path.is_file() {
        Some(events_path.as_path())
    } else {
        None
    };

    let thresholds = config::TeamConfig::load(&config_path)
        .map(|config| crate::team::board::AgingThresholds {
            stale_in_progress_hours: config.workflow_policy.stale_in_progress_hours,
            aged_todo_hours: config.workflow_policy.aged_todo_hours,
            stale_review_hours: config.workflow_policy.stale_review_hours,
        })
        .unwrap_or_default();

    match compute_metrics_with_telemetry_and_aging(
        &board_dir,
        members,
        thresholds,
        db.as_ref(),
        events_fallback,
    ) {
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
            "workflow_mode: hybrid"
                | "workflow_mode: workflow_first"
                | "workflow_mode: board_first"
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        }
    }

    fn architect(name: &str) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type: RoleType::Architect,
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

    fn user_member(name: &str) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type: RoleType::User,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
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
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "reviewing");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("nudge paused, needs review (2)")
        );
    }

    #[test]
    fn build_team_status_rows_surfaces_supervisory_stall_signal_and_role_token() {
        let members = vec![architect("architect"), manager("manager")];
        let runtime_statuses = HashMap::from([
            (
                "architect".to_string(),
                RuntimeMemberStatus {
                    state: "working".to_string(),
                    signal: None,
                    label: Some("working".to_string()),
                },
            ),
            (
                "manager".to_string(),
                RuntimeMemberStatus {
                    state: "working".to_string(),
                    signal: Some("nudge paused".to_string()),
                    label: Some("working".to_string()),
                },
            ),
        ]);
        let agent_health = HashMap::from([
            (
                "architect".to_string(),
                AgentHealthSummary {
                    stall_reason: Some(
                        "supervisory_stalled_architect_no_actionable_progress".to_string(),
                    ),
                    stall_summary: Some(
                        "architect (architect) stalled after 5m: no actionable progress"
                            .to_string(),
                    ),
                    ..AgentHealthSummary::default()
                },
            ),
            (
                "manager".to_string(),
                AgentHealthSummary {
                    stall_reason: Some(
                        "supervisory_stalled_manager_shim_activity_only".to_string(),
                    ),
                    stall_summary: Some(
                        "manager (manager) stalled after 5m: shim activity only".to_string(),
                    ),
                    ..AgentHealthSummary::default()
                },
            ),
        ]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &agent_health,
        );

        assert_eq!(rows[0].health_summary, "stall:architect:no-progress");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("architect (architect) stalled after 5m: no actionable progress")
        );
        assert_eq!(rows[1].health_summary, "stall:manager:shim");
        assert_eq!(
            rows[1].signal.as_deref(),
            Some("nudge paused, manager (manager) stalled after 5m: shim activity only")
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
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "starting");
        assert_eq!(rows[0].runtime_label, None);
    }

    #[test]
    fn build_team_status_rows_includes_branch_mismatch_signal() {
        let members = vec![engineer("eng-1")];
        let runtime_statuses = HashMap::from([(
            "eng-1".to_string(),
            RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: Some("nudge paused".to_string()),
                label: Some("idle".to_string()),
            },
        )]);
        let owned_task_buckets = HashMap::from([(
            "eng-1".to_string(),
            OwnedTaskBuckets {
                active: vec![41],
                review: Vec::new(),
            },
        )]);
        let branch_mismatches = HashMap::from([(
            "eng-1".to_string(),
            "branch mismatch (#41 detached HEAD; expected eng-1/41)".to_string(),
        )]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &owned_task_buckets,
            &branch_mismatches,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(
            rows[0].signal.as_deref(),
            Some("nudge paused, branch mismatch (#41 detached HEAD; expected eng-1/41)")
        );
    }

    #[test]
    fn branch_mismatch_by_member_flags_detached_head_claimed_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-branch-mismatch");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &base_branch,
            &team_config_dir,
        )
        .unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/41")
            .unwrap();
        let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "--detach", &head]);

        write_board_task(
            &repo,
            "041-active.md",
            "id: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\n",
        );

        let mut member = engineer("eng-1");
        member.use_worktrees = true;
        let mismatches = branch_mismatch_by_member(&repo, &[member]);

        assert_eq!(
            mismatches.get("eng-1").map(String::as_str),
            Some("branch mismatch (#41 detached HEAD; expected eng-1/41)")
        );
    }

    #[test]
    fn branch_mismatch_by_member_ignores_matching_claimed_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-branch-match");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &base_branch,
            &team_config_dir,
        )
        .unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/41")
            .unwrap();

        write_board_task(
            &repo,
            "041-active.md",
            "id: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\n",
        );

        let mut member = engineer("eng-1");
        member.use_worktrees = true;
        let mismatches = branch_mismatch_by_member(&repo, &[member]);

        assert!(mismatches.is_empty());
    }

    #[test]
    fn branch_mismatch_by_member_ignores_detached_head_without_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-detached-no-claim");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &base_branch,
            &team_config_dir,
        )
        .unwrap();
        let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "--detach", &head]);

        let mut member = engineer("eng-1");
        member.use_worktrees = true;
        let mismatches = branch_mismatch_by_member(&repo, &[member]);

        assert!(mismatches.is_empty());
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
                worktree_staleness: None,
                health: AgentHealthSummary::default(),
                health_summary: "-".to_string(),
                eta: "-".to_string(),
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
                worktree_staleness: None,
                health: AgentHealthSummary {
                    restart_count: 1,
                    context_exhaustion_count: 0,
                    delivery_failure_count: 0,
                    supervisory_digest_count: 0,
                    dispatch_fallback_count: 0,
                    dispatch_fallback_reason: None,
                    task_elapsed_secs: None,
                    backend_health: crate::agent::BackendHealth::default(),
                    stall_summary: None,
                    stall_reason: None,
                },
                health_summary: "r1".to_string(),
                eta: "-".to_string(),
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
                worktree_staleness: None,
                health: AgentHealthSummary {
                    restart_count: 0,
                    context_exhaustion_count: 1,
                    delivery_failure_count: 1,
                    supervisory_digest_count: 0,
                    dispatch_fallback_count: 0,
                    dispatch_fallback_reason: None,
                    task_elapsed_secs: None,
                    backend_health: crate::agent::BackendHealth::default(),
                    stall_summary: None,
                    stall_reason: None,
                },
                health_summary: "c1 d1".to_string(),
                eta: "-".to_string(),
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
    fn workflow_metrics_section_returns_formatted_metrics_for_board_first() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        fs::create_dir_all(&team_config_dir).unwrap();
        fs::write(
            team_config_dir.join("team.yaml"),
            "team: test\nworkflow_mode: board_first\n",
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
            watchdog: WatchdogStatus {
                state: "running".to_string(),
                restart_count: 2,
                current_backoff_secs: None,
                last_exit_reason: Some("daemon exited with status 101".to_string()),
            },
            workflow_metrics: Some(WorkflowMetrics {
                runnable_count: 1,
                ..WorkflowMetrics::default()
            }),
            active_tasks: Vec::new(),
            review_queue: Vec::new(),
            optional_subsystems: None,
            engineer_profiles: Some(vec![
                crate::team::telemetry_db::EngineerPerformanceProfileRow {
                    role: "eng-1".to_string(),
                    completed_tasks: 2,
                    avg_task_completion_secs: Some(1800.0),
                    lines_per_hour: Some(120.0),
                    first_pass_test_rate: Some(0.5),
                    context_exhaustion_frequency: Some(0.0),
                },
            ]),
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
                worktree_staleness: None,
                health: AgentHealthSummary::default(),
                health_summary: "-".to_string(),
                eta: "-".to_string(),
            }],
        });

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["team"], "test");
        assert_eq!(json["running"], true);
        assert_eq!(json["watchdog"]["restart_count"], 2);
        assert_eq!(json["health"]["member_count"], 1);
        assert_eq!(json["workflow_metrics"]["runnable_count"], 1);
        assert!(json["members"].is_array());
        assert_eq!(json["engineer_profiles"][0]["role"], "eng-1");
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
            merge_status_signal(
                Some("nudged".to_string()),
                None,
                Some("manager (manager) stalled after 5m: no actionable progress".to_string()),
                2,
                1,
            ),
            Some(
                "nudged, manager (manager) stalled after 5m: no actionable progress, needs triage (2), needs review (1)"
                    .to_string()
            )
        );
    }

    #[test]
    fn merge_status_signal_returns_none_when_no_signals_exist() {
        assert_eq!(merge_status_signal(None, None, None, 0, 0), None);
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
        assert!(review_queue[0].test_summary.is_none());
    }

    #[test]
    fn board_status_task_queues_infers_worktree_metadata_when_frontmatter_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-infer");
        let tasks_dir = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("041-active.md"),
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        )
        .unwrap();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();
        fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "note.txt"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "note"]);

        let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(active_tasks.len(), 1);
        assert_eq!(
            active_tasks[0].worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1")
        );
        assert!(
            active_tasks[0]
                .branch
                .as_deref()
                .is_some_and(|branch| branch.contains("eng-1"))
        );
        assert!(active_tasks[0].commit.as_deref().is_some());
        assert_eq!(
            active_tasks[0].branch_mismatch.as_deref(),
            Some("branch mismatch (#41 on eng-1; expected eng-1/41)")
        );
        assert!(active_tasks[0].test_summary.is_none());
    }

    #[test]
    fn board_status_task_queues_surfaces_detached_head_branch_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-detached-board");
        let tasks_dir = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("041-active.md"),
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        )
        .unwrap();

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &base_branch,
            &team_config_dir,
        )
        .unwrap();
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/41")
            .unwrap();
        let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "--detach", &head]);

        let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(active_tasks[0].branch.as_deref(), Some("HEAD"));
        assert_eq!(
            active_tasks[0].branch_mismatch.as_deref(),
            Some("branch mismatch (#41 detached HEAD; expected eng-1/41)")
        );
    }

    #[test]
    fn board_status_task_queues_surfaces_failed_test_summary() {
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
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\ntest_results:\n  framework: cargo\n  total: 3\n  passed: 2\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n      message: assertion failed\n      location: src/parser.rs:12:5\n---\n",
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(active_tasks.len(), 1);
        assert_eq!(
            active_tasks[0].test_summary.as_deref(),
            Some("1 tests failed: parser::it_works (assertion failed at src/parser.rs:12:5)")
        );
    }

    #[test]
    fn build_team_status_json_report_includes_health_and_queues() {
        let report = build_team_status_json_report(TeamStatusJsonReportInput {
            team: "test".to_string(),
            session: "batty-test".to_string(),
            session_running: true,
            paused: true,
            watchdog: WatchdogStatus {
                state: "restarting".to_string(),
                restart_count: 1,
                current_backoff_secs: Some(4),
                last_exit_reason: Some("daemon exited with status 101".to_string()),
            },
            workflow_metrics: Some(WorkflowMetrics {
                runnable_count: 2,
                blocked_count: 1,
                in_review_count: 1,
                in_progress_count: 3,
                idle_with_runnable: vec!["eng-2".to_string()],
                top_runnable_tasks: vec!["#7 (high) Unstick manager inbox".to_string()],
                oldest_review_age_secs: Some(60),
                oldest_assignment_age_secs: Some(120),
                ..Default::default()
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
                branch_mismatch: None,
                next_action: None,
                test_summary: Some("1 tests failed: parser::it_works".to_string()),
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
                branch_mismatch: None,
                next_action: Some("review now".to_string()),
                test_summary: None,
            }],
            optional_subsystems: None,
            engineer_profiles: None,
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
                    worktree_staleness: Some(3),
                    health: AgentHealthSummary {
                        restart_count: 1,
                        context_exhaustion_count: 0,
                        delivery_failure_count: 0,
                        supervisory_digest_count: 0,
                        dispatch_fallback_count: 0,
                        dispatch_fallback_reason: None,
                        task_elapsed_secs: Some(30),
                        stall_reason: None,
                        stall_summary: None,
                        backend_health: crate::agent::BackendHealth::default(),
                    },
                    health_summary: "r1 t30s".to_string(),
                    eta: "-".to_string(),
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
                    worktree_staleness: Some(0),
                    health: AgentHealthSummary::default(),
                    health_summary: "-".to_string(),
                    eta: "-".to_string(),
                },
            ],
        });

        assert_eq!(report.team, "test");
        assert_eq!(report.watchdog.restart_count, 1);
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
    fn format_engineer_profiles_renders_compact_table() {
        let rendered =
            format_engineer_profiles(&[crate::team::telemetry_db::EngineerPerformanceProfileRow {
                role: "eng-1".to_string(),
                completed_tasks: 3,
                avg_task_completion_secs: Some(5400.0),
                lines_per_hour: Some(42.5),
                first_pass_test_rate: Some(2.0 / 3.0),
                context_exhaustion_frequency: Some(1.0 / 3.0),
            }]);

        assert!(rendered.contains("Engineer Profiles"));
        assert!(rendered.contains("eng-1"));
        assert!(rendered.contains("1h"));
        assert!(rendered.contains("42.5"));
        assert!(rendered.contains("67%"));
        assert!(rendered.contains("33%"));
    }

    #[test]
    fn format_benched_engineers_includes_reason_and_timestamp() {
        let rendered = format_benched_engineers(&crate::team::bench::BenchState {
            benched: std::collections::BTreeMap::from([(
                "eng-1".to_string(),
                crate::team::bench::BenchEntry {
                    timestamp: "2026-04-10T12:00:00Z".to_string(),
                    reason: Some("session end".to_string()),
                },
            )]),
        })
        .unwrap();

        assert!(rendered.contains("Benched Engineers"));
        assert!(rendered.contains("eng-1"));
        assert!(rendered.contains("2026-04-10T12:00:00Z"));
        assert!(rendered.contains("session end"));
    }

    #[test]
    fn load_watchdog_status_marks_circuit_breaker_and_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        std::fs::write(
            watchdog_state_path(tmp.path()),
            serde_json::json!({
                "restart_count": 5,
                "circuit_breaker_tripped": true,
                "last_exit_reason": "daemon exited with status 101"
            })
            .to_string(),
        )
        .unwrap();

        let watchdog = load_watchdog_status(tmp.path(), true);

        assert_eq!(watchdog.state, "circuit-open");
        assert_eq!(watchdog.restart_count, 5);
        assert_eq!(
            watchdog.last_exit_reason.as_deref(),
            Some("daemon exited with status 101")
        );
    }

    #[test]
    fn format_watchdog_summary_includes_backoff_and_reason() {
        let summary = format_watchdog_summary(&WatchdogStatus {
            state: "restarting".to_string(),
            restart_count: 2,
            current_backoff_secs: Some(4),
            last_exit_reason: Some("daemon exited with status 101".to_string()),
        });

        assert!(summary.contains("restarting"));
        assert!(summary.contains("r2"));
        assert!(summary.contains("backoff=4s"));
        assert!(summary.contains("daemon exited with status 101"));
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
            supervisory_digest_count: 1,
            dispatch_fallback_count: 0,
            dispatch_fallback_reason: None,
            task_elapsed_secs: Some(750),
            stall_reason: None,
            stall_summary: None,
            backend_health: crate::agent::BackendHealth::default(),
        });

        assert_eq!(summary, "r2 c1 d3 sd1 t12m");
        assert_eq!(
            format_agent_health_summary(&AgentHealthSummary::default()),
            "-"
        );
    }

    #[test]
    fn format_agent_health_summary_includes_supervisory_stall_token() {
        let summary = format_agent_health_summary(&AgentHealthSummary {
            stall_reason: Some("supervisory_stalled_manager_no_actionable_progress".to_string()),
            stall_summary: Some(
                "lead (manager) stalled after 5m: no actionable progress".to_string(),
            ),
            ..AgentHealthSummary::default()
        });

        assert_eq!(summary, "stall:manager:no-progress");
    }

    #[test]
    fn format_agent_health_summary_includes_dispatch_fallback_token() {
        let summary = format_agent_health_summary(&AgentHealthSummary {
            dispatch_fallback_count: 1,
            dispatch_fallback_reason: Some(
                "manager_supervisory_no_actionable_progress".to_string(),
            ),
            ..AgentHealthSummary::default()
        });

        assert_eq!(summary, "fd1:manager_supervisory_no_actionable_progress");
    }

    #[test]
    fn format_agent_health_summary_for_role_includes_supervisory_role_token() {
        let summary = format_agent_health_summary_for_role(
            &AgentHealthSummary {
                stall_reason: Some("supervisory_stalled_manager_shim_activity_only".to_string()),
                stall_summary: Some(
                    "lead (manager) stalled after 5m: shim activity only".to_string(),
                ),
                ..AgentHealthSummary::default()
            },
            Some(RoleType::Manager),
        );

        assert_eq!(summary, "stall:manager:shim");
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

        let mut digest_emitted = TeamEvent::supervisory_digest_emitted("eng-1", 3, 1);
        digest_emitted.ts = now_unix().saturating_sub(565);
        sink.emit(digest_emitted).unwrap();

        let mut supervisory_stall = TeamEvent::stall_detected_with_reason(
            "eng-1",
            None,
            300,
            Some("supervisory_stalled_manager_no_actionable_progress"),
        );
        supervisory_stall.ts = now_unix().saturating_sub(560);
        supervisory_stall.task = Some("supervisory::eng-1".to_string());
        supervisory_stall.details =
            Some("eng-1 (manager) stalled after 5m: no actionable progress".to_string());
        sink.emit(supervisory_stall).unwrap();

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
        assert_eq!(eng_1.supervisory_digest_count, 1);
        assert_eq!(
            eng_1.stall_reason.as_deref(),
            Some("supervisory_stalled_manager_no_actionable_progress")
        );
        assert_eq!(
            eng_1.stall_summary.as_deref(),
            Some("eng-1 (manager) stalled after 5m: no actionable progress")
        );
        assert!(eng_1.task_elapsed_secs.unwrap() >= 600);
        assert_eq!(health.get("eng-2").unwrap(), &AgentHealthSummary::default());
    }

    #[test]
    fn format_agent_health_summary_includes_backend_health() {
        let summary = format_agent_health_summary(&AgentHealthSummary {
            backend_health: crate::agent::BackendHealth::Unreachable,
            ..AgentHealthSummary::default()
        });
        assert_eq!(summary, "B:unreachable");

        let summary = format_agent_health_summary(&AgentHealthSummary {
            backend_health: crate::agent::BackendHealth::Degraded,
            restart_count: 1,
            ..AgentHealthSummary::default()
        });
        assert_eq!(summary, "B:degraded r1");
    }

    #[test]
    fn load_optional_subsystem_statuses_reads_budget_state() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();
        for _ in 0..6 {
            let mut event = TeamEvent::loop_step_error("process_telegram_queue", "telegram down");
            event.ts = now_unix();
            sink.emit(event).unwrap();
        }

        let daemon_state = serde_json::json!({
            "optional_subsystem_backoff": {"telegram": 2},
            "optional_subsystem_disabled_remaining_secs": {"telegram": 45}
        });
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            daemon_state_path(tmp.path()),
            serde_json::to_vec_pretty(&daemon_state).unwrap(),
        )
        .unwrap();

        let statuses = load_optional_subsystem_statuses(tmp.path());
        let telegram = statuses
            .iter()
            .find(|status| status.name == "telegram")
            .expect("telegram status should exist");
        assert_eq!(telegram.state, "disabled");
        assert_eq!(telegram.recent_errors, 6);
        assert_eq!(telegram.disabled_remaining_secs, Some(45));
        assert_eq!(telegram.backoff_stage, Some(2));
        assert_eq!(telegram.last_error.as_deref(), Some("telegram down"));
    }

    #[test]
    fn agent_health_by_member_reads_health_changed_events() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();
        sink.emit(TeamEvent::health_changed("eng-1", "healthy→unreachable"))
            .unwrap();

        let health = agent_health_by_member(tmp.path(), &[engineer("eng-1")]);
        assert_eq!(
            health.get("eng-1").unwrap().backend_health,
            crate::agent::BackendHealth::Unreachable,
        );
    }

    #[test]
    fn agent_health_by_member_uses_latest_health_event() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();
        sink.emit(TeamEvent::health_changed("eng-1", "healthy→unreachable"))
            .unwrap();
        sink.emit(TeamEvent::health_changed("eng-1", "unreachable→healthy"))
            .unwrap();

        let health = agent_health_by_member(tmp.path(), &[engineer("eng-1")]);
        assert_eq!(
            health.get("eng-1").unwrap().backend_health,
            crate::agent::BackendHealth::Healthy,
        );
    }

    #[test]
    fn build_team_status_health_counts_unhealthy_backend() {
        let rows = vec![TeamStatusRow {
            name: "eng-bad".to_string(),
            role: "engineer".to_string(),
            role_type: "Engineer".to_string(),
            agent: Some("claude".to_string()),
            reports_to: Some("manager".to_string()),
            state: "working".to_string(),
            pending_inbox: 0,
            triage_backlog: 0,
            active_owned_tasks: Vec::new(),
            review_owned_tasks: Vec::new(),
            signal: None,
            runtime_label: Some("working".to_string()),
            worktree_staleness: None,
            health: AgentHealthSummary {
                backend_health: crate::agent::BackendHealth::Unreachable,
                ..AgentHealthSummary::default()
            },
            health_summary: "B:unreachable".to_string(),
            eta: "-".to_string(),
        }];
        let health = build_team_status_health(&rows, true, false);
        assert_eq!(health.unhealthy_members, vec!["eng-bad".to_string()]);
    }

    #[test]
    fn build_team_status_health_counts_supervisory_stall_warning() {
        let rows = vec![TeamStatusRow {
            name: "eng-stalled".to_string(),
            role: "engineer".to_string(),
            role_type: "Engineer".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("manager".to_string()),
            state: "working".to_string(),
            pending_inbox: 0,
            triage_backlog: 0,
            active_owned_tasks: Vec::new(),
            review_owned_tasks: Vec::new(),
            signal: None,
            runtime_label: Some("working".to_string()),
            worktree_staleness: None,
            health: AgentHealthSummary {
                stall_reason: Some(
                    "supervisory_stalled_manager_no_actionable_progress".to_string(),
                ),
                stall_summary: Some(
                    "eng-stalled (manager) stalled after 5m: no actionable progress".to_string(),
                ),
                ..AgentHealthSummary::default()
            },
            health_summary: "stall:manager:no-progress".to_string(),
            eta: "-".to_string(),
        }];

        let health = build_team_status_health(&rows, true, false);
        assert_eq!(health.unhealthy_members, vec!["eng-stalled".to_string()]);
    }

    // --- SQLite telemetry migration tests ---

    #[test]
    fn compute_metrics_with_telemetry_db_returns_review_metrics() {
        let tmp = tempfile::tempdir().unwrap();

        // Set up board with a task.
        write_board_task(
            tmp.path(),
            "001-task.md",
            "id: 1\ntitle: Test\nstatus: todo\npriority: high\n",
        );

        // Set up in-memory telemetry DB with events.
        let conn = crate::team::telemetry_db::open_in_memory().unwrap();
        let events = vec![
            crate::team::events::TeamEvent::task_completed("eng-1", Some("1")),
            crate::team::events::TeamEvent::task_auto_merged("eng-1", "1", 0.9, 3, 50),
            crate::team::events::TeamEvent::task_completed("eng-1", Some("2")),
            crate::team::events::TeamEvent::task_manual_merged("2"),
            crate::team::events::TeamEvent::task_reworked("eng-1", "3"),
        ];
        for event in &events {
            crate::team::telemetry_db::insert_event(&conn, event).unwrap();
        }

        let metrics =
            compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], Some(&conn), None).unwrap();

        assert_eq!(metrics.auto_merge_count, 1);
        assert_eq!(metrics.manual_merge_count, 1);
        assert_eq!(metrics.rework_count, 1);
        assert!(metrics.auto_merge_rate.is_some());
        // Board metric still works.
        assert_eq!(metrics.runnable_count, 1);
    }

    #[test]
    fn compute_metrics_without_db_falls_back_to_jsonl() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task(
            tmp.path(),
            "001-task.md",
            "id: 1\ntitle: Test\nstatus: todo\npriority: high\n",
        );

        // Write events to JSONL (no DB).
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();
        sink.emit(TeamEvent::task_auto_merged("eng-1", "1", 0.9, 3, 50))
            .unwrap();

        let metrics =
            compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], None, Some(&events_path))
                .unwrap();

        assert_eq!(metrics.auto_merge_count, 1);
        assert_eq!(metrics.runnable_count, 1);
    }

    #[test]
    fn compute_metrics_without_db_or_events_returns_zero_review_metrics() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task(
            tmp.path(),
            "001-task.md",
            "id: 1\ntitle: Test\nstatus: todo\npriority: high\n",
        );

        // No DB, no events path.
        let metrics =
            compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], None, None).unwrap();

        assert_eq!(metrics.auto_merge_count, 0);
        assert_eq!(metrics.manual_merge_count, 0);
        assert_eq!(metrics.rework_count, 0);
        assert_eq!(metrics.auto_merge_rate, None);
        // Board metric still works.
        assert_eq!(metrics.runnable_count, 1);
    }

    #[test]
    fn compute_metrics_excludes_manual_blocked_todo_from_runnable_count() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task(
            tmp.path(),
            "001-dispatchable-a.md",
            "id: 1\ntitle: Dispatchable A\nstatus: todo\npriority: high\n",
        );
        write_board_task(
            tmp.path(),
            "002-dispatchable-b.md",
            "id: 2\ntitle: Dispatchable B\nstatus: todo\npriority: high\n",
        );
        write_board_task(
            tmp.path(),
            "003-manual-blocked.md",
            "id: 3\ntitle: Manual Blocked\nstatus: todo\npriority: high\nblocked: manual provider-console token rotation\n",
        );

        let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();
        assert_eq!(metrics.runnable_count, 2);
        assert_eq!(metrics.blocked_count, 1);
    }

    #[test]
    fn format_metrics_unchanged_with_db_source() {
        let conn = crate::team::telemetry_db::open_in_memory().unwrap();
        let events = vec![
            crate::team::events::TeamEvent::task_completed("eng-1", Some("1")),
            crate::team::events::TeamEvent::task_auto_merged("eng-1", "1", 0.9, 3, 50),
        ];
        for event in &events {
            crate::team::telemetry_db::insert_event(&conn, event).unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "001-task.md",
            "id: 1\ntitle: Test\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
        );

        let metrics =
            compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], Some(&conn), None).unwrap();

        let formatted = format_metrics(&metrics);
        assert!(formatted.contains("Workflow Metrics"));
        assert!(formatted.contains("Auto-merge Rate: 100%"));
        assert!(formatted.contains("In Review: 1"));
    }
}
