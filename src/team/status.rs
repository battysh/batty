use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::task;

use super::config::{self, RoleType};
use super::daemon::{MainSmokeState, NudgeSchedule};
use super::daemon_mgmt::{
    PersistedWatchdogState, daemon_child_pid_path, daemon_log_path, process_exists,
    watchdog_pid_path, watchdog_state_path,
};
use super::events;
use super::hierarchy::MemberInstance;
use super::inbox;
use super::review::ReviewQueueState;
use super::standup::MemberState;
use super::supervisory_notice::{
    SupervisoryMemberActivity, SupervisoryPressure, SupervisoryPressureSnapshot,
    classify_supervisory_pressure_normalized, normalized_body, supervisory_pressure_snapshots,
};
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
    pub(crate) stale_review: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AgentHealthSummary {
    pub(crate) restart_count: u32,
    pub(crate) context_exhaustion_count: u32,
    pub(crate) proactive_handoff_count: u32,
    pub(crate) delivery_failure_count: u32,
    pub(crate) supervisory_digest_count: u32,
    pub(crate) dispatch_fallback_count: u32,
    pub(crate) dispatch_fallback_reason: Option<String>,
    pub(crate) stale_active_cleared_count: u32,
    pub(crate) stale_active_summary: Option<String>,
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

    pub(crate) fn record_stale_active_clear(&mut self, task: Option<&str>, reason: Option<&str>) {
        let Some(summary) = stale_active_clear_summary(task, reason) else {
            return;
        };
        self.stale_active_cleared_count += 1;
        self.stale_active_summary = Some(summary);
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
            || self.proactive_handoff_count > 0
            || self.delivery_failure_count > 0
            || self.stale_active_cleared_count > 0
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
    states: HashMap<String, MemberState>,
    #[serde(default)]
    active_tasks: HashMap<String, u32>,
    #[serde(default)]
    retry_counts: HashMap<String, u32>,
    #[serde(default)]
    main_smoke_state: Option<MainSmokeState>,
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
    pub board_state: WorkflowBoardState,
    pub runnable_count: u32,
    pub implementation_runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub actionable_review_count: u32,
    pub in_progress_count: u32,
    pub stale_in_progress_count: u32,
    pub aged_todo_count: u32,
    pub stale_review_count: u32,
    pub idle_with_runnable: Vec<String>,
    pub top_runnable_tasks: Vec<String>,
    pub blocked_dispatch_reasons: Vec<String>,
    pub oldest_review_age_secs: Option<u64>,
    pub oldest_assignment_age_secs: Option<u64>,
    // Review pipeline metrics (computed from event log)
    pub auto_merge_count: u32,
    pub manual_merge_count: u32,
    pub direct_root_merge_count: u32,
    pub isolated_integration_merge_count: u32,
    pub direct_root_failure_count: u32,
    pub isolated_integration_failure_count: u32,
    pub auto_merge_rate: Option<f64>,
    pub rework_count: u32,
    pub rework_rate: Option<f64>,
    pub review_nudge_count: u32,
    pub review_escalation_count: u32,
    pub avg_review_latency_secs: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowBoardState {
    #[default]
    EmptyBoard,
    BlockedOnlyBoard,
    ReviewBacklogGated,
    RunnableBoard,
    ActiveBoard,
}

impl WorkflowBoardState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyBoard => "empty-board",
            Self::BlockedOnlyBoard => "blocked-only-board",
            Self::ReviewBacklogGated => "review-backlog-gated",
            Self::RunnableBoard => "runnable-board",
            Self::ActiveBoard => "active-board",
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed_test_state: Option<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct WatchdogStatus {
    pub(crate) state: String,
    pub(crate) restart_count: u32,
    pub(crate) current_backoff_secs: Option<u64>,
    pub(crate) last_exit_category: Option<String>,
    pub(crate) last_exit_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watchdog_pid: Option<u32>,
    pub(crate) watchdog_pid_live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_pid: Option<u32>,
    pub(crate) daemon_pid_live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watchdog_state_updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watchdog_state_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_state_updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_state_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_log_updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_log_age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) orphan_codex_execs: Vec<OrphanProcessStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OrphanProcessStatus {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct TeamStatusJsonReport {
    pub(crate) team: String,
    pub(crate) session: String,
    pub(crate) running: bool,
    pub(crate) paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) main_smoke: Option<MainSmokeState>,
    pub(crate) watchdog: WatchdogStatus,
    pub(crate) health: TeamStatusHealth,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) publish_handoff: Option<crate::release::ReleasePublishHandoff>,
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
    let output = crate::tmux::run_tmux_with_timeout(
        [
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_id}\t#{@batty_role}\t#{@batty_status}\t#{pane_dead}",
        ],
        "list-panes runtime status",
        Some(session),
    )
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
    supervisory_pressures: &HashMap<String, SupervisoryPressureSnapshot>,
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
            let stale_review_backlog = owned_tasks.stale_review.len();
            let supervisory_pressure = supervisory_pressures
                .get(&member.name)
                .cloned()
                .unwrap_or_default();
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
                health.stale_active_summary.clone(),
                health.stall_summary.clone(),
                &supervisory_pressure,
                stale_review_backlog,
            );
            let health_summary =
                format_agent_health_summary_for_role(&health, Some(member.role_type));
            let pending_inbox =
                if matches!(member.role_type, RoleType::Architect | RoleType::Manager)
                    && supervisory_pressure.actionable_count() > 0
                {
                    supervisory_pressure.actionable_count()
                } else {
                    pending_inbox
                };

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

fn format_blocked_branch_recovery_signal(
    task_id: u32,
    current_branch: &str,
    expected_branch: &str,
    detail: &str,
) -> String {
    if current_branch == "HEAD" {
        format!(
            "branch recovery blocked (#{} detached HEAD; expected {}; {})",
            task_id, expected_branch, detail
        )
    } else {
        format!(
            "branch recovery blocked (#{} on {}; expected {}; {})",
            task_id, current_branch, expected_branch, detail
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

fn task_branch_signal_for_task(
    project_root: &Path,
    task: &task::Task,
    current_branch: &str,
) -> Option<String> {
    let member_name = task.claimed_by.as_deref()?;
    if !member_name.starts_with("eng-")
        || classify_owned_task_status(task.status.as_str()) != Some(true)
    {
        return None;
    }

    let expected_branch = managed_task_branch(member_name, task);
    if current_branch == expected_branch {
        return None;
    }

    if current_branch == "HEAD" {
        return Some(format_blocked_branch_recovery_signal(
            task.id,
            current_branch,
            &expected_branch,
            "manual checkout required",
        ));
    }

    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(member_name);
    if crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap_or(false) {
        return Some(format_blocked_branch_recovery_signal(
            task.id,
            current_branch,
            &expected_branch,
            "dirty worktree",
        ));
    }

    Some(format_branch_mismatch_signal(
        task.id,
        current_branch,
        &expected_branch,
    ))
}

fn preserved_completed_lane_signal(project_root: &Path, member_name: &str) -> Option<String> {
    let record = crate::team::checkpoint::read_preserved_lane_record(project_root, member_name)?;
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(member_name);
    if !worktree_dir.is_dir() {
        return None;
    }

    let current_branch = git_stdout_raw(&worktree_dir, ["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current_branch != crate::team::task_loop::engineer_base_branch_name(member_name) {
        return None;
    }
    if crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap_or(false) {
        return None;
    }

    Some(record.status_signal())
}

pub(crate) fn claimed_task_branch_signal(
    project_root: &Path,
    member_name: &str,
    claimed_tasks: &[&task::Task],
) -> Option<String> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(member_name);
    if !worktree_dir.is_dir() {
        return None;
    }

    let current_branch = git_stdout_raw(&worktree_dir, ["rev-parse", "--abbrev-ref", "HEAD"])?;
    let task = select_authoritative_claimed_task(member_name, &current_branch, claimed_tasks)?;
    task_branch_signal_for_task(project_root, task, &current_branch)
}

fn load_board_tasks_for_status(board_dir: &Path, context: &str) -> Result<Vec<task::Task>> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(Vec::new());
    }

    let repairs = crate::team::task_cmd::repair_board_frontmatter_compat(board_dir)?;
    if !repairs.is_empty() {
        let repaired_tasks = repairs
            .iter()
            .map(|repair| {
                let task_label = repair
                    .task_id
                    .map(|task_id| format!("#{task_id}"))
                    .unwrap_or_else(|| repair.path.display().to_string());
                match repair.reason.as_deref() {
                    Some(reason) => format!("{task_label} ({reason})"),
                    None => task_label,
                }
            })
            .collect::<Vec<_>>();
        warn!(
            context = context,
            repaired_count = repairs.len(),
            repaired_tasks = ?repaired_tasks,
            "repaired malformed board task frontmatter during status scan"
        );
    }

    task::load_tasks_from_dir(&tasks_dir)
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
    let tasks = if tasks_dir.is_dir() {
        match load_board_tasks_for_status(
            tasks_dir.parent().unwrap_or(&tasks_dir),
            "branch_mismatch_by_member",
        ) {
            Ok(tasks) => tasks,
            Err(error) => {
                warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for branch mismatch status");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer && member.use_worktrees)
        .filter_map(|member| {
            let claimed_tasks = tasks
                .iter()
                .filter(|task| task_has_active_claim(task, &member.name))
                .collect::<Vec<_>>();
            claimed_task_branch_signal(project_root, &member.name, &claimed_tasks)
                .or_else(|| preserved_completed_lane_signal(project_root, &member.name))
                .map(|signal| (member.name.clone(), signal))
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

            let mut max_count: Option<u32> = None;
            for repo in status_worktree_repo_targets(&worktree_dir) {
                match super::task_loop::worktree_commits_behind_main(&repo.path) {
                    Ok(count) => {
                        max_count = Some(match max_count {
                            Some(current) => current.max(count),
                            None => count,
                        });
                    }
                    Err(error) => {
                        warn!(
                            member = %member.name,
                            repo = repo.label.as_deref().unwrap_or("<root>"),
                            worktree = %repo.path.display(),
                            error = %error,
                            "failed to measure engineer worktree staleness for status"
                        );
                    }
                }
            }

            max_count.map(|count| (member.name.clone(), count))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusWorktreeRepoTarget {
    label: Option<String>,
    path: PathBuf,
}

fn status_worktree_repo_targets(worktree_dir: &Path) -> Vec<StatusWorktreeRepoTarget> {
    if super::git_cmd::is_git_repo(worktree_dir) {
        return vec![StatusWorktreeRepoTarget {
            label: None,
            path: worktree_dir.to_path_buf(),
        }];
    }

    super::git_cmd::discover_sub_repos(worktree_dir)
        .into_iter()
        .map(|path| StatusWorktreeRepoTarget {
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            path,
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
    // Track the latest daemon_started timestamp so stall events from prior
    // daemon sessions don't leak into the current status display. Without
    // this, a stall_detected event from 2 hours ago (before the last
    // restart) appears as a live "stalled after 2h" signal on a freshly
    // restarted member.
    let mut latest_daemon_started_ts: u64 = 0;
    match events::read_events(&team_events_path(project_root)) {
        Ok(events) => {
            for event in events {
                // Session lifecycle events don't carry a role but are
                // still meaningful — process them before the role guard.
                if event.event == "daemon_started" {
                    if event.ts > latest_daemon_started_ts {
                        latest_daemon_started_ts = event.ts;
                        // Clear stall state for every member so stall events
                        // from prior sessions don't affect the current
                        // display.
                        for health in health_by_member.values_mut() {
                            health.record_supervisory_stall(None, None);
                        }
                    }
                    continue;
                }

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
                        if event.reason.as_deref() == Some("context_pressure") {
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .proactive_handoff_count += 1;
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
                    "state_reconciliation" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .record_stale_active_clear(
                                event.task.as_deref(),
                                event.reason.as_deref(),
                            );
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
    let watchdog_state = watchdog_state_path(project_root);
    let persisted = if !watchdog_state.exists() {
        PersistedWatchdogState::default()
    } else {
        match std::fs::read_to_string(&watchdog_state)
            .with_context(|| format!("failed to read {}", watchdog_state.display()))
            .and_then(|content| {
                serde_json::from_str::<PersistedWatchdogState>(&content)
                    .with_context(|| format!("failed to parse {}", watchdog_state.display()))
            }) {
            Ok(state) => state,
            Err(error) => {
                warn!(error = %error, "failed to load watchdog state");
                PersistedWatchdogState::default()
            }
        }
    };

    let watchdog_pid = read_status_pid(&watchdog_pid_path(project_root));
    let watchdog_pid_live = watchdog_pid.is_some_and(process_exists);
    let daemon_pid = persisted
        .child_pid
        .or_else(|| read_status_pid(&daemon_child_pid_path(project_root)));
    let daemon_pid_live = daemon_pid.is_some_and(process_exists);
    let now = SystemTime::now();
    let watchdog_state_updated_at = file_modified_unix_secs(&watchdog_state);
    let watchdog_state_age_secs = file_age_secs(&watchdog_state, now);
    let daemon_state = daemon_state_path(project_root);
    let daemon_state_updated_at = file_modified_unix_secs(&daemon_state);
    let daemon_state_age_secs = file_age_secs(&daemon_state, now);
    let daemon_log = daemon_log_path(project_root);
    let daemon_log_updated_at = file_modified_unix_secs(&daemon_log);
    let daemon_log_age_secs = file_age_secs(&daemon_log, now);

    let state = if !session_running {
        "stopped".to_string()
    } else if !watchdog_pid_live {
        "offline".to_string()
    } else if persisted.circuit_breaker_tripped {
        "circuit-open".to_string()
    } else if persisted.current_backoff_secs.is_some() {
        "restarting".to_string()
    } else if daemon_pid_live {
        "running".to_string()
    } else {
        "degraded".to_string()
    };
    let orphan_codex_execs = if session_running
        && matches!(
            state.as_str(),
            "offline" | "degraded" | "circuit-open" | "restarting"
        ) {
        load_codex_exec_process_statuses()
    } else {
        Vec::new()
    };

    WatchdogStatus {
        state,
        restart_count: persisted.restart_count,
        current_backoff_secs: persisted.current_backoff_secs,
        last_exit_category: persisted.last_exit_category,
        last_exit_reason: persisted.last_exit_reason,
        watchdog_pid,
        watchdog_pid_live,
        daemon_pid,
        daemon_pid_live,
        watchdog_state_updated_at,
        watchdog_state_age_secs,
        daemon_state_updated_at,
        daemon_state_age_secs,
        daemon_log_updated_at,
        daemon_log_age_secs,
        orphan_codex_execs,
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
    if let Some(pid) = watchdog.watchdog_pid {
        parts.push(format!(
            "watchdog-pid={pid}:{}",
            if watchdog.watchdog_pid_live {
                "live"
            } else {
                "dead"
            }
        ));
    } else if watchdog.state != "stopped" {
        parts.push("watchdog-pid=missing".to_string());
    }
    if let Some(pid) = watchdog.daemon_pid {
        parts.push(format!(
            "daemon-pid={pid}:{}",
            if watchdog.daemon_pid_live {
                "live"
            } else {
                "dead"
            }
        ));
    } else if matches!(watchdog.state.as_str(), "running" | "degraded" | "offline") {
        parts.push("daemon-pid=missing".to_string());
    }
    if let Some(age_secs) = watchdog.daemon_state_age_secs {
        parts.push(format!("state-age={}", format_health_duration(age_secs)));
    }
    if let Some(updated_at) = watchdog.daemon_state_updated_at {
        parts.push(format!("state-updated={updated_at}"));
    }
    if let Some(age_secs) = watchdog.daemon_log_age_secs {
        parts.push(format!("log-age={}", format_health_duration(age_secs)));
    }
    if let Some(updated_at) = watchdog.daemon_log_updated_at {
        parts.push(format!("log-updated={updated_at}"));
    }
    if !watchdog.orphan_codex_execs.is_empty() {
        let pids = watchdog
            .orphan_codex_execs
            .iter()
            .take(5)
            .map(|process| format!("{}<-{}", process.pid, process.ppid))
            .collect::<Vec<_>>()
            .join(",");
        let suffix = if watchdog.orphan_codex_execs.len() > 5 {
            format!("+{}", watchdog.orphan_codex_execs.len() - 5)
        } else {
            String::new()
        };
        parts.push(format!(
            "orphan-codex-exec={} [{}{}]",
            watchdog.orphan_codex_execs.len(),
            pids,
            suffix
        ));
    }
    if let Some(category) = &watchdog.last_exit_category {
        if category != crate::team::daemon_mgmt::DAEMON_EXIT_CATEGORY_UNKNOWN {
            parts.push(category.clone());
        }
    }
    if let Some(reason) = &watchdog.last_exit_reason {
        parts.push(reason.clone());
    }
    parts.join(" | ")
}

fn load_codex_exec_process_statuses() -> Vec<OrphanProcessStatus> {
    match super::process_tree::codex_exec_processes() {
        Ok(processes) => processes
            .into_iter()
            .map(|process| OrphanProcessStatus {
                pid: process.pid,
                ppid: process.ppid,
                command: process.command,
            })
            .collect(),
        Err(error) => {
            warn!(error = %error, "failed to inspect codex exec processes for status");
            Vec::new()
        }
    }
}

fn read_status_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn file_modified_unix_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
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

fn stale_active_clear_summary(task: Option<&str>, reason: Option<&str>) -> Option<String> {
    let label = match reason {
        Some("clear_done") => "done",
        Some("clear_archived") => "archived",
        Some("clear_missing") => "missing",
        Some("clear_unassigned") => "unassigned",
        Some("release_review") => "review",
        Some("release_blocked") => "blocked",
        _ => return None,
    };
    let task = task.and_then(parse_assigned_task_id);
    Some(match task {
        Some(task_id) => format!("cleared stale active #{task_id} ({label})"),
        None => format!("cleared stale active ({label})"),
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
    if health.proactive_handoff_count > 0 {
        parts.push(format!("ph{}", health.proactive_handoff_count));
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
    if health.stale_active_cleared_count > 0 {
        parts.push(format!("sa{}", health.stale_active_cleared_count));
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
    stale_active_signal: Option<String>,
    stall_signal: Option<String>,
    supervisory_pressure: &SupervisoryPressureSnapshot,
    stale_review_backlog: usize,
) -> Option<String> {
    let stale_review_signal =
        (stale_review_backlog > 0).then(|| format!("stale review ({stale_review_backlog})"));
    let actionable_backlog_present = supervisory_pressure.actionable_count() > 0;
    let mut signals = Vec::new();
    if let Some(existing) = signal {
        signals.push(existing);
    }
    if let Some(branch_mismatch) = branch_mismatch_signal {
        signals.push(branch_mismatch);
    }
    if let Some(stale_active) = stale_active_signal {
        signals.push(stale_active);
    }
    if let Some(stall) = stall_signal
        && (!actionable_backlog_present || is_actionable_control_plane_stall(&stall))
    {
        signals.push(stall);
    }
    if let Some(summary) = supervisory_pressure.status_summary() {
        signals.push(summary);
    }
    if let Some(stale_review) = stale_review_signal {
        signals.push(stale_review);
    }
    if signals.is_empty() {
        None
    } else {
        Some(signals.join(", "))
    }
}

fn is_actionable_control_plane_stall(stall_signal: &str) -> bool {
    stall_signal.contains("stale review backlog")
        || stall_signal.contains("stale direct-report packets")
        || stall_signal.contains("stale dispatch gap")
        || stall_signal.contains("stale planning inbox")
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
    let detail_token = supervisory_slo_status_token(reason);
    format!("{role_token}:{detail_token}")
}

fn supervisory_slo_status_token(reason: &str) -> &'static str {
    if reason.ends_with("review_waiting") || reason.ends_with("review_backlog") {
        "review-wait-timeout"
    } else if reason.ends_with("dispatch_gap") {
        "dispatch-gap-pressure"
    } else if reason.ends_with("direct_report_packets")
        || reason.ends_with("inbox_batching")
        || reason.ends_with("planning_inbox")
    {
        "inbox-backlog-pressure"
    } else {
        "working-timeout"
    }
}

fn supervisory_reason_label(reason: &str) -> &'static str {
    if reason.ends_with("inbox_batching") {
        "inbox batching"
    } else if reason.ends_with("review_waiting") {
        "review waiting"
    } else if reason.ends_with("review_backlog") {
        "stale review backlog"
    } else if reason.ends_with("direct_report_packets") {
        "stale direct-report packets"
    } else if reason.ends_with("dispatch_gap") {
        "stale dispatch gap"
    } else if reason.ends_with("planning_inbox") {
        "stale planning inbox"
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
    let mut latest_pending_by_report: HashMap<String, u64> = HashMap::new();
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
            latest_pending_by_report
                .entry(msg.from.clone())
                .and_modify(|ts| *ts = (*ts).max(msg.timestamp))
                .or_insert(msg.timestamp);
        }
    }

    Ok(TriageBacklogState {
        count: latest_pending_by_report.len(),
        newest_result_ts: latest_pending_by_report
            .values()
            .copied()
            .max()
            .unwrap_or(0),
    })
}

pub(crate) fn pending_inbox_counts(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, usize> {
    let root = inbox::inboxes_root(project_root);
    members
        .iter()
        .filter_map(|member| {
            let count = if matches!(member.role_type, RoleType::Architect | RoleType::Manager) {
                // For supervisors, exclude low-priority status rollups from the inbox
                // count (they would otherwise inflate the number and mask real pending
                // work). Unclassified messages such as direct requests from humans are
                // always counted; only the specific low-signal categories are dropped.
                match inbox::pending_messages(&root, &member.name) {
                    Ok(messages) => messages
                        .iter()
                        .filter(|msg| {
                            !matches!(
                                classify_supervisory_pressure_normalized(&normalized_body(
                                    &msg.body
                                )),
                                Some(
                                    SupervisoryPressure::StatusUpdate
                                        | SupervisoryPressure::ResolvedUpdate
                                        | SupervisoryPressure::RecoveryUpdate
                                        | SupervisoryPressure::IdleNudge
                                        | SupervisoryPressure::ReviewNudge
                                )
                            )
                        })
                        .count(),
                    Err(error) => {
                        warn!(member = %member.name, error = %error, "failed to read pending inbox messages");
                        return None;
                    }
                }
            } else {
                match inbox::pending_message_count(&root, &member.name) {
                    Ok(count) => count,
                    Err(error) => {
                        warn!(member = %member.name, error = %error, "failed to count pending inbox messages");
                        return None;
                    }
                }
            };
            Some((member.name.clone(), count))
        })
        .collect()
}

pub(crate) fn supervisory_status_pressure(
    project_root: &Path,
    members: &[MemberInstance],
    session_running: bool,
    runtime_statuses: &HashMap<String, RuntimeMemberStatus>,
) -> HashMap<String, SupervisoryPressureSnapshot> {
    let activity = members
        .iter()
        .map(|member| {
            let idle = session_running
                && runtime_statuses
                    .get(&member.name)
                    .is_some_and(|runtime| runtime.state == "idle");
            (member.name.clone(), SupervisoryMemberActivity { idle })
        })
        .collect::<HashMap<_, _>>();
    supervisory_pressure_snapshots(project_root, members, &activity)
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
    let tasks = match load_board_tasks_for_status(
        tasks_dir.parent().unwrap_or(&tasks_dir),
        "owned_task_buckets",
    ) {
        Ok(tasks) => tasks,
        Err(error) => {
            warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for status");
            return HashMap::new();
        }
    };

    for task in &tasks {
        let Some(claimed_by) = task.claimed_by.as_deref() else {
            continue;
        };
        if !member_names.contains(claimed_by) {
            continue;
        }
        let Some(is_active) = classify_owned_task_status(task.status.as_str()) else {
            continue;
        };
        let owner = if is_active {
            claimed_by.to_string()
        } else {
            members
                .iter()
                .find(|member| member.name == claimed_by)
                .and_then(|member| member.reports_to.as_deref())
                .unwrap_or(claimed_by)
                .to_string()
        };
        let entry = owned.entry(owner).or_default();
        if is_active {
            entry.active.push(task.id);
        } else {
            match super::review::classify_review_task(project_root, task, &tasks) {
                ReviewQueueState::Current => entry.review.push(task.id),
                ReviewQueueState::Stale(_) => entry.stale_review.push(task.id),
            }
        }
    }

    for buckets in owned.values_mut() {
        buckets.active.sort_unstable();
        buckets.review.sort_unstable();
        buckets.stale_review.sort_unstable();
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

pub(crate) fn format_failed_test_task(task: &StatusTaskEntry) -> Option<String> {
    let summary = task.test_summary.as_deref()?;
    let owner = task.claimed_by.as_deref().unwrap_or("unclaimed");
    let artifact = task.latest_artifact.as_deref().unwrap_or("none");
    let state = task.failed_test_state.as_deref().unwrap_or("owner-locked");
    Some(format!(
        "#{} {} (owner: {}; artifact: {}; {}): {}",
        task.id, task.title, owner, artifact, state, summary
    ))
}

fn verification_retry_rework(metadata: &crate::team::board::WorkflowMetadata) -> bool {
    metadata.tests_passed == Some(false)
        && metadata.outcome.as_deref() == Some("verification_retry_required")
        && !metadata.artifacts.is_empty()
}

fn failed_test_summary(metadata: &crate::team::board::WorkflowMetadata) -> Option<String> {
    metadata
        .test_results
        .clone()
        .filter(|results| results.failed > 0)
        .map(|results| results.failure_summary())
        .or_else(|| {
            (metadata.tests_passed == Some(false)).then(|| {
                if metadata.outcome.as_deref() == Some("verification_retry_required") {
                    "verification retry required".to_string()
                } else {
                    "tests failed".to_string()
                }
            })
        })
}

fn current_owner_active_on_task(
    task: &task::Task,
    daemon_state: &PersistedDaemonHealthState,
) -> bool {
    let Some(owner) = task.claimed_by.as_deref() else {
        return false;
    };
    daemon_state.active_tasks.get(owner) == Some(&task.id)
        && daemon_state.states.get(owner) == Some(&MemberState::Working)
}

fn failed_test_state(
    task: &task::Task,
    retry_rework: bool,
    daemon_state: &PersistedDaemonHealthState,
) -> String {
    if retry_rework && !current_owner_active_on_task(task, daemon_state) {
        "dispatchable rework".to_string()
    } else {
        "owner-locked".to_string()
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
    let tasks = load_board_tasks_for_status(
        tasks_dir.parent().unwrap_or(&tasks_dir),
        "board_status_task_queues",
    )?;
    let github_feedback =
        crate::team::github_feedback::summarize_github_feedback_for_tasks(project_root, &tasks)
            .unwrap_or_default();
    let daemon_state = load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .unwrap_or_default()
        .unwrap_or_default();
    for task in &tasks {
        let inferred = infer_runtime_task_metadata(project_root, task);
        let branch_mismatch = task_branch_mismatch(project_root, task, &inferred);
        let review_state = super::review::classify_review_task(project_root, task, &tasks);
        let workflow_metadata = crate::team::board::read_workflow_metadata(&task.source_path).ok();
        let latest_artifact = workflow_metadata
            .as_ref()
            .and_then(|metadata| metadata.artifacts.last().cloned());
        let retry_rework = workflow_metadata
            .as_ref()
            .is_some_and(verification_retry_rework);
        let mut test_summary = workflow_metadata.as_ref().and_then(failed_test_summary);
        let mut blocked_on = task.blocked_on.clone();
        let mut next_action = match review_state {
            ReviewQueueState::Current => task.next_action.clone(),
            ReviewQueueState::Stale(stale) => Some(stale.status_next_action()),
        };
        if let Some(feedback) = github_feedback.failed.get(&task.id) {
            blocked_on = Some(feedback.blocked_on_summary());
            next_action = Some(feedback.next_action_summary());
            test_summary = Some(feedback.status_summary());
        } else if test_summary.is_none()
            && let Some(feedback) = github_feedback.passed.get(&task.id)
        {
            test_summary = Some(feedback.status_summary());
        }
        let failed_test_state = test_summary
            .as_ref()
            .map(|_| failed_test_state(task, retry_rework, &daemon_state));
        let entry = StatusTaskEntry {
            id: task.id,
            title: task.title.clone(),
            status: task.status.clone(),
            priority: task.priority.clone(),
            claimed_by: task.claimed_by.clone(),
            review_owner: task.review_owner.clone(),
            blocked_on,
            branch: task.branch.clone().or_else(|| inferred.branch.clone()),
            worktree_path: task
                .worktree_path
                .clone()
                .or_else(|| inferred.worktree_path.clone()),
            commit: task.commit.clone().or_else(|| inferred.commit.clone()),
            branch_mismatch,
            next_action,
            test_summary,
            latest_artifact,
            failed_test_state,
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

fn task_branch_mismatch(
    project_root: &Path,
    task: &task::Task,
    inferred: &InferredTaskMetadata,
) -> Option<String> {
    let current_branch = inferred.branch.as_deref()?;
    task_branch_signal_for_task(project_root, task, current_branch)
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
    pub(crate) main_smoke: Option<MainSmokeState>,
    pub(crate) watchdog: WatchdogStatus,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    pub(crate) publish_handoff: Option<crate::release::ReleasePublishHandoff>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    pub(crate) optional_subsystems: Option<Vec<OptionalSubsystemStatus>>,
    pub(crate) engineer_profiles:
        Option<Vec<crate::team::telemetry_db::EngineerPerformanceProfileRow>>,
    pub(crate) members: Vec<TeamStatusRow>,
}

pub(crate) fn load_main_smoke_state(project_root: &Path) -> Option<MainSmokeState> {
    load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .ok()
        .flatten()
        .and_then(|state| state.main_smoke_state)
}

pub(crate) fn format_main_smoke_summary(main_smoke: &MainSmokeState) -> String {
    if main_smoke.broken {
        let broken_commit = main_smoke.broken_commit.as_deref().unwrap_or("unknown");
        let suspects = if main_smoke.suspects.is_empty() {
            "none".to_string()
        } else {
            main_smoke.suspects.join(", ")
        };
        let summary = main_smoke.summary.as_deref().unwrap_or("main smoke failed");
        format!("BROKEN by {broken_commit}; suspects: [{suspects}]; {summary}")
    } else {
        let commit = main_smoke
            .last_success_commit
            .as_deref()
            .unwrap_or("unknown");
        let summary = main_smoke.summary.as_deref().unwrap_or("main smoke passed");
        format!("healthy at {commit}; {summary}")
    }
}

pub(crate) fn format_publish_handoff_summary(
    handoff: &crate::release::ReleasePublishHandoff,
) -> String {
    let mut summary = format!("{} ({})", handoff.path, handoff.status);
    if !handoff.blocked_reasons.is_empty() {
        summary.push_str(": ");
        summary.push_str(&handoff.blocked_reasons.join("; "));
    }
    summary
}

pub(crate) fn build_team_status_json_report(
    input: TeamStatusJsonReportInput,
) -> TeamStatusJsonReport {
    let TeamStatusJsonReportInput {
        team,
        session,
        session_running,
        paused,
        main_smoke,
        watchdog,
        workflow_metrics,
        publish_handoff,
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
        main_smoke,
        watchdog,
        health,
        workflow_metrics,
        publish_handoff,
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
        board_state: board_metrics.board_state,
        runnable_count: board_metrics.runnable_count,
        implementation_runnable_count: board_metrics.implementation_runnable_count,
        blocked_count: board_metrics.blocked_count,
        in_review_count: board_metrics.in_review_count,
        actionable_review_count: board_metrics.actionable_review_count,
        in_progress_count: board_metrics.in_progress_count,
        stale_in_progress_count: board_metrics.stale_in_progress_count,
        aged_todo_count: board_metrics.aged_todo_count,
        stale_review_count: board_metrics.stale_review_count,
        idle_with_runnable: board_metrics.idle_with_runnable,
        top_runnable_tasks: board_metrics.top_runnable_tasks,
        blocked_dispatch_reasons: board_metrics.blocked_dispatch_reasons,
        oldest_review_age_secs: board_metrics.oldest_review_age_secs,
        oldest_assignment_age_secs: board_metrics.oldest_assignment_age_secs,
        auto_merge_count: review.auto_merge_count,
        manual_merge_count: review.manual_merge_count,
        direct_root_merge_count: review.direct_root_merge_count,
        isolated_integration_merge_count: review.isolated_integration_merge_count,
        direct_root_failure_count: review.direct_root_failure_count,
        isolated_integration_failure_count: review.isolated_integration_failure_count,
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
    board_state: WorkflowBoardState,
    runnable_count: u32,
    implementation_runnable_count: u32,
    blocked_count: u32,
    in_review_count: u32,
    actionable_review_count: u32,
    in_progress_count: u32,
    stale_in_progress_count: u32,
    aged_todo_count: u32,
    stale_review_count: u32,
    idle_with_runnable: Vec<String>,
    top_runnable_tasks: Vec<String>,
    blocked_dispatch_reasons: Vec<String>,
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
            board_state: WorkflowBoardState::EmptyBoard,
            runnable_count: 0,
            implementation_runnable_count: 0,
            blocked_count: 0,
            in_review_count: 0,
            actionable_review_count: 0,
            in_progress_count: 0,
            stale_in_progress_count: 0,
            aged_todo_count: 0,
            stale_review_count: 0,
            idle_with_runnable: Vec::new(),
            top_runnable_tasks: Vec::new(),
            blocked_dispatch_reasons: Vec::new(),
            oldest_review_age_secs: None,
            oldest_assignment_age_secs: None,
        });
    }

    let tasks = load_board_tasks_for_status(board_dir, "compute_board_metrics")?;
    if tasks.is_empty() {
        return Ok(BoardMetrics {
            board_state: WorkflowBoardState::EmptyBoard,
            runnable_count: 0,
            implementation_runnable_count: 0,
            blocked_count: 0,
            in_review_count: 0,
            actionable_review_count: 0,
            in_progress_count: 0,
            stale_in_progress_count: 0,
            aged_todo_count: 0,
            stale_review_count: 0,
            idle_with_runnable: Vec::new(),
            top_runnable_tasks: Vec::new(),
            blocked_dispatch_reasons: Vec::new(),
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
    let actionable_review_count = tasks
        .iter()
        .filter(|task| task.status == "review")
        .filter(|task| !is_review_task_explicitly_blocked(task))
        .count() as u32;
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

    // #711: "idle with runnable" must only count tasks an engineer could
    // actually take. A task owned by the architect (body "Owner: maya-lead")
    // or manager (frontmatter assignee: jordan-pm) is runnable-in-general but
    // not engineer-runnable. Counting those caused the status display and
    // standup report to warn "idle while runnable work exists" to engineers
    // who had nothing to pick up, and surfaced the same false signal in
    // jordan-pm's standup workflow-signals. Same classification pattern as
    // #709 on the dispatch-gap / planning-cycle paths.
    let non_engineer_names: HashSet<String> = members
        .iter()
        .filter(|member| member.role_type != RoleType::Engineer)
        .map(|member| member.name.clone())
        .collect();
    let engineer_runnable_count = dispatchable_tasks
        .iter()
        .filter(|task| crate::team::resolver::is_engineer_dispatchable(task, &non_engineer_names))
        .count() as u32;
    let idle_with_runnable =
        compute_idle_with_runnable(board_dir, members, &tasks, engineer_runnable_count);
    let top_runnable_tasks = top_runnable_task_summaries(&dispatchable_tasks, 3);
    let blocked_dispatch_reasons =
        blocked_dispatch_reason_summaries(&tasks, &dispatchable_task_ids, 3);
    let board_state = classify_workflow_board_state(
        &tasks,
        runnable_count,
        blocked_count,
        actionable_review_count,
    );
    let aging = project_root_from_board_dir(board_dir)
        .and_then(|project_root| {
            crate::team::board::compute_task_aging(board_dir, project_root, thresholds).ok()
        })
        .unwrap_or_default();

    Ok(BoardMetrics {
        board_state,
        runnable_count,
        implementation_runnable_count: engineer_runnable_count,
        blocked_count,
        in_review_count,
        actionable_review_count,
        in_progress_count,
        stale_in_progress_count: aging.stale_in_progress.len() as u32,
        aged_todo_count: aging.aged_todo.len() as u32,
        stale_review_count: aging.stale_review.len() as u32,
        idle_with_runnable,
        top_runnable_tasks,
        blocked_dispatch_reasons,
        oldest_review_age_secs,
        oldest_assignment_age_secs,
    })
}

fn classify_workflow_board_state(
    tasks: &[task::Task],
    runnable_count: u32,
    blocked_count: u32,
    actionable_review_count: u32,
) -> WorkflowBoardState {
    if tasks
        .iter()
        .all(|task| matches!(task.status.as_str(), "done" | "archived"))
    {
        return WorkflowBoardState::EmptyBoard;
    }

    if runnable_count > 0 {
        return WorkflowBoardState::RunnableBoard;
    }

    if actionable_review_count > 0 {
        return WorkflowBoardState::ReviewBacklogGated;
    }

    if blocked_count > 0 {
        return WorkflowBoardState::BlockedOnlyBoard;
    }

    WorkflowBoardState::ActiveBoard
}

#[derive(Default)]
struct ReviewMetrics {
    auto_merge_count: u32,
    manual_merge_count: u32,
    direct_root_merge_count: u32,
    isolated_integration_merge_count: u32,
    direct_root_failure_count: u32,
    isolated_integration_failure_count: u32,
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
    let mut direct_root_merge_count: u32 = 0;
    let mut isolated_integration_merge_count: u32 = 0;
    let mut direct_root_failure_count: u32 = 0;
    let mut isolated_integration_failure_count: u32 = 0;
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
                match event.merge_mode.as_deref() {
                    Some("direct_root") => direct_root_merge_count += 1,
                    Some("isolated_integration") => isolated_integration_merge_count += 1,
                    _ => {}
                }
                if let Some(task_id) = &event.task {
                    if let Some(enter_ts) = review_enter_ts.remove(task_id) {
                        review_latencies.push((event.ts - enter_ts) as f64);
                    }
                }
            }
            "task_manual_merged" => {
                manual_merge_count += 1;
                match event.merge_mode.as_deref() {
                    Some("direct_root") => direct_root_merge_count += 1,
                    Some("isolated_integration") => isolated_integration_merge_count += 1,
                    _ => {}
                }
                if let Some(task_id) = &event.task {
                    if let Some(enter_ts) = review_enter_ts.remove(task_id) {
                        review_latencies.push((event.ts - enter_ts) as f64);
                    }
                }
            }
            "task_reworked" => {
                rework_count += 1;
            }
            "task_merge_failed" => match event.merge_mode.as_deref() {
                Some("direct_root") => direct_root_failure_count += 1,
                Some("isolated_integration") => isolated_integration_failure_count += 1,
                _ => {}
            },
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
        direct_root_merge_count,
        isolated_integration_merge_count,
        direct_root_failure_count,
        isolated_integration_failure_count,
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
    let direct_root_merge_count = row.direct_root_merge_count as u32;
    let isolated_integration_merge_count = row.isolated_integration_merge_count as u32;
    let direct_root_failure_count = row.direct_root_failure_count as u32;
    let isolated_integration_failure_count = row.isolated_integration_failure_count as u32;
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
        direct_root_merge_count,
        isolated_integration_merge_count,
        direct_root_failure_count,
        isolated_integration_failure_count,
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
    let blocked_dispatch = if metrics.blocked_dispatch_reasons.is_empty() {
        "-".to_string()
    } else {
        metrics.blocked_dispatch_reasons.join("; ")
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
    let implementation_summary = crate::team::tact::parser::implementation_work_summary(
        metrics.implementation_runnable_count as usize,
        metrics.actionable_review_count as usize,
    );

    format!(
        "Workflow Metrics\n\
Board State: {}\n\
Runnable: {}\n\
Implementation Runnable: {}\n\
Blocked: {}\n\
In Review: {}\n\
Actionable Review: {}\n\
In Progress: {}\n\
Implementation Work: {}\n\
Aging Alerts: stale in-progress {} | aged todo {} | stale review {}\n\
Idle With Runnable: {}\n\
Top Runnable: {}\n\
Blocked Dispatch: {}\n\
Oldest Review Age: {}\n\
Oldest Assignment Age: {}\n\n\
Review Pipeline\n\
Queue: {} | Avg Latency: {} | Auto-merge Rate: {} | Rework Rate: {}\n\
Auto: {} | Manual: {} | Rework: {} | Nudges: {} | Escalations: {}\n\
Merge Modes: direct ok {} / fail {} | isolated ok {} / fail {}",
        metrics.board_state.as_str(),
        metrics.runnable_count,
        metrics.implementation_runnable_count,
        metrics.blocked_count,
        metrics.in_review_count,
        metrics.actionable_review_count,
        metrics.in_progress_count,
        implementation_summary,
        metrics.stale_in_progress_count,
        metrics.aged_todo_count,
        metrics.stale_review_count,
        idle,
        top_runnable,
        blocked_dispatch,
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
        metrics.direct_root_merge_count,
        metrics.direct_root_failure_count,
        metrics.isolated_integration_merge_count,
        metrics.isolated_integration_failure_count,
    )
}

pub(crate) fn format_review_blocks(review_queue: &[StatusTaskEntry]) -> Option<String> {
    let blocked = review_queue
        .iter()
        .filter_map(|task| {
            task.blocked_on.as_ref().map(|blocked_on| {
                let next_action = task
                    .next_action
                    .as_deref()
                    .unwrap_or("resolve the blocker, then retry review");
                format!(
                    "#{} {}: {blocked_on} Next: {next_action}",
                    task.id, task.title
                )
            })
        })
        .collect::<Vec<_>>();
    if blocked.is_empty() {
        return None;
    }

    let mut lines = vec!["Review Blocks".to_string()];
    lines.extend(blocked.into_iter().map(|line| format!("- {line}")));
    Some(lines.join("\n"))
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

pub(crate) fn is_review_task_explicitly_blocked(task: &task::Task) -> bool {
    task.blocked
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || task
            .blocked_on
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
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

fn blocked_dispatch_reason_summaries(
    tasks: &[task::Task],
    dispatchable_task_ids: &HashSet<u32>,
    limit: usize,
) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "todo" | "backlog" | "runnable"))
        .filter(|task| !dispatchable_task_ids.contains(&task.id))
        .filter_map(|task| {
            crate::team::resolver::dispatch_blocking_reason(task, tasks)
                .map(|reason| format!("#{} {}: {}", task.id, task.title, reason))
        })
        .take(limit)
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
    // Use read-only opener to avoid blocking on the daemon's write lock (#676).
    let db = crate::team::telemetry_db::open_readonly(project_root)
        .ok()
        .flatten();
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
            let mut formatted = format_metrics(&metrics);

            // Append binary freshness line (#675).
            let freshness_line = binary_freshness_status_line(project_root);
            formatted.push('\n');
            formatted.push_str(&freshness_line);

            Some((formatted, metrics))
        }
        Err(error) => {
            warn!(path = %board_dir.display(), error = %error, "failed to compute workflow metrics");
            None
        }
    }
}

/// Compute the binary freshness status line for `batty status` (#675).
fn binary_freshness_status_line(project_root: &Path) -> String {
    let binary_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return "Daemon Binary: unknown (cannot resolve exe)".to_string(),
    };
    let result = crate::team::daemon::health::binary_freshness::evaluate_binary_freshness(
        &binary_path,
        project_root,
    );
    match result {
        Ok(Some(report)) => {
            if report.fresh {
                return report.status_line();
            }
            match crate::team::daemon::health::binary_freshness::load_binary_refresh_state(
                project_root,
            ) {
                Ok(Some(state)) if state.matches_report(&report) => state.status_line(),
                _ => report.status_line(),
            }
        }
        Ok(None) => "Daemon Binary: n/a".to_string(),
        Err(_) => "Daemon Binary: unknown (check failed)".to_string(),
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

        let _ = crate::tmux::run_tmux_with_timeout(
            ["set-option", "-p", "-t", pane_id, "@batty_status", &label],
            "set-option @batty_status",
            Some(pane_id),
        );
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
    use rusqlite::Connection;
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

    fn create_legacy_telemetry_db(project_root: &Path) -> Connection {
        let batty_dir = project_root.join(".batty");
        fs::create_dir_all(&batty_dir).unwrap();
        let conn = Connection::open(batty_dir.join("telemetry.db")).unwrap();
        crate::team::telemetry_db::install_legacy_schema_for_tests(&conn).unwrap();
        conn
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
        let supervisory_pressure = HashMap::from([("manager".to_string(), {
            let mut snapshot = SupervisoryPressureSnapshot::default();
            snapshot.add_pressure(
                crate::team::supervisory_notice::SupervisoryPressure::TriageBacklog,
                2,
            );
            snapshot
        })]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &triage_backlog_counts,
            &HashMap::new(),
            &supervisory_pressure,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "triaging");
        assert_eq!(rows[0].pending_inbox, 1);
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("pressure 1: direct-report packets (2)")
        );
    }

    #[test]
    fn delivered_direct_report_triage_state_dedupes_repeated_delivered_results() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&inbox_root, "lead").unwrap();
        crate::team::inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        let mut first = InboxMessage::new_send("eng-1", "lead", "first result");
        first.timestamp = now_unix().saturating_sub(30);
        let first_id = crate::team::inbox::deliver_to_inbox(&inbox_root, &first).unwrap();
        crate::team::inbox::mark_delivered(&inbox_root, "lead", &first_id).unwrap();

        let mut second = InboxMessage::new_send("eng-1", "lead", "second result");
        second.timestamp = now_unix().saturating_sub(10);
        let second_id = crate::team::inbox::deliver_to_inbox(&inbox_root, &second).unwrap();
        crate::team::inbox::mark_delivered(&inbox_root, "lead", &second_id).unwrap();

        let state =
            delivered_direct_report_triage_state(&inbox_root, "lead", &["eng-1".to_string()])
                .unwrap();
        assert_eq!(state.count, 1);
        assert_eq!(state.newest_result_ts, second.timestamp);
    }

    #[test]
    fn pending_inbox_counts_for_supervisors_ignore_stale_status_rollups() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&inbox_root, "manager").unwrap();

        crate::team::inbox::deliver_to_inbox(
            &inbox_root,
            &InboxMessage::new_send(
                "daemon",
                "manager",
                "Rollup: review backlog is healthy and no action is required right now.",
            ),
        )
        .unwrap();
        crate::team::inbox::deliver_to_inbox(
            &inbox_root,
            &InboxMessage::new_send(
                "daemon",
                "manager",
                "Dispatch queue entry failed validation too many times.",
            ),
        )
        .unwrap();

        let counts = pending_inbox_counts(tmp.path(), &[manager("manager")]);
        assert_eq!(counts.get("manager"), Some(&1usize));
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
                stale_review: Vec::new(),
            },
        )]);
        let supervisory_pressure = HashMap::from([("manager".to_string(), {
            let mut snapshot = SupervisoryPressureSnapshot::default();
            snapshot.add_pressure(
                crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
                2,
            );
            snapshot
        })]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &owned_task_buckets,
            &supervisory_pressure,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "reviewing");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("nudge paused, pressure 1: review backlog (2)")
        );
    }

    #[test]
    fn build_team_status_rows_distinguishes_stale_review_backlog() {
        let members = vec![manager("manager")];
        let runtime_statuses = HashMap::from([(
            "manager".to_string(),
            RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: None,
                label: Some("idle".to_string()),
            },
        )]);
        let owned_task_buckets = HashMap::from([(
            "manager".to_string(),
            OwnedTaskBuckets {
                active: Vec::new(),
                review: vec![41],
                stale_review: vec![42],
            },
        )]);
        let supervisory_pressure = HashMap::from([("manager".to_string(), {
            let mut snapshot = SupervisoryPressureSnapshot::default();
            snapshot.add_pressure(
                crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
                1,
            );
            snapshot
        })]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &owned_task_buckets,
            &supervisory_pressure,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "reviewing");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("pressure 1: review backlog (1), stale review (1)")
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
            &HashMap::new(),
            &agent_health,
        );

        assert_eq!(rows[0].health_summary, "stall:architect:working-timeout");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("architect (architect) stalled after 5m: no actionable progress")
        );
        assert_eq!(rows[1].health_summary, "stall:manager:working-timeout");
        assert_eq!(
            rows[1].signal.as_deref(),
            Some("nudge paused, manager (manager) stalled after 5m: shim activity only")
        );
    }

    #[test]
    fn build_team_status_rows_keeps_actionable_control_plane_stall_with_pressure() {
        let members = vec![manager("manager")];
        let runtime_statuses = HashMap::from([(
            "manager".to_string(),
            RuntimeMemberStatus {
                state: "working".to_string(),
                signal: Some("nudge paused".to_string()),
                label: Some("working".to_string()),
            },
        )]);
        let supervisory_pressure = HashMap::from([("manager".to_string(), {
            let mut snapshot = SupervisoryPressureSnapshot::default();
            snapshot.add_pressure(
                crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
                2,
            );
            snapshot
        })]);
        let agent_health = HashMap::from([(
            "manager".to_string(),
            AgentHealthSummary {
                stall_reason: Some("supervisory_stalled_manager_review_backlog".to_string()),
                stall_summary: Some(
                    "manager (manager) stalled after 5m: stale review backlog; next: review and disposition queued work"
                        .to_string(),
                ),
                ..AgentHealthSummary::default()
            },
        )]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &supervisory_pressure,
            &HashMap::new(),
            &HashMap::new(),
            &agent_health,
        );

        assert_eq!(rows[0].state, "working");
        assert_eq!(rows[0].health_summary, "stall:manager:review-wait-timeout");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some(
                "nudge paused, manager (manager) stalled after 5m: stale review backlog; next: review and disposition queued work, pressure 1: review backlog (2)"
            )
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
            &HashMap::new(),
        );

        assert_eq!(rows[0].state, "starting");
        assert_eq!(rows[0].runtime_label, None);
    }

    #[test]
    fn build_team_status_rows_surfaces_cleared_stale_active_lane() {
        let members = vec![engineer("eng-1")];
        let runtime_statuses = HashMap::from([(
            "eng-1".to_string(),
            RuntimeMemberStatus {
                state: "idle".to_string(),
                signal: None,
                label: None,
            },
        )]);
        let agent_health = HashMap::from([(
            "eng-1".to_string(),
            AgentHealthSummary {
                stale_active_cleared_count: 1,
                stale_active_summary: Some("cleared stale active #42 (done)".to_string()),
                ..AgentHealthSummary::default()
            },
        )]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &agent_health,
        );

        assert_eq!(rows[0].state, "idle");
        assert_eq!(
            rows[0].signal.as_deref(),
            Some("cleared stale active #42 (done)")
        );
        assert_eq!(rows[0].health_summary, "sa1");
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
                stale_review: Vec::new(),
            },
        )]);
        let branch_mismatches = HashMap::from([(
            "eng-1".to_string(),
            "branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)".to_string(),
        )]);

        let rows = build_team_status_rows(
            &members,
            true,
            &runtime_statuses,
            &HashMap::new(),
            &HashMap::new(),
            &owned_task_buckets,
            &HashMap::new(),
            &branch_mismatches,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(
            rows[0].signal.as_deref(),
            Some(
                "nudge paused, branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
            )
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
            Some(
                "branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
            )
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
    fn branch_mismatch_by_member_surfaces_preserved_completed_lane_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-preserved-done");
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

        let record = crate::team::checkpoint::PreservedLaneRecord::commit(
            "eng-1",
            &task::Task {
                id: 628,
                title: "done lane".to_string(),
                status: "done".to_string(),
                priority: "high".to_string(),
                assignee: None,
                claimed_by: Some("eng-1".to_string()),
                claimed_at: None,
                claim_ttl_secs: None,
                claim_expires_at: None,
                last_progress_at: None,
                claim_warning_sent_at: None,
                claim_extensions: None,
                last_output_bytes: None,
                blocked: None,
                tags: vec![],
                depends_on: vec![],
                review_owner: None,
                blocked_on: None,
                worktree_path: Some(".batty/worktrees/eng-1".to_string()),
                branch: Some("eng-1/628".to_string()),
                commit: None,
                artifacts: vec![],
                next_action: None,
                scheduled_for: None,
                cron_schedule: None,
                cron_last_run: None,
                completed: None,
                description: "done".to_string(),
                batty_config: None,
                source_path: repo
                    .join(".batty")
                    .join("team_config")
                    .join("board")
                    .join("tasks")
                    .join("628-done.md"),
            },
            "eng-1/628",
            &base_branch,
            "completed task no longer needs engineer lane",
            Some("abc1234567890".to_string()),
            "def4567890abc".to_string(),
        );
        crate::team::checkpoint::write_preserved_lane_record(&repo, &record).unwrap();

        let mut member = engineer("eng-1");
        member.use_worktrees = true;
        let mismatches = branch_mismatch_by_member(&repo, &[member]);

        assert_eq!(
            mismatches.get("eng-1").map(String::as_str),
            Some("saved completed lane #628 before cleanup (commit eng-1/628 @ def4567890ab)")
        );
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
    fn status_worktree_repo_targets_discovers_git_children_under_non_git_worktree_root() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1");
        let sub_repo = worktree_dir.join("repo");
        fs::create_dir_all(&sub_repo).unwrap();
        crate::team::test_support::git_ok(&sub_repo, &["init", "-b", "main"]);
        crate::team::test_support::git_ok(
            &sub_repo,
            &["config", "user.email", "batty@example.com"],
        );
        crate::team::test_support::git_ok(&sub_repo, &["config", "user.name", "Batty Tests"]);
        fs::write(sub_repo.join("README.md"), "hello\n").unwrap();
        crate::team::test_support::git_ok(&sub_repo, &["add", "README.md"]);
        crate::team::test_support::git_ok(&sub_repo, &["commit", "-m", "initial"]);

        let targets = status_worktree_repo_targets(&worktree_dir);

        assert_eq!(
            targets,
            vec![StatusWorktreeRepoTarget {
                label: Some("repo".to_string()),
                path: sub_repo,
            }]
        );
    }

    #[test]
    fn worktree_staleness_by_member_uses_subrepos_for_multi_repo_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1");
        let sub_repo = worktree_dir.join("repo");
        fs::create_dir_all(&sub_repo).unwrap();
        crate::team::test_support::git_ok(&sub_repo, &["init", "-b", "main"]);
        crate::team::test_support::git_ok(
            &sub_repo,
            &["config", "user.email", "batty@example.com"],
        );
        crate::team::test_support::git_ok(&sub_repo, &["config", "user.name", "Batty Tests"]);
        fs::write(sub_repo.join("README.md"), "base\n").unwrap();
        crate::team::test_support::git_ok(&sub_repo, &["add", "README.md"]);
        crate::team::test_support::git_ok(&sub_repo, &["commit", "-m", "initial"]);
        crate::team::test_support::git_ok(&sub_repo, &["checkout", "-b", "feature"]);
        crate::team::test_support::git_ok(&sub_repo, &["checkout", "main"]);
        fs::write(sub_repo.join("README.md"), "main update\n").unwrap();
        crate::team::test_support::git_ok(&sub_repo, &["commit", "-am", "main update"]);
        crate::team::test_support::git_ok(&sub_repo, &["checkout", "feature"]);

        let mut member = engineer("eng-1");
        member.use_worktrees = true;

        let staleness = worktree_staleness_by_member(tmp.path(), &[member]);

        assert_eq!(staleness.get("eng-1"), Some(&1));
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
                    proactive_handoff_count: 0,
                    delivery_failure_count: 0,
                    supervisory_digest_count: 0,
                    dispatch_fallback_count: 0,
                    dispatch_fallback_reason: None,
                    stale_active_cleared_count: 0,
                    stale_active_summary: None,
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
                    proactive_handoff_count: 0,
                    delivery_failure_count: 1,
                    supervisory_digest_count: 0,
                    dispatch_fallback_count: 0,
                    dispatch_fallback_reason: None,
                    stale_active_cleared_count: 0,
                    stale_active_summary: None,
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
                review: vec![4],
                stale_review: vec![3],
            })
        );
        assert_eq!(
            buckets.get("eng-2"),
            Some(&OwnedTaskBuckets {
                active: vec![5],
                review: Vec::new(),
                stale_review: Vec::new(),
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
        assert_eq!(metrics.board_state, WorkflowBoardState::EmptyBoard);
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

        assert_eq!(metrics.board_state, WorkflowBoardState::RunnableBoard);
        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.implementation_runnable_count, 1);
        assert_eq!(metrics.blocked_count, 1);
        assert_eq!(metrics.in_review_count, 1);
        assert_eq!(metrics.actionable_review_count, 1);
        assert_eq!(metrics.in_progress_count, 2);
        assert_eq!(metrics.idle_with_runnable, vec!["eng-4".to_string()]);
        assert_eq!(
            metrics.blocked_dispatch_reasons,
            vec![
                "#5 Claimed todo: claimed by eng-3".to_string(),
                "#6 Waiting: unmet dependency #7 (in-progress)".to_string(),
            ]
        );
        assert!(metrics.oldest_review_age_secs.is_some());
        assert!(metrics.oldest_assignment_age_secs.is_some());
    }

    #[test]
    fn idle_with_runnable_excludes_non_engineer_owned_runnable() {
        // Regression for #711 — when the only runnable task is body-owned by
        // the architect, compute_idle_with_runnable must not list engineers
        // as "idle with runnable" because they cannot dispatch it.
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = board_dir(tmp.path()).join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        // Maya-owned strategy task — runnable but not engineer-runnable.
        fs::write(
            tasks_dir.join("001-maya-strategy.md"),
            "---\nid: 1\ntitle: Maya strategy\nstatus: todo\npriority: high\nclass: standard\n---\n\n**Owner:** maya-lead (architect strategy work)\n",
        )
        .unwrap();

        let metrics = compute_metrics(
            &board_dir(tmp.path()),
            &[architect("maya-lead"), engineer("eng-1"), engineer("eng-2")],
        )
        .unwrap();

        // runnable_count still reports 1 — the task IS dispatchable in general.
        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.implementation_runnable_count, 0);
        // But no engineer should be flagged as idle-with-runnable because
        // no engineer-dispatchable task exists.
        assert!(
            metrics.idle_with_runnable.is_empty(),
            "expected no idle-with-runnable engineers, got {:?}",
            metrics.idle_with_runnable
        );
    }

    #[test]
    fn idle_with_runnable_lists_engineers_when_engineer_task_available() {
        // Companion test for #711 — when at least one engineer-dispatchable
        // task exists, idle engineers are still flagged.
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = board_dir(tmp.path()).join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        // Maya-owned (non-engineer) — filtered out.
        fs::write(
            tasks_dir.join("001-maya-strategy.md"),
            "---\nid: 1\ntitle: Maya strategy\nstatus: todo\npriority: high\nclass: standard\n---\n\n**Owner:** maya-lead\n",
        )
        .unwrap();
        // Plain engineer-runnable task — keeps idle_with_runnable non-empty.
        fs::write(
            tasks_dir.join("002-engineer-todo.md"),
            "---\nid: 2\ntitle: Engineer work\nstatus: todo\npriority: medium\nclass: standard\n---\n\nRegular engineer work.\n",
        )
        .unwrap();

        let metrics = compute_metrics(
            &board_dir(tmp.path()),
            &[architect("maya-lead"), engineer("eng-1")],
        )
        .unwrap();

        assert_eq!(metrics.runnable_count, 2);
        assert_eq!(metrics.implementation_runnable_count, 1);
        assert_eq!(metrics.idle_with_runnable, vec!["eng-1".to_string()]);
    }

    #[test]
    fn format_metrics_distinguishes_no_work_from_review_bottleneck() {
        let no_work = format_metrics(&WorkflowMetrics::default());
        assert!(no_work.contains("Board State: empty-board"));
        assert!(no_work.contains("Implementation Work: no executable implementation work"));

        let review_bottleneck = format_metrics(&WorkflowMetrics {
            board_state: WorkflowBoardState::ReviewBacklogGated,
            in_review_count: 1,
            actionable_review_count: 1,
            ..WorkflowMetrics::default()
        });
        assert!(review_bottleneck.contains("Board State: review-backlog-gated"));
        assert!(
            review_bottleneck.contains("Implementation Work: review backlog is the bottleneck")
        );

        let runnable = format_metrics(&WorkflowMetrics {
            board_state: WorkflowBoardState::RunnableBoard,
            runnable_count: 1,
            implementation_runnable_count: 1,
            actionable_review_count: 1,
            ..WorkflowMetrics::default()
        });
        assert!(runnable.contains("Board State: runnable-board"));
        assert!(runnable.contains("Implementation Work: executable implementation work available"));
    }

    #[test]
    fn compute_metrics_distinguishes_blocked_only_board() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "737-blocked.md",
            "id: 737\ntitle: Blocked only\nstatus: blocked\npriority: high\nblocked_on: missing unblock task\n",
        );

        let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

        assert_eq!(metrics.board_state, WorkflowBoardState::BlockedOnlyBoard);
        assert_eq!(metrics.runnable_count, 0);
        assert_eq!(metrics.blocked_count, 1);
        assert!(format_metrics(&metrics).contains("Board State: blocked-only-board"));
    }

    #[test]
    fn compute_metrics_distinguishes_review_backlog_gated_board() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "041-review.md",
            "id: 41\ntitle: Needs review\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
        );

        let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

        assert_eq!(metrics.board_state, WorkflowBoardState::ReviewBacklogGated);
        assert_eq!(metrics.runnable_count, 0);
        assert_eq!(metrics.actionable_review_count, 1);
    }

    #[test]
    fn compute_metrics_prefers_runnable_state_for_mixed_blocked_and_runnable_board() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "001-runnable.md",
            "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
        );
        write_board_task(
            tmp.path(),
            "002-blocked.md",
            "id: 2\ntitle: Blocked\nstatus: blocked\npriority: high\nblocked_on: external dependency\n",
        );

        let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

        assert_eq!(metrics.board_state, WorkflowBoardState::RunnableBoard);
        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.blocked_count, 1);
    }

    #[test]
    fn compute_metrics_excludes_blocked_review_from_actionable_review_count() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "001-actionable-review.md",
            "id: 1\ntitle: Actionable review\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
        );
        write_board_task(
            tmp.path(),
            "002-blocked-review.md",
            "id: 2\ntitle: Blocked review\nstatus: review\npriority: high\nclaimed_by: eng-1\nblocked_on: external reviewer\n",
        );

        let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

        assert_eq!(metrics.in_review_count, 2);
        assert_eq!(metrics.actionable_review_count, 1);
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
        assert!(formatted.contains("Blocked Dispatch: -"));
        assert_eq!(metrics.runnable_count, 1);
    }

    #[test]
    fn workflow_metrics_explains_todo_dependency_blocker() {
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
            "001-parent.md",
            "id: 1\ntitle: Parent\nstatus: blocked\npriority: high\n",
        );
        write_board_task(
            tmp.path(),
            "002-child.md",
            "id: 2\ntitle: Child\nstatus: todo\npriority: high\ndepends_on:\n  - 1\n",
        );

        let (formatted, metrics) =
            workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

        assert_eq!(metrics.runnable_count, 0);
        assert_eq!(
            metrics.blocked_dispatch_reasons,
            vec!["#2 Child: unmet dependency #1 (blocked)".to_string()]
        );
        assert!(formatted.contains("Blocked Dispatch: #2 Child: unmet dependency #1 (blocked)"));
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
    fn workflow_metrics_section_repairs_legacy_telemetry_db_before_querying() {
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

        let legacy = create_legacy_telemetry_db(tmp.path());
        let completed = TeamEvent::task_completed("eng-1", Some("1"));
        let merged = TeamEvent::task_auto_merged_with_mode(
            "eng-1",
            "1",
            0.9,
            2,
            30,
            Some(crate::team::merge::MergeMode::DirectRoot),
        );
        for event in [completed, merged] {
            legacy
                .execute(
                    "INSERT INTO events (timestamp, event_type, role, task_id, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        event.ts as i64,
                        event.event,
                        event.role,
                        event.task,
                        serde_json::to_string(&event).unwrap()
                    ],
                )
                .unwrap();
        }
        drop(legacy);

        let (formatted, metrics) =
            workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

        assert!(formatted.contains("Workflow Metrics"));
        assert!(formatted.contains("Auto-merge Rate: 100%"));
        assert_eq!(metrics.auto_merge_count, 1);

        let conn = crate::team::telemetry_db::open(tmp.path()).unwrap();
        let repairs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'telemetry_schema_repaired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repairs, 1);
    }

    #[test]
    fn build_team_status_json_report_serializes_machine_readable_json() {
        let report = build_team_status_json_report(TeamStatusJsonReportInput {
            team: "test".to_string(),
            session: "batty-test".to_string(),
            session_running: true,
            paused: false,
            main_smoke: None,
            watchdog: WatchdogStatus {
                state: "running".to_string(),
                restart_count: 2,
                current_backoff_secs: None,
                last_exit_category: Some("unknown".to_string()),
                last_exit_reason: Some("daemon exited with status 101".to_string()),
                ..WatchdogStatus::default()
            },
            workflow_metrics: Some(WorkflowMetrics {
                runnable_count: 1,
                ..WorkflowMetrics::default()
            }),
            publish_handoff: Some(crate::release::ReleasePublishHandoff {
                generated_at: "2026-04-24T00:00:00Z".to_string(),
                path: ".batty/reports/release/publish-handoff.json".to_string(),
                markdown_path: ".batty/releases/publish-handoff.md".to_string(),
                status: "blocked".to_string(),
                package_name: Some("batty".to_string()),
                version: Some("0.10.0".to_string()),
                tag: Some("v0.10.0".to_string()),
                git_ref: Some("abc123".to_string()),
                branch: Some("main".to_string()),
                release_notes_path: Some(".batty/releases/v0.10.0.md".to_string()),
                changelog_path: "CHANGELOG.md".to_string(),
                release_record_success: true,
                release_record_reason: "created annotated tag".to_string(),
                verification: crate::release::ReleasePublishVerificationEvidence {
                    command: Some("cargo test".to_string()),
                    summary: Some("cargo test passed".to_string()),
                    passed: true,
                },
                manual_publish_commands: vec![
                    "git push origin main".to_string(),
                    "git push origin v0.10.0".to_string(),
                    "cargo publish --package batty".to_string(),
                ],
                blocked_reasons: vec!["missing_publish_credentials: token missing".to_string()],
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
        assert_eq!(
            json["publish_handoff"]["path"],
            ".batty/reports/release/publish-handoff.json"
        );
        assert_eq!(json["publish_handoff"]["status"], "blocked");
        assert!(json["members"].is_array());
        assert_eq!(json["engineer_profiles"][0]["role"], "eng-1");
    }

    #[test]
    fn format_publish_handoff_summary_reports_path_and_blocked_state() {
        let handoff = crate::release::ReleasePublishHandoff {
            generated_at: "2026-04-24T00:00:00Z".to_string(),
            path: ".batty/reports/release/publish-handoff.json".to_string(),
            markdown_path: ".batty/releases/publish-handoff.md".to_string(),
            status: "blocked".to_string(),
            package_name: Some("batty".to_string()),
            version: Some("0.10.0".to_string()),
            tag: Some("v0.10.0".to_string()),
            git_ref: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            release_notes_path: Some(".batty/releases/v0.10.0.md".to_string()),
            changelog_path: "CHANGELOG.md".to_string(),
            release_record_success: false,
            release_record_reason: "dirty_main".to_string(),
            verification: crate::release::ReleasePublishVerificationEvidence {
                command: Some("cargo test".to_string()),
                summary: Some("cargo test passed".to_string()),
                passed: false,
            },
            manual_publish_commands: vec!["git push origin main".to_string()],
            blocked_reasons: vec!["dirty_main: main worktree has uncommitted changes".to_string()],
        };

        let rendered = format_publish_handoff_summary(&handoff);

        assert!(rendered.contains(".batty/reports/release/publish-handoff.json"));
        assert!(rendered.contains("(blocked)"));
        assert!(rendered.contains("dirty_main"));
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
        let mut pressure = SupervisoryPressureSnapshot::default();
        pressure.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::TriageBacklog,
            2,
        );
        pressure.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
            1,
        );
        assert_eq!(
            merge_status_signal(
                Some("nudged".to_string()),
                None,
                None,
                Some("manager (manager) stalled after 5m: no actionable progress".to_string()),
                &pressure,
                0,
            ),
            Some("nudged, pressure 2: review backlog (1)".to_string())
        );
    }

    #[test]
    fn merge_status_signal_returns_none_when_no_signals_exist() {
        assert_eq!(
            merge_status_signal(
                None,
                None,
                None,
                None,
                &SupervisoryPressureSnapshot::default(),
                0,
            ),
            None
        );
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
    fn format_review_blocks_surfaces_dirty_main_next_action() {
        let formatted = format_review_blocks(&[StatusTaskEntry {
            id: 42,
            title: "Review task".to_string(),
            status: "review".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".to_string()),
            review_owner: Some("manager".to_string()),
            blocked_on: Some(
                "Dirty source paths: src/lib.rs. Next action: commit, stash, or clean these root worktree changes before retrying the review merge."
                    .to_string(),
            ),
            branch: Some("eng-1/42".to_string()),
            worktree_path: None,
            commit: None,
            branch_mismatch: None,
            next_action: None,
            test_summary: None,
            latest_artifact: None,
            failed_test_state: None,
        }])
        .unwrap();

        assert!(formatted.contains("Review Blocks"));
        assert!(formatted.contains("#42 Review task"));
        assert!(formatted.contains("src/lib.rs"));
        assert!(formatted.contains("retry review"));
    }

    #[test]
    fn board_status_task_queues_marks_stale_review_when_engineer_moved_on() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-stale-review");
        let tasks_dir = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("042-review.md"),
            "---\nid: 42\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-1\nreview_owner: manager\nclass: standard\n---\n",
        )
        .unwrap();
        fs::write(
            tasks_dir.join("043-active.md"),
            "---\nid: 43\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/43\nclass: standard\n---\n",
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
        crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/43")
            .unwrap();
        std::fs::write(worktree_dir.join("active-43.txt"), "active branch\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "active-43.txt"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "active branch"]);

        let (_, review_queue) = board_status_task_queues(&repo).unwrap();

        assert_eq!(review_queue.len(), 1);
        assert_eq!(
            review_queue[0].next_action.as_deref(),
            Some("stale review -> merge: eng-1 already moved to task #43 on branch `eng-1/43`")
        );
    }

    #[test]
    fn board_status_task_queues_marks_branch_mismatch_review_as_rework() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("042-review.md"),
            "---\nid: 42\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-1\nreview_owner: manager\nbranch: eng-1/task-99\nclass: standard\n---\n",
        )
        .unwrap();

        let (_, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert_eq!(review_queue.len(), 1);
        assert_eq!(
            review_queue[0].next_action.as_deref(),
            Some(
                "stale review -> rework: branch `eng-1/task-99` references task(s) #99 but assigned task is #42"
            )
        );
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
    fn board_status_task_queues_repairs_hidden_in_progress_task_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-repair-hidden");
        let tasks_dir = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("041-hidden-active.md");
        fs::write(
            &task_path,
            "---\nid: 41\ntitle: Hidden active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nblocked: waiting on reviewer\nclass: standard\n---\n",
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(active_tasks.len(), 1);
        assert_eq!(active_tasks[0].id, 41);
        assert_eq!(active_tasks[0].status, "in-progress");
        assert_eq!(
            active_tasks[0].blocked_on.as_deref(),
            Some("waiting on reviewer")
        );

        let content = fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked: true"));
        assert!(content.contains("block_reason: waiting on reviewer"));
        assert!(content.contains("blocked_on: waiting on reviewer"));
    }

    #[test]
    fn board_status_task_queues_repairs_legacy_timestamp_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-repair-timestamp");
        let tasks_dir = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("623-stale-review.md");
        fs::write(
            &task_path,
            "---\nid: 623\ntitle: stale review\nstatus: review\npriority: high\nclaimed_by: eng-1\ncreated: 2026-04-10T16:31:02.743151-04:00\nupdated: 2026-04-10T19:26:40-0400\nreview_owner: manager\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

        assert!(active_tasks.is_empty());
        assert_eq!(review_queue.len(), 1);
        assert_eq!(review_queue[0].id, 623);
        let content = fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("updated: 2026-04-10T19:26:40-04:00"));
        assert!(content.ends_with("\n\nTask body.\n"));
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
            Some(
                "branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
            )
        );
    }

    #[test]
    fn board_status_task_queues_surfaces_dirty_branch_recovery_blocker() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "status-dirty-board");
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
        fs::write(worktree_dir.join("scratch.txt"), "dirty\n").unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(
            active_tasks[0].branch_mismatch.as_deref(),
            Some(
                "branch recovery blocked (#41 on eng-main/eng-1; expected eng-1/41; dirty worktree)"
            )
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
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\nartifacts:\n  - artifacts/retry-41.log\noutcome: verification_retry_required\ntest_results:\n  framework: cargo\n  total: 3\n  passed: 2\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n      message: assertion failed\n      location: src/parser.rs:12:5\n---\n",
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(active_tasks.len(), 1);
        assert_eq!(
            active_tasks[0].test_summary.as_deref(),
            Some("1 tests failed: parser::it_works (assertion failed at src/parser.rs:12:5)")
        );
        assert_eq!(
            active_tasks[0].latest_artifact.as_deref(),
            Some("artifacts/retry-41.log")
        );
        assert_eq!(
            active_tasks[0].failed_test_state.as_deref(),
            Some("dispatchable rework")
        );
        assert_eq!(
            format_failed_test_task(&active_tasks[0]).as_deref(),
            Some(
                "#41 Active task (owner: eng-1; artifact: artifacts/retry-41.log; dispatchable rework): 1 tests failed: parser::it_works (assertion failed at src/parser.rs:12:5)"
            )
        );
    }

    #[test]
    fn board_status_task_queues_marks_active_owner_retry_owner_locked() {
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
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\nartifacts:\n  - artifacts/retry-41.log\noutcome: verification_retry_required\ntest_results:\n  framework: cargo\n  total: 3\n  passed: 2\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n---\n",
        )
        .unwrap();
        let daemon_state = serde_json::json!({
            "states": {"eng-1": "working"},
            "active_tasks": {"eng-1": 41}
        });
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            daemon_state_path(tmp.path()),
            serde_json::to_vec_pretty(&daemon_state).unwrap(),
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(
            active_tasks[0].failed_test_state.as_deref(),
            Some("owner-locked")
        );
        assert_eq!(
            format_failed_test_task(&active_tasks[0]).as_deref(),
            Some(
                "#41 Active task (owner: eng-1; artifact: artifacts/retry-41.log; owner-locked): 1 tests failed: parser::it_works"
            )
        );
    }

    #[test]
    fn board_status_task_queues_surfaces_retry_artifact_without_test_results() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("728-active.md"),
            "---\nid: 728\ntitle: Verification retry\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\nartifacts:\n  - artifacts/retry-728.log\noutcome: verification_retry_required\n---\n",
        )
        .unwrap();
        let daemon_state = serde_json::json!({
            "states": {"eng-1": "working", "eng-2": "idle"},
            "active_tasks": {"eng-1": 777}
        });
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            daemon_state_path(tmp.path()),
            serde_json::to_vec_pretty(&daemon_state).unwrap(),
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(
            active_tasks[0].test_summary.as_deref(),
            Some("verification retry required")
        );
        assert_eq!(
            active_tasks[0].latest_artifact.as_deref(),
            Some("artifacts/retry-728.log")
        );
        assert_eq!(
            active_tasks[0].failed_test_state.as_deref(),
            Some("dispatchable rework")
        );
        assert_eq!(
            format_failed_test_task(&active_tasks[0]).as_deref(),
            Some(
                "#728 Verification retry (owner: eng-1; artifact: artifacts/retry-728.log; dispatchable rework): verification retry required"
            )
        );
    }

    #[test]
    fn board_status_task_queues_labels_failed_tests_owner_locked_without_retry_artifact() {
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
            "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\ntest_results:\n  framework: cargo\n  total: 1\n  passed: 0\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n---\n",
        )
        .unwrap();

        let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert!(review_queue.is_empty());
        assert_eq!(
            active_tasks[0].failed_test_state.as_deref(),
            Some("owner-locked")
        );
        assert_eq!(
            format_failed_test_task(&active_tasks[0]).as_deref(),
            Some(
                "#41 Active task (owner: eng-1; artifact: none; owner-locked): 1 tests failed: parser::it_works"
            )
        );
    }

    #[test]
    fn session_failed_tests_formatter_includes_owner_artifact_and_state() {
        let task = StatusTaskEntry {
            id: 728,
            title: "Verification retry".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".to_string()),
            review_owner: None,
            blocked_on: None,
            branch: Some("eng-1/728".to_string()),
            worktree_path: None,
            commit: None,
            branch_mismatch: None,
            next_action: None,
            test_summary: Some("cargo test failed".to_string()),
            latest_artifact: Some("artifacts/728.log".to_string()),
            failed_test_state: Some("dispatchable rework".to_string()),
        };

        assert_eq!(
            format_failed_test_task(&task).as_deref(),
            Some(
                "#728 Verification retry (owner: eng-1; artifact: artifacts/728.log; dispatchable rework): cargo test failed"
            )
        );
    }

    #[test]
    fn board_status_task_queues_surfaces_failed_github_feedback() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("042-review.md"),
            "---\nid: 42\ntitle: Review task\nstatus: review\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/42\ncommit: abcdef1\nclass: standard\n---\n",
        )
        .unwrap();
        crate::team::github_feedback::write_github_feedback_record(
            tmp.path(),
            &crate::team::github_feedback::GithubVerificationRecord {
                task_id: 42,
                branch: Some("eng-1/42".to_string()),
                commit: Some("abcdef1".to_string()),
                check_name: "ci/test".to_string(),
                status: "failure".to_string(),
                next_action: Some("fix failing CI".to_string()),
                details: Some("unit test failed".to_string()),
                ts: Some(1),
            },
        )
        .unwrap();

        let (_, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert_eq!(review_queue.len(), 1);
        assert_eq!(
            review_queue[0].blocked_on.as_deref(),
            Some("GitHub check failed: ci/test on eng-1/42@abcdef1 (unit test failed)")
        );
        assert_eq!(
            review_queue[0].next_action.as_deref(),
            Some("fix failing CI")
        );
        assert_eq!(
            review_queue[0].test_summary.as_deref(),
            Some("GitHub check failed: ci/test on eng-1/42@abcdef1")
        );
    }

    #[test]
    fn board_status_task_queues_clears_github_failure_after_passing_record() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("042-review.md"),
            "---\nid: 42\ntitle: Review task\nstatus: review\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/42\ncommit: abcdef1\nnext_action: review now\nclass: standard\n---\n",
        )
        .unwrap();
        for status in ["failure", "success"] {
            crate::team::github_feedback::write_github_feedback_record(
                tmp.path(),
                &crate::team::github_feedback::GithubVerificationRecord {
                    task_id: 42,
                    branch: Some("eng-1/42".to_string()),
                    commit: Some("abcdef1".to_string()),
                    check_name: "ci/test".to_string(),
                    status: status.to_string(),
                    next_action: Some("fix failing CI".to_string()),
                    details: None,
                    ts: Some(1),
                },
            )
            .unwrap();
        }

        let (_, review_queue) = board_status_task_queues(tmp.path()).unwrap();

        assert_eq!(review_queue.len(), 1);
        assert!(review_queue[0].blocked_on.is_none());
        assert_eq!(review_queue[0].next_action.as_deref(), Some("review now"));
        assert_eq!(
            review_queue[0].test_summary.as_deref(),
            Some("GitHub check passed: ci/test on eng-1/42@abcdef1")
        );
    }

    #[test]
    fn build_team_status_json_report_includes_health_and_queues() {
        let report = build_team_status_json_report(TeamStatusJsonReportInput {
            team: "test".to_string(),
            session: "batty-test".to_string(),
            session_running: true,
            paused: true,
            main_smoke: None,
            watchdog: WatchdogStatus {
                state: "restarting".to_string(),
                restart_count: 1,
                current_backoff_secs: Some(4),
                last_exit_category: Some("unknown".to_string()),
                last_exit_reason: Some("daemon exited with status 101".to_string()),
                ..WatchdogStatus::default()
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
            publish_handoff: None,
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
                latest_artifact: Some("artifacts/failed-tests.log".to_string()),
                failed_test_state: Some("dispatchable rework".to_string()),
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
                latest_artifact: None,
                failed_test_state: None,
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
                        proactive_handoff_count: 0,
                        delivery_failure_count: 0,
                        supervisory_digest_count: 0,
                        dispatch_fallback_count: 0,
                        dispatch_fallback_reason: None,
                        stale_active_cleared_count: 0,
                        stale_active_summary: None,
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
        fs::write(
            watchdog_pid_path(tmp.path()),
            std::process::id().to_string(),
        )
        .unwrap();
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
    fn load_watchdog_status_reports_offline_for_stale_dead_pids() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let dead_watchdog_pid = exited_test_pid();
        let dead_daemon_pid = exited_test_pid();
        fs::write(watchdog_pid_path(tmp.path()), dead_watchdog_pid.to_string()).unwrap();
        fs::write(
            daemon_child_pid_path(tmp.path()),
            dead_daemon_pid.to_string(),
        )
        .unwrap();
        fs::write(
            watchdog_state_path(tmp.path()),
            serde_json::json!({
                "restart_count": 1,
                "child_pid": dead_daemon_pid
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            daemon_state_path(tmp.path()),
            serde_json::json!({"saved_at": 1, "clean_shutdown": false}).to_string(),
        )
        .unwrap();
        fs::write(daemon_log_path(tmp.path()), "daemon stopped\n").unwrap();

        let watchdog = load_watchdog_status(tmp.path(), true);

        assert_eq!(watchdog.state, "offline");
        assert_eq!(watchdog.watchdog_pid, Some(dead_watchdog_pid));
        assert!(!watchdog.watchdog_pid_live);
        assert_eq!(watchdog.daemon_pid, Some(dead_daemon_pid));
        assert!(!watchdog.daemon_pid_live);
        assert!(watchdog.daemon_state_updated_at.is_some());
        assert!(watchdog.daemon_state_age_secs.is_some());
        assert!(watchdog.daemon_log_updated_at.is_some());
        assert!(watchdog.daemon_log_age_secs.is_some());

        let summary = format_watchdog_summary(&watchdog);
        assert!(summary.contains("offline"));
        assert!(summary.contains(&format!("watchdog-pid={dead_watchdog_pid}:dead")));
        assert!(summary.contains(&format!("daemon-pid={dead_daemon_pid}:dead")));
        assert!(summary.contains("state-age="));
        assert!(summary.contains("log-age="));
    }

    fn exited_test_pid() -> u32 {
        #[cfg(unix)]
        {
            let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        }
        #[cfg(not(unix))]
        {
            999_998
        }
    }

    #[cfg(unix)]
    #[test]
    fn load_watchdog_status_reports_running_for_live_watchdog_and_daemon_pids() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let mut watchdog_process = Command::new("sleep").arg("5").spawn().unwrap();
        let mut daemon_process = Command::new("sleep").arg("5").spawn().unwrap();
        fs::write(
            watchdog_pid_path(tmp.path()),
            watchdog_process.id().to_string(),
        )
        .unwrap();
        fs::write(
            daemon_child_pid_path(tmp.path()),
            daemon_process.id().to_string(),
        )
        .unwrap();
        fs::write(
            watchdog_state_path(tmp.path()),
            serde_json::json!({
                "restart_count": 0,
                "child_pid": daemon_process.id()
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            daemon_state_path(tmp.path()),
            serde_json::json!({"saved_at": crate::team::now_unix(), "clean_shutdown": false})
                .to_string(),
        )
        .unwrap();
        fs::write(daemon_log_path(tmp.path()), "daemon alive\n").unwrap();

        let watchdog = load_watchdog_status(tmp.path(), true);

        assert_eq!(watchdog.state, "running");
        assert_eq!(watchdog.watchdog_pid, Some(watchdog_process.id()));
        assert!(watchdog.watchdog_pid_live);
        assert_eq!(watchdog.daemon_pid, Some(daemon_process.id()));
        assert!(watchdog.daemon_pid_live);
        assert!(format_watchdog_summary(&watchdog).contains("daemon-pid="));

        let _ = watchdog_process.kill();
        let _ = daemon_process.kill();
        let _ = watchdog_process.wait();
        let _ = daemon_process.wait();
    }

    #[test]
    fn format_watchdog_summary_includes_backoff_and_reason() {
        let summary = format_watchdog_summary(&WatchdogStatus {
            state: "restarting".to_string(),
            restart_count: 2,
            current_backoff_secs: Some(4),
            last_exit_category: Some("unknown".to_string()),
            last_exit_reason: Some("daemon exited with status 101".to_string()),
            ..WatchdogStatus::default()
        });

        assert!(summary.contains("restarting"));
        assert!(summary.contains("r2"));
        assert!(summary.contains("backoff=4s"));
        assert!(summary.contains("daemon exited with status 101"));
    }

    #[test]
    fn format_watchdog_summary_surfaces_orphan_codex_exec_processes() {
        let summary = format_watchdog_summary(&WatchdogStatus {
            state: "offline".to_string(),
            restart_count: 1,
            orphan_codex_execs: vec![OrphanProcessStatus {
                pid: 42,
                ppid: 1,
                command: "codex exec --json -".to_string(),
            }],
            ..WatchdogStatus::default()
        });

        assert!(summary.contains("orphan-codex-exec=1 [42<-1]"));
    }

    #[test]
    fn format_main_smoke_summary_includes_commit_and_suspects() {
        let summary = format_main_smoke_summary(&MainSmokeState {
            broken: true,
            pause_dispatch: true,
            last_run_at: 1,
            last_success_commit: Some("aaa1111".to_string()),
            broken_commit: Some("bbb2222".to_string()),
            suspects: vec![
                "bbb2222 break main".to_string(),
                "ccc3333 prior".to_string(),
            ],
            summary: Some("could not compile `batty-cli`".to_string()),
        });
        assert!(summary.contains("BROKEN by bbb2222"));
        assert!(summary.contains("bbb2222 break main"));
        assert!(summary.contains("could not compile `batty-cli`"));
    }

    #[test]
    fn build_team_status_json_report_includes_main_smoke_state() {
        let report = build_team_status_json_report(TeamStatusJsonReportInput {
            team: "test".to_string(),
            session: "batty-test".to_string(),
            session_running: true,
            paused: false,
            main_smoke: Some(MainSmokeState {
                broken: true,
                pause_dispatch: true,
                last_run_at: 1,
                last_success_commit: None,
                broken_commit: Some("abc1234".to_string()),
                suspects: vec!["abc1234 break main".to_string()],
                summary: Some("could not compile `batty-cli`".to_string()),
            }),
            watchdog: WatchdogStatus {
                state: "running".to_string(),
                restart_count: 0,
                current_backoff_secs: None,
                last_exit_category: None,
                last_exit_reason: None,
                ..WatchdogStatus::default()
            },
            workflow_metrics: None,
            publish_handoff: None,
            active_tasks: Vec::new(),
            review_queue: Vec::new(),
            optional_subsystems: None,
            engineer_profiles: None,
            members: Vec::new(),
        });
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["main_smoke"]["broken"].as_bool(), Some(true));
        assert_eq!(
            json["main_smoke"]["broken_commit"].as_str(),
            Some("abc1234")
        );
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
            proactive_handoff_count: 1,
            delivery_failure_count: 3,
            supervisory_digest_count: 1,
            dispatch_fallback_count: 0,
            dispatch_fallback_reason: None,
            stale_active_cleared_count: 1,
            stale_active_summary: Some("cleared stale active #42 (done)".to_string()),
            task_elapsed_secs: Some(750),
            stall_reason: None,
            stall_summary: None,
            backend_health: crate::agent::BackendHealth::default(),
        });

        assert_eq!(summary, "r2 c1 ph1 d3 sd1 sa1 t12m");
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

        assert_eq!(summary, "stall:manager:working-timeout");
    }

    #[test]
    fn format_agent_health_summary_uses_non_engineer_slo_tokens() {
        let cases = [
            (
                "supervisory_stalled_manager_review_backlog",
                RoleType::Manager,
                "stall:manager:review-wait-timeout",
            ),
            (
                "supervisory_stalled_manager_dispatch_gap",
                RoleType::Manager,
                "stall:manager:dispatch-gap-pressure",
            ),
            (
                "supervisory_stalled_architect_direct_report_packets",
                RoleType::Architect,
                "stall:architect:inbox-backlog-pressure",
            ),
            (
                "supervisory_stalled_architect_inbox_batching",
                RoleType::Architect,
                "stall:architect:inbox-backlog-pressure",
            ),
            (
                "supervisory_stalled_manager_working_timeout",
                RoleType::Manager,
                "stall:manager:working-timeout",
            ),
        ];

        for (reason, role_type, expected) in cases {
            let summary = format_agent_health_summary_for_role(
                &AgentHealthSummary {
                    stall_reason: Some(reason.to_string()),
                    stall_summary: Some("stalled".to_string()),
                    ..AgentHealthSummary::default()
                },
                Some(role_type),
            );
            assert_eq!(summary, expected);
        }
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

        assert_eq!(summary, "stall:manager:working-timeout");
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

        let mut proactive = TeamEvent::agent_restarted("eng-1", "42", "context_pressure", 1);
        proactive.ts = now_unix().saturating_sub(575);
        sink.emit(proactive).unwrap();

        let mut delivery_failed =
            TeamEvent::delivery_failed("eng-1", "manager", "message delivery failed after retries");
        delivery_failed.ts = now_unix().saturating_sub(570);
        sink.emit(delivery_failed).unwrap();

        let mut digest_emitted = TeamEvent::supervisory_digest_emitted("eng-1", 3, 1);
        digest_emitted.ts = now_unix().saturating_sub(565);
        sink.emit(digest_emitted).unwrap();

        let mut stale_active =
            TeamEvent::state_reconciliation(Some("eng-1"), Some("42"), "clear_done");
        stale_active.ts = now_unix().saturating_sub(563);
        sink.emit(stale_active).unwrap();

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
        assert_eq!(eng_1.proactive_handoff_count, 1);
        assert_eq!(eng_1.delivery_failure_count, 1);
        assert_eq!(eng_1.supervisory_digest_count, 1);
        assert_eq!(eng_1.stale_active_cleared_count, 1);
        assert_eq!(
            eng_1.stale_active_summary.as_deref(),
            Some("cleared stale active #42 (done)")
        );
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
    fn agent_health_by_member_clears_stall_from_previous_daemon_session() {
        // Regression: stall_detected events from prior daemon sessions
        // used to leak into the current status display, producing signals
        // like "manager (manager) stalled after 2h" on a freshly-restarted
        // member. The fix: clear supervisory_stall state when a newer
        // daemon_started event is seen.
        let tmp = tempfile::tempdir().unwrap();
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();

        // Old daemon session emits a stall for the manager.
        let mut old_stall = TeamEvent::stall_detected_with_reason(
            "manager",
            None,
            7200,
            Some("supervisory_inbox_batching"),
        );
        old_stall.task = Some("supervisory::manager".to_string());
        old_stall.details = Some("manager (manager) stalled after 2h: inbox batching".to_string());
        sink.emit(old_stall).unwrap();

        // Daemon restarts — this should clear the prior stall state.
        sink.emit(TeamEvent::daemon_started()).unwrap();

        // After the restart, no new stall events have been emitted for the
        // manager. The status display must NOT show the stale 2h stall.
        let health = agent_health_by_member(tmp.path(), &[manager("manager"), engineer("eng-1")]);
        let manager_health = health.get("manager").unwrap();
        assert!(
            !manager_health.has_supervisory_warning(),
            "manager should not carry a supervisory stall warning from a prior daemon session; got reason={:?} summary={:?}",
            manager_health.stall_reason,
            manager_health.stall_summary,
        );
    }

    #[test]
    fn agent_health_by_member_keeps_stall_from_current_daemon_session() {
        // Companion to the clears-previous-session test: a stall event that
        // happens AFTER the latest daemon_started should be preserved.
        let tmp = tempfile::tempdir().unwrap();
        let events_path = team_events_path(tmp.path());
        let mut sink = EventSink::new(&events_path).unwrap();

        sink.emit(TeamEvent::daemon_started()).unwrap();
        let mut stall = TeamEvent::stall_detected_with_reason(
            "manager",
            None,
            300,
            Some("supervisory_inbox_batching"),
        );
        stall.task = Some("supervisory::manager".to_string());
        stall.details = Some("manager (manager) stalled after 5m: inbox batching".to_string());
        sink.emit(stall).unwrap();

        let health = agent_health_by_member(tmp.path(), &[manager("manager"), engineer("eng-1")]);
        let manager_health = health.get("manager").unwrap();
        assert!(
            manager_health.has_supervisory_warning(),
            "manager should retain a stall warning from the current session",
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
            crate::team::events::TeamEvent::task_auto_merged_with_mode(
                "eng-1",
                "1",
                0.9,
                3,
                50,
                Some(crate::team::merge::MergeMode::DirectRoot),
            ),
            crate::team::events::TeamEvent::task_completed("eng-1", Some("2")),
            crate::team::events::TeamEvent::task_manual_merged_with_mode(
                "2",
                Some(crate::team::merge::MergeMode::IsolatedIntegration),
            ),
            crate::team::events::TeamEvent::task_merge_failed(
                "eng-1",
                "3",
                Some(crate::team::merge::MergeMode::IsolatedIntegration),
                "isolated merge path failed: integration checkout broke",
            ),
            crate::team::events::TeamEvent::task_reworked("eng-1", "3"),
        ];
        for event in &events {
            crate::team::telemetry_db::insert_event(&conn, event).unwrap();
        }

        let metrics =
            compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], Some(&conn), None).unwrap();

        assert_eq!(metrics.auto_merge_count, 1);
        assert_eq!(metrics.manual_merge_count, 1);
        assert_eq!(metrics.direct_root_merge_count, 1);
        assert_eq!(metrics.isolated_integration_merge_count, 1);
        assert_eq!(metrics.direct_root_failure_count, 0);
        assert_eq!(metrics.isolated_integration_failure_count, 1);
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
        sink.emit(TeamEvent::task_auto_merged_with_mode(
            "eng-1",
            "1",
            0.9,
            3,
            50,
            Some(crate::team::merge::MergeMode::DirectRoot),
        ))
        .unwrap();
        sink.emit(TeamEvent::task_merge_failed(
            "eng-1",
            "2",
            Some(crate::team::merge::MergeMode::IsolatedIntegration),
            "isolated merge path failed: integration checkout broke",
        ))
        .unwrap();

        let metrics =
            compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], None, Some(&events_path))
                .unwrap();

        assert_eq!(metrics.auto_merge_count, 1);
        assert_eq!(metrics.direct_root_merge_count, 1);
        assert_eq!(metrics.isolated_integration_failure_count, 1);
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
        assert_eq!(metrics.direct_root_merge_count, 0);
        assert_eq!(metrics.isolated_integration_merge_count, 0);
        assert_eq!(metrics.direct_root_failure_count, 0);
        assert_eq!(metrics.isolated_integration_failure_count, 0);
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

    /// Regression for #618: when a supervisor has actionable backlog
    /// (`needs review` / `needs triage`), generic stall text must be
    /// suppressed so operators see the actionable reason first.
    #[test]
    fn merge_status_signal_suppresses_generic_stall_when_actionable_backlog_present() {
        let mut review_pressure = SupervisoryPressureSnapshot::default();
        review_pressure.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
            2,
        );
        let result = merge_status_signal(
            None,
            None,
            None,
            Some("manager stall after 32m".to_string()),
            &review_pressure,
            0,
        );
        assert_eq!(
            result,
            Some("pressure 1: review backlog (2)".to_string()),
            "generic stall text should be suppressed when review backlog is non-zero"
        );

        let mut triage_pressure = SupervisoryPressureSnapshot::default();
        triage_pressure.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::TriageBacklog,
            3,
        );
        let result = merge_status_signal(
            None,
            None,
            None,
            Some("architect stall after 15m".to_string()),
            &triage_pressure,
            0,
        );
        assert_eq!(
            result,
            Some("pressure 1: direct-report packets (3)".to_string()),
            "generic stall text should be suppressed when triage backlog is non-zero"
        );

        let result = merge_status_signal(
            None,
            None,
            None,
            Some("manager stall after 42m".to_string()),
            &SupervisoryPressureSnapshot::default(),
            0,
        );
        assert_eq!(
            result,
            Some("manager stall after 42m".to_string()),
            "generic stall text should remain when no actionable backlog is present"
        );
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
