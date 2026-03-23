//! Pure event-log analysis and markdown report generation for retrospectives.
//!
//! Prefers SQLite telemetry DB when available, falls back to JSONL parsing.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::events::{TeamEvent, read_events};
use super::telemetry_db;
use crate::task;

#[derive(Debug, Clone, PartialEq)]
pub struct RunStats {
    pub run_start: u64,
    pub run_end: u64,
    pub total_duration_secs: u64,
    pub task_stats: Vec<TaskStats>,
    pub average_cycle_time_secs: Option<u64>,
    pub fastest_task_id: Option<String>,
    pub fastest_cycle_time_secs: Option<u64>,
    pub longest_task_id: Option<String>,
    pub longest_cycle_time_secs: Option<u64>,
    pub idle_time_pct: f64,
    pub escalation_count: u32,
    pub message_count: u32,
    // Review pipeline metrics
    pub auto_merge_count: u32,
    pub manual_merge_count: u32,
    pub rework_count: u32,
    pub review_nudge_count: u32,
    pub review_escalation_count: u32,
    /// Average time (seconds) tasks spent in review before merge.
    pub avg_review_stall_secs: Option<u64>,
    /// Longest review stall and the associated task.
    pub max_review_stall_secs: Option<u64>,
    pub max_review_stall_task: Option<String>,
    /// Per-task rework cycle counts (task_id → rework count).
    pub task_rework_counts: Vec<(String, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStats {
    pub task_id: String,
    pub assigned_to: String,
    pub assigned_at: u64,
    pub completed_at: Option<u64>,
    pub cycle_time_secs: Option<u64>,
    pub retry_count: u32,
    pub was_escalated: bool,
}

#[derive(Debug, Clone)]
struct TaskAccumulator {
    task_id: String,
    assigned_to: String,
    assigned_at: u64,
    completed_at: Option<u64>,
    cycle_time_secs: Option<u64>,
    retry_count: u32,
    was_escalated: bool,
}

impl TaskAccumulator {
    fn new(task_id: String, assigned_to: String, assigned_at: u64, retry_count: u32) -> Self {
        Self {
            task_id,
            assigned_to,
            assigned_at,
            completed_at: None,
            cycle_time_secs: None,
            retry_count,
            was_escalated: false,
        }
    }

    fn into_stats(self) -> TaskStats {
        TaskStats {
            task_id: self.task_id,
            assigned_to: self.assigned_to,
            assigned_at: self.assigned_at,
            completed_at: self.completed_at,
            cycle_time_secs: self.cycle_time_secs,
            retry_count: self.retry_count,
            was_escalated: self.was_escalated,
        }
    }
}

fn task_reference(task: &str) -> String {
    let line = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| task.trim());

    task_id_from_assignment_line(line).unwrap_or_else(|| line.to_string())
}

fn task_id_from_assignment_line(line: &str) -> Option<String> {
    let suffix = line.strip_prefix("Task #")?;
    let digits: String = suffix
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

type CycleTimeMetrics = (
    Option<u64>,
    Option<String>,
    Option<u64>,
    Option<String>,
    Option<u64>,
);

fn cycle_time_metrics(task_stats: &[TaskStats]) -> CycleTimeMetrics {
    let completed: Vec<(&TaskStats, u64)> = task_stats
        .iter()
        .filter_map(|task| task.cycle_time_secs.map(|cycle| (task, cycle)))
        .collect();
    if completed.is_empty() {
        return (None, None, None, None, None);
    }

    let total_cycle_secs: u64 = completed.iter().map(|(_, cycle)| *cycle).sum();
    let average_cycle_time_secs = Some(total_cycle_secs / completed.len() as u64);
    let (fastest_task, fastest_cycle_time_secs) = completed
        .iter()
        .min_by_key(|(_, cycle)| *cycle)
        .map(|(task, cycle)| (task.task_id.clone(), *cycle))
        .expect("completed is not empty");
    let (longest_task, longest_cycle_time_secs) = completed
        .iter()
        .max_by_key(|(_, cycle)| *cycle)
        .map(|(task, cycle)| (task.task_id.clone(), *cycle))
        .expect("completed is not empty");

    (
        average_cycle_time_secs,
        Some(fastest_task),
        Some(fastest_cycle_time_secs),
        Some(longest_task),
        Some(longest_cycle_time_secs),
    )
}

/// Analyze a single run (events between consecutive daemon_started events).
/// Returns the last run's stats.
pub fn analyze_events(events: &[TeamEvent]) -> Option<RunStats> {
    if events.is_empty() {
        return None;
    }

    let last_run_start = events
        .iter()
        .rposition(|event| event.event == "daemon_started")
        .unwrap_or(0);
    let run_events = &events[last_run_start..];
    if run_events.is_empty() {
        return None;
    }

    let run_start = run_events[0].ts;
    let run_end = run_events
        .iter()
        .rev()
        .find(|event| event.event == "daemon_stopped")
        .map(|event| event.ts)
        .unwrap_or_else(|| run_events.last().map(|event| event.ts).unwrap_or(run_start));

    let mut tasks: HashMap<String, TaskAccumulator> = HashMap::new();
    let mut active_task_by_role: HashMap<String, String> = HashMap::new();
    let mut idle_samples = Vec::new();
    let mut escalation_count = 0u32;
    let mut message_count = 0u32;
    let mut auto_merge_count = 0u32;
    let mut manual_merge_count = 0u32;
    let mut rework_count = 0u32;
    let mut review_nudge_count = 0u32;
    let mut review_escalation_count = 0u32;
    // Track per-task: completion timestamp (for stall calc) and rework counts.
    let mut task_completed_at: HashMap<String, u64> = HashMap::new();
    let mut review_stall_durations: Vec<(String, u64)> = Vec::new();
    let mut per_task_rework: HashMap<String, u32> = HashMap::new();

    for event in run_events {
        match event.event.as_str() {
            "task_assigned" => {
                let Some(role) = event.role.as_deref() else {
                    continue;
                };
                let Some(task) = event.task.as_deref() else {
                    continue;
                };
                let task_id = task_reference(task);

                let entry = tasks.entry(task_id.clone()).or_insert_with(|| {
                    TaskAccumulator::new(task_id.clone(), role.to_string(), event.ts, 0)
                });
                entry.retry_count += 1;
                entry.assigned_to = role.to_string();
                active_task_by_role.insert(role.to_string(), task_id);
            }
            // The completion event does not include a task id, so completion is
            // attributed to the role's currently active assignment in this run.
            "task_completed" => {
                let Some(role) = event.role.as_deref() else {
                    continue;
                };
                let Some(task_id) = active_task_by_role.remove(role) else {
                    continue;
                };
                let Some(task) = tasks.get_mut(&task_id) else {
                    continue;
                };
                if task.completed_at.is_none() {
                    task.completed_at = Some(event.ts);
                    task.cycle_time_secs = Some(event.ts.saturating_sub(task.assigned_at));
                }
                // Record completion time for review stall calculation.
                task_completed_at.insert(task_id, event.ts);
            }
            "task_escalated" => {
                escalation_count += 1;
                let Some(task_id) = event.task.as_deref() else {
                    continue;
                };
                let role = event.role.clone().unwrap_or_default();
                let entry = tasks.entry(task_id.to_string()).or_insert_with(|| {
                    TaskAccumulator::new(task_id.to_string(), role, event.ts, 0)
                });
                entry.was_escalated = true;
            }
            "message_routed" => {
                message_count += 1;
            }
            "task_auto_merged" => {
                auto_merge_count += 1;
                if let Some(task) = event.task.as_deref() {
                    let task_id = task_reference(task);
                    if let Some(completed_ts) = task_completed_at.get(&task_id) {
                        review_stall_durations
                            .push((task_id, event.ts.saturating_sub(*completed_ts)));
                    }
                }
            }
            "task_manual_merged" => {
                manual_merge_count += 1;
                if let Some(task) = event.task.as_deref() {
                    let task_id = task_reference(task);
                    if let Some(completed_ts) = task_completed_at.get(&task_id) {
                        review_stall_durations
                            .push((task_id, event.ts.saturating_sub(*completed_ts)));
                    }
                }
            }
            "task_reworked" => {
                rework_count += 1;
                if let Some(task) = event.task.as_deref() {
                    let task_id = task_reference(task);
                    *per_task_rework.entry(task_id).or_insert(0) += 1;
                }
            }
            "review_nudge_sent" => {
                review_nudge_count += 1;
            }
            "review_escalated" => {
                review_escalation_count += 1;
            }
            "load_snapshot" => {
                let Some(working_members) = event.working_members else {
                    continue;
                };
                let Some(total_members) = event.total_members else {
                    continue;
                };
                let idle_pct = if total_members == 0 {
                    1.0
                } else {
                    1.0 - (working_members as f64 / total_members as f64)
                };
                idle_samples.push(idle_pct);
            }
            _ => {}
        }
    }

    let mut task_stats: Vec<TaskStats> =
        tasks.into_values().map(|task| task.into_stats()).collect();
    task_stats.sort_by(|left, right| {
        left.assigned_at
            .cmp(&right.assigned_at)
            .then_with(|| left.task_id.cmp(&right.task_id))
    });

    let idle_time_pct = if idle_samples.is_empty() {
        0.0
    } else {
        idle_samples.iter().sum::<f64>() / idle_samples.len() as f64
    };
    let (
        average_cycle_time_secs,
        fastest_task_id,
        fastest_cycle_time_secs,
        longest_task_id,
        longest_cycle_time_secs,
    ) = cycle_time_metrics(&task_stats);

    // Compute review stall metrics.
    let (avg_review_stall_secs, max_review_stall_secs, max_review_stall_task) =
        if review_stall_durations.is_empty() {
            (None, None, None)
        } else {
            let total: u64 = review_stall_durations.iter().map(|(_, d)| *d).sum();
            let avg = total / review_stall_durations.len() as u64;
            let (max_task, max_dur) = review_stall_durations
                .iter()
                .max_by_key(|(_, d)| *d)
                .map(|(t, d)| (t.clone(), *d))
                .expect("non-empty");
            (Some(avg), Some(max_dur), Some(max_task))
        };

    // Collect per-task rework counts, sorted by count descending.
    let mut task_rework_counts: Vec<(String, u32)> = per_task_rework.into_iter().collect();
    task_rework_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Some(RunStats {
        run_start,
        run_end,
        total_duration_secs: run_end.saturating_sub(run_start),
        task_stats,
        average_cycle_time_secs,
        fastest_task_id,
        fastest_cycle_time_secs,
        longest_task_id,
        longest_cycle_time_secs,
        idle_time_pct,
        escalation_count,
        message_count,
        auto_merge_count,
        manual_merge_count,
        rework_count,
        review_nudge_count,
        review_escalation_count,
        avg_review_stall_secs,
        max_review_stall_secs,
        max_review_stall_task,
        task_rework_counts,
    })
}

/// Build a `RunStats` from the SQLite telemetry database.
///
/// Queries the `events`, `task_metrics`, `agent_metrics`, and `session_summary`
/// tables to produce the same report that `analyze_events` builds from JSONL.
pub fn analyze_from_db(conn: &Connection) -> Option<RunStats> {
    // Find the last session (last daemon_started event).
    let last_session_start: Option<i64> = conn
        .query_row(
            "SELECT MAX(timestamp) FROM events WHERE event_type = 'daemon_started'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let run_start = last_session_start? as u64;

    // Find run_end: daemon_stopped after last start, or latest event.
    let run_end: u64 = conn
        .query_row(
            "SELECT timestamp FROM events
             WHERE event_type = 'daemon_stopped' AND timestamp >= ?1
             ORDER BY timestamp DESC LIMIT 1",
            params![run_start as i64],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or_else(|_| {
            conn.query_row(
                "SELECT MAX(timestamp) FROM events WHERE timestamp >= ?1",
                params![run_start as i64],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap_or(None)
            .unwrap_or(run_start as i64)
        }) as u64;

    // --- Task stats from events table (same logic as analyze_events) ---
    // We still need to walk events for the current run to build per-task stats
    // because task_metrics doesn't track assignment order or role-based completion.
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, event_type, role, task_id, payload FROM events
             WHERE timestamp >= ?1 ORDER BY timestamp ASC",
        )
        .ok()?;

    let rows: Vec<(i64, String, Option<String>, Option<String>, String)> = stmt
        .query_map(params![run_start as i64], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        return None;
    }

    let mut tasks: HashMap<String, TaskAccumulator> = HashMap::new();
    let mut active_task_by_role: HashMap<String, String> = HashMap::new();
    let mut idle_samples = Vec::new();
    let mut escalation_count = 0u32;
    let mut message_count = 0u32;
    let mut auto_merge_count = 0u32;
    let mut manual_merge_count = 0u32;
    let mut rework_count = 0u32;
    let mut review_nudge_count = 0u32;
    let mut review_escalation_count = 0u32;
    let mut task_completed_at: HashMap<String, u64> = HashMap::new();
    let mut review_stall_durations: Vec<(String, u64)> = Vec::new();
    let mut per_task_rework: HashMap<String, u32> = HashMap::new();

    for (ts, event_type, role, task_id, payload) in &rows {
        let ts = *ts as u64;
        match event_type.as_str() {
            "task_assigned" => {
                let Some(role) = role.as_deref() else {
                    continue;
                };
                let Some(task) = task_id.as_deref() else {
                    continue;
                };
                let tid = task_reference(task);
                let entry = tasks
                    .entry(tid.clone())
                    .or_insert_with(|| TaskAccumulator::new(tid.clone(), role.to_string(), ts, 0));
                entry.retry_count += 1;
                entry.assigned_to = role.to_string();
                active_task_by_role.insert(role.to_string(), tid);
            }
            "task_completed" => {
                let Some(role) = role.as_deref() else {
                    continue;
                };
                let Some(tid) = active_task_by_role.remove(role) else {
                    continue;
                };
                let Some(task) = tasks.get_mut(&tid) else {
                    continue;
                };
                if task.completed_at.is_none() {
                    task.completed_at = Some(ts);
                    task.cycle_time_secs = Some(ts.saturating_sub(task.assigned_at));
                }
                task_completed_at.insert(tid, ts);
            }
            "task_escalated" => {
                escalation_count += 1;
                let Some(task) = task_id.as_deref() else {
                    continue;
                };
                let r = role.clone().unwrap_or_default();
                let entry = tasks
                    .entry(task.to_string())
                    .or_insert_with(|| TaskAccumulator::new(task.to_string(), r, ts, 0));
                entry.was_escalated = true;
            }
            "message_routed" => {
                message_count += 1;
            }
            "task_auto_merged" => {
                auto_merge_count += 1;
                if let Some(task) = task_id.as_deref() {
                    let tid = task_reference(task);
                    if let Some(completed_ts) = task_completed_at.get(&tid) {
                        review_stall_durations.push((tid, ts.saturating_sub(*completed_ts)));
                    }
                }
            }
            "task_manual_merged" => {
                manual_merge_count += 1;
                if let Some(task) = task_id.as_deref() {
                    let tid = task_reference(task);
                    if let Some(completed_ts) = task_completed_at.get(&tid) {
                        review_stall_durations.push((tid, ts.saturating_sub(*completed_ts)));
                    }
                }
            }
            "task_reworked" => {
                rework_count += 1;
                if let Some(task) = task_id.as_deref() {
                    let tid = task_reference(task);
                    *per_task_rework.entry(tid).or_insert(0) += 1;
                }
            }
            "review_nudge_sent" => {
                review_nudge_count += 1;
            }
            "review_escalated" => {
                review_escalation_count += 1;
            }
            "load_snapshot" => {
                // Parse working_members/total_members from payload JSON.
                if let Ok(evt) = serde_json::from_str::<TeamEvent>(payload) {
                    let Some(working_members) = evt.working_members else {
                        continue;
                    };
                    let Some(total_members) = evt.total_members else {
                        continue;
                    };
                    let idle_pct = if total_members == 0 {
                        1.0
                    } else {
                        1.0 - (working_members as f64 / total_members as f64)
                    };
                    idle_samples.push(idle_pct);
                }
            }
            _ => {}
        }
    }

    let mut task_stats: Vec<TaskStats> = tasks.into_values().map(|t| t.into_stats()).collect();
    task_stats.sort_by(|a, b| {
        a.assigned_at
            .cmp(&b.assigned_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });

    let idle_time_pct = if idle_samples.is_empty() {
        0.0
    } else {
        idle_samples.iter().sum::<f64>() / idle_samples.len() as f64
    };

    let (
        average_cycle_time_secs,
        fastest_task_id,
        fastest_cycle_time_secs,
        longest_task_id,
        longest_cycle_time_secs,
    ) = cycle_time_metrics(&task_stats);

    let (avg_review_stall_secs, max_review_stall_secs, max_review_stall_task) =
        if review_stall_durations.is_empty() {
            (None, None, None)
        } else {
            let total: u64 = review_stall_durations.iter().map(|(_, d)| *d).sum();
            let avg = total / review_stall_durations.len() as u64;
            let (max_task, max_dur) = review_stall_durations
                .iter()
                .max_by_key(|(_, d)| *d)
                .map(|(t, d)| (t.clone(), *d))
                .expect("non-empty");
            (Some(avg), Some(max_dur), Some(max_task))
        };

    let mut task_rework_counts: Vec<(String, u32)> = per_task_rework.into_iter().collect();
    task_rework_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Some(RunStats {
        run_start,
        run_end,
        total_duration_secs: run_end.saturating_sub(run_start),
        task_stats,
        average_cycle_time_secs,
        fastest_task_id,
        fastest_cycle_time_secs,
        longest_task_id,
        longest_cycle_time_secs,
        idle_time_pct,
        escalation_count,
        message_count,
        auto_merge_count,
        manual_merge_count,
        rework_count,
        review_nudge_count,
        review_escalation_count,
        avg_review_stall_secs,
        max_review_stall_secs,
        max_review_stall_task,
        task_rework_counts,
    })
}

/// Analyze the last run. Prefers telemetry DB when available, falls back to JSONL.
pub fn analyze_project(project_root: &Path) -> Result<Option<RunStats>> {
    let db_path = project_root.join(".batty").join("telemetry.db");
    if db_path.exists() {
        if let Ok(conn) = telemetry_db::open(project_root) {
            if let Some(stats) = analyze_from_db(&conn) {
                return Ok(Some(stats));
            }
        }
    }

    // Fallback to JSONL
    let events_path = project_root
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    analyze_event_log(&events_path)
}

/// Parse the events file and analyze.
pub fn analyze_event_log(path: &Path) -> Result<Option<RunStats>> {
    let events = read_events(path)?;
    Ok(analyze_events(&events))
}

pub fn should_generate_retro(
    project_root: &Path,
    retro_generated: bool,
    min_duration_secs: u64,
) -> Result<Option<RunStats>> {
    if retro_generated {
        return Ok(None);
    }

    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(None);
    }

    let tasks = task::load_tasks_from_dir(&tasks_dir)?;
    let active_tasks: Vec<&task::Task> = tasks
        .iter()
        .filter(|task| task.status != "archived")
        .collect();
    if active_tasks.is_empty() || active_tasks.iter().any(|task| task.status != "done") {
        return Ok(None);
    }

    let stats = analyze_project(project_root)?;

    // Suppress trivial retrospectives: short runs with zero completions.
    // Completions override the duration check — a short run that finished
    // tasks is still worth reporting.
    if let Some(ref stats) = stats {
        let completed = stats
            .task_stats
            .iter()
            .filter(|t| t.completed_at.is_some())
            .count();
        if stats.total_duration_secs < min_duration_secs && completed == 0 {
            tracing::debug!(
                duration_secs = stats.total_duration_secs,
                completed_tasks = completed,
                "Skipping trivial retrospective: {}s, {} tasks",
                stats.total_duration_secs,
                completed,
            );
            return Ok(None);
        }
    }

    Ok(stats)
}

pub fn generate_retrospective(project_root: &Path, stats: &RunStats) -> Result<PathBuf> {
    let retrospectives_dir = project_root.join(".batty").join("retrospectives");
    fs::create_dir_all(&retrospectives_dir).with_context(|| {
        format!(
            "failed to create retrospectives directory: {}",
            retrospectives_dir.display()
        )
    })?;

    let report_path = retrospectives_dir.join(format!("{}.md", stats.run_end));
    let report = render_retrospective(stats);
    fs::write(&report_path, report)
        .with_context(|| format!("failed to write retrospective: {}", report_path.display()))?;

    Ok(report_path)
}

pub fn format_duration(secs: u64) -> String {
    let hours = secs / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn render_retrospective(stats: &RunStats) -> String {
    let completed_tasks = stats
        .task_stats
        .iter()
        .filter(|task| task.completed_at.is_some())
        .count();
    let average_cycle_time = stats
        .average_cycle_time_secs
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());
    let fastest_cycle_time = stats
        .fastest_cycle_time_secs
        .map(|cycle| {
            format!(
                "{} ({})",
                format_duration(cycle),
                stats.fastest_task_id.as_deref().unwrap_or("-")
            )
        })
        .unwrap_or_else(|| "-".to_string());
    let longest_cycle_time = stats
        .longest_cycle_time_secs
        .map(|cycle| {
            format!(
                "{} ({})",
                format_duration(cycle),
                stats.longest_task_id.as_deref().unwrap_or("-")
            )
        })
        .unwrap_or_else(|| "-".to_string());

    let task_cycle_rows = render_task_cycle_rows(&stats.task_stats);
    let bottlenecks = render_bottlenecks(&stats.task_stats);
    let recommendations = render_recommendations(stats);
    let review_section = render_review_performance(stats);

    format!(
        "# Batty Retrospective\n\n\
## Summary\n\n\
- Duration: {}\n\
- Tasks completed: {}\n\
- Average cycle time: {}\n\
- Fastest task: {}\n\
- Longest task: {}\n\
- Messages: {}\n\
- Escalations: {}\n\
- Idle: {:.1}%\n\n\
## Task Cycle Times\n\n\
| Task | Assignee | Status | Cycle Time | Retries | Escalated |\n\
| --- | --- | --- | --- | --- | --- |\n\
{}\
\n\
{}\
## Bottlenecks\n\n\
{}\
\n\
## Recommendations\n\n\
{}",
        format_duration(stats.total_duration_secs),
        completed_tasks,
        average_cycle_time,
        fastest_cycle_time,
        longest_cycle_time,
        stats.message_count,
        stats.escalation_count,
        stats.idle_time_pct * 100.0,
        task_cycle_rows,
        review_section,
        bottlenecks,
        recommendations
    )
}

fn render_review_performance(stats: &RunStats) -> String {
    let total_merges = stats.auto_merge_count + stats.manual_merge_count;
    if total_merges == 0 && stats.rework_count == 0 && stats.review_nudge_count == 0 {
        return String::new();
    }

    let auto_rate = if total_merges > 0 {
        format!(
            "{:.0}%",
            stats.auto_merge_count as f64 / total_merges as f64 * 100.0
        )
    } else {
        "-".to_string()
    };
    let total_reviewed = total_merges + stats.rework_count;
    let rework_rate = if total_reviewed > 0 {
        format!(
            "{:.0}%",
            stats.rework_count as f64 / total_reviewed as f64 * 100.0
        )
    } else {
        "-".to_string()
    };

    let avg_stall = stats
        .avg_review_stall_secs
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());
    let max_stall = stats
        .max_review_stall_secs
        .map(|secs| {
            format!(
                "{} ({})",
                format_duration(secs),
                stats.max_review_stall_task.as_deref().unwrap_or("-")
            )
        })
        .unwrap_or_else(|| "-".to_string());

    let mut section = format!(
        "## Review Pipeline\n\n\
- Auto-merged: {}\n\
- Manually merged: {}\n\
- Auto-merge rate: {}\n\
- Avg review stall: {}\n\
- Max review stall: {}\n\
- Rework cycles: {}\n\
- Rework rate: {}\n\
- Review nudges: {}\n\
- Review escalations: {}\n",
        stats.auto_merge_count,
        stats.manual_merge_count,
        auto_rate,
        avg_stall,
        max_stall,
        stats.rework_count,
        rework_rate,
        stats.review_nudge_count,
        stats.review_escalation_count,
    );

    if !stats.task_rework_counts.is_empty() {
        section.push_str(
            "\n### Rework by Task\n\n\
| Task | Rework Cycles |\n\
| --- | --- |\n",
        );
        for (task_id, count) in &stats.task_rework_counts {
            section.push_str(&format!("| {} | {} |\n", task_id, count));
        }
    }

    section.push('\n');
    section
}

fn render_task_cycle_rows(tasks: &[TaskStats]) -> String {
    if tasks.is_empty() {
        return "| No tasks recorded | - | - | - | - | - |\n".to_string();
    }

    let mut rows = String::new();
    for task in tasks {
        let status = if task.completed_at.is_some() {
            "completed"
        } else {
            "incomplete"
        };
        let cycle_time = task
            .cycle_time_secs
            .map(format_duration)
            .unwrap_or_else(|| "-".to_string());
        let escalated = if task.was_escalated { "yes" } else { "no" };
        rows.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            task.task_id, task.assigned_to, status, cycle_time, task.retry_count, escalated
        ));
    }
    rows
}

fn render_bottlenecks(tasks: &[TaskStats]) -> String {
    let longest_task = tasks
        .iter()
        .filter_map(|task| task.cycle_time_secs.map(|cycle| (task, cycle)))
        .max_by_key(|(_, cycle)| *cycle);

    let most_retried = tasks.iter().max_by_key(|task| task.retry_count);

    let mut lines = Vec::new();
    match longest_task {
        Some((task, cycle)) => lines.push(format!(
            "- Longest task: `{}` owned by `{}` at {}.",
            task.task_id,
            task.assigned_to,
            format_duration(cycle)
        )),
        None => lines.push("- Longest task: no completed tasks recorded.".to_string()),
    }

    match most_retried {
        Some(task) if task.retry_count > 1 => lines.push(format!(
            "- Most retried: `{}` retried {} times.",
            task.task_id, task.retry_count
        )),
        _ => lines.push("- Most retried: no task needed multiple attempts.".to_string()),
    }

    format!("{}\n", lines.join("\n"))
}

fn render_recommendations(stats: &RunStats) -> String {
    let mut lines = Vec::new();
    let max_retry_count = stats
        .task_stats
        .iter()
        .map(|task| task.retry_count)
        .max()
        .unwrap_or(0);

    if stats.idle_time_pct >= 0.5 {
        lines.push(
            "- Idle time stayed high. Queue more ready tasks so engineers are not waiting on assignment."
                .to_string(),
        );
    }

    if max_retry_count >= 3 {
        lines.push(
            "- Several retries were needed. Break work into smaller tasks with clearer acceptance criteria."
                .to_string(),
        );
    }

    if lines.is_empty() {
        lines.push(
            "- No major bottlenecks stood out. Keep the current task sizing and routing cadence."
                .to_string(),
        );
    }

    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn at(mut event: TeamEvent, ts: u64) -> TeamEvent {
        event.ts = ts;
        event
    }

    #[test]
    fn test_analyze_events_basic_run() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::message_routed("manager", "eng-1"), 115),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 50), 160),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.run_start, 100);
        assert_eq!(stats.run_end, 160);
        assert_eq!(stats.total_duration_secs, 60);
        assert_eq!(stats.escalation_count, 0);
        assert_eq!(stats.message_count, 1);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.average_cycle_time_secs, Some(40));
        assert_eq!(stats.fastest_task_id.as_deref(), Some("42"));
        assert_eq!(stats.fastest_cycle_time_secs, Some(40));
        assert_eq!(stats.longest_task_id.as_deref(), Some("42"));
        assert_eq!(stats.longest_cycle_time_secs, Some(40));
        assert_eq!(
            stats.task_stats[0],
            TaskStats {
                task_id: "42".to_string(),
                assigned_to: "eng-1".to_string(),
                assigned_at: 110,
                completed_at: Some(150),
                cycle_time_secs: Some(40),
                retry_count: 1,
                was_escalated: false,
            }
        );
    }

    #[test]
    fn test_analyze_events_with_retries() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(
                TeamEvent::task_assigned("eng-1", "Task #42: retry task"),
                110,
            ),
            at(
                TeamEvent::task_assigned("eng-1", "Task #42: retry task"),
                130,
            ),
            at(TeamEvent::task_completed("eng-1", None), 170),
            at(TeamEvent::daemon_stopped_with_reason("signal", 70), 180),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].retry_count, 2);
        assert_eq!(stats.task_stats[0].assigned_at, 110);
        assert_eq!(stats.task_stats[0].cycle_time_secs, Some(60));
        assert_eq!(stats.task_stats[0].task_id, "42");
    }

    #[test]
    fn test_analyze_events_with_escalation() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::task_escalated("eng-1", "42", None), 125),
            at(TeamEvent::daemon_stopped_with_reason("signal", 30), 130),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.escalation_count, 1);
        assert_eq!(stats.task_stats.len(), 1);
        assert!(stats.task_stats[0].was_escalated);
        assert_eq!(stats.task_stats[0].completed_at, None);
    }

    #[test]
    fn test_analyze_events_idle_time() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::load_snapshot(1, 4, true), 110),
            at(TeamEvent::load_snapshot(3, 4, true), 120),
            at(TeamEvent::daemon_stopped_with_reason("signal", 25), 125),
        ];

        let stats = analyze_events(&events).unwrap();

        assert!((stats.idle_time_pct - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_analyze_events_empty() {
        assert_eq!(analyze_events(&[]), None);
    }

    #[test]
    fn test_analyze_events_multiple_runs() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "old-task"), 105),
            at(TeamEvent::daemon_stopped_with_reason("signal", 10), 110),
            at(TeamEvent::daemon_started(), 200),
            at(
                TeamEvent::task_assigned("eng-2", "Task #12: new-task\n\nTask details."),
                210,
            ),
            at(TeamEvent::task_completed("eng-2", None), 240),
            at(TeamEvent::daemon_stopped_with_reason("signal", 45), 245),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.run_start, 200);
        assert_eq!(stats.run_end, 245);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "12");
        assert_eq!(stats.task_stats[0].assigned_to, "eng-2");
        assert_eq!(stats.task_stats[0].cycle_time_secs, Some(30));
        assert_eq!(stats.average_cycle_time_secs, Some(30));
        assert_eq!(stats.fastest_task_id.as_deref(), Some("12"));
        assert_eq!(stats.longest_task_id.as_deref(), Some("12"));
    }

    #[test]
    fn test_analyze_events_computes_average_fastest_and_longest_cycle_times() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(
                TeamEvent::task_assigned("eng-1", "Task #11: short task\n\nBody."),
                110,
            ),
            at(TeamEvent::task_completed("eng-1", None), 140),
            at(
                TeamEvent::task_assigned("eng-2", "Task #12: long task\n\nBody."),
                150,
            ),
            at(TeamEvent::task_completed("eng-2", None), 240),
            at(TeamEvent::daemon_stopped_with_reason("signal", 150), 250),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.average_cycle_time_secs, Some(60));
        assert_eq!(stats.fastest_task_id.as_deref(), Some("11"));
        assert_eq!(stats.fastest_cycle_time_secs, Some(30));
        assert_eq!(stats.longest_task_id.as_deref(), Some("12"));
        assert_eq!(stats.longest_cycle_time_secs, Some(90));
    }

    fn sample_task(task_id: &str, cycle_time_secs: Option<u64>, retry_count: u32) -> TaskStats {
        TaskStats {
            task_id: task_id.to_string(),
            assigned_to: "eng-1".to_string(),
            assigned_at: 100,
            completed_at: cycle_time_secs.map(|cycle| 100 + cycle),
            cycle_time_secs,
            retry_count,
            was_escalated: retry_count > 2,
        }
    }

    #[test]
    fn format_duration_variants() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(65), "1m 05s");
        assert_eq!(format_duration(3_665), "1h 01m 05s");
    }

    #[test]
    fn generate_retrospective_writes_report_with_sections() {
        let tmp = tempdir().unwrap();
        let stats = RunStats {
            run_start: 1_700_000_000,
            run_end: 1_700_000_123,
            total_duration_secs: 123,
            task_stats: vec![
                sample_task("T-101", Some(90), 1),
                sample_task("T-102", Some(30), 2),
            ],
            average_cycle_time_secs: Some(60),
            fastest_task_id: Some("T-102".to_string()),
            fastest_cycle_time_secs: Some(30),
            longest_task_id: Some("T-101".to_string()),
            longest_cycle_time_secs: Some(90),
            idle_time_pct: 0.25,
            escalation_count: 1,
            message_count: 6,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(
            path,
            tmp.path()
                .join(".batty")
                .join("retrospectives")
                .join("1700000123.md")
        );
        assert!(content.contains("## Summary"));
        assert!(content.contains("## Task Cycle Times"));
        assert!(content.contains("## Bottlenecks"));
        assert!(content.contains("## Recommendations"));
        assert!(content.contains("| T-101 | eng-1 | completed | 1m 30s | 1 | no |"));
        assert!(content.contains("- Tasks completed: 2"));
        assert!(content.contains("- Average cycle time: 1m 00s"));
        assert!(content.contains("- Fastest task: 30s (T-102)"));
        assert!(content.contains("- Longest task: 1m 30s (T-101)"));
    }

    #[test]
    fn generate_retrospective_handles_empty_tasks() {
        let tmp = tempdir().unwrap();
        let stats = RunStats {
            run_start: 10,
            run_end: 20,
            total_duration_secs: 10,
            task_stats: Vec::new(),
            average_cycle_time_secs: None,
            fastest_task_id: None,
            fastest_cycle_time_secs: None,
            longest_task_id: None,
            longest_cycle_time_secs: None,
            idle_time_pct: 0.0,
            escalation_count: 0,
            message_count: 0,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = fs::read_to_string(path).unwrap();

        assert!(content.contains("| No tasks recorded | - | - | - | - | - |"));
        assert!(content.contains("- Average cycle time: -"));
        assert!(content.contains("- Fastest task: -"));
        assert!(content.contains("- Longest task: -"));
        assert!(content.contains("- Longest task: no completed tasks recorded."));
        assert!(content.contains("- Most retried: no task needed multiple attempts."));
    }

    #[test]
    fn generate_retrospective_adds_high_idle_recommendation() {
        let tmp = tempdir().unwrap();
        let stats = RunStats {
            run_start: 10,
            run_end: 30,
            total_duration_secs: 20,
            task_stats: vec![sample_task("T-201", Some(20), 1)],
            average_cycle_time_secs: Some(20),
            fastest_task_id: Some("T-201".to_string()),
            fastest_cycle_time_secs: Some(20),
            longest_task_id: Some("T-201".to_string()),
            longest_cycle_time_secs: Some(20),
            idle_time_pct: 0.75,
            escalation_count: 0,
            message_count: 1,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = fs::read_to_string(path).unwrap();

        assert!(content.contains("Idle time stayed high"));
        assert!(content.contains("Queue more ready tasks"));
    }

    #[test]
    fn generate_retrospective_adds_high_retry_recommendation() {
        let tmp = tempdir().unwrap();
        let stats = RunStats {
            run_start: 10,
            run_end: 40,
            total_duration_secs: 30,
            task_stats: vec![sample_task("T-301", Some(25), 3)],
            average_cycle_time_secs: Some(25),
            fastest_task_id: Some("T-301".to_string()),
            fastest_cycle_time_secs: Some(25),
            longest_task_id: Some("T-301".to_string()),
            longest_cycle_time_secs: Some(25),
            idle_time_pct: 0.1,
            escalation_count: 0,
            message_count: 2,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = fs::read_to_string(path).unwrap();

        assert!(content.contains("Several retries were needed"));
        assert!(content.contains("smaller tasks"));
    }

    fn write_owned_task_file(
        project_root: &Path,
        task_id: u32,
        title: &str,
        status: &str,
        claimed_by: &str,
    ) {
        let board_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks_dir = board_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let slug = title.replace(' ', "-");
        let task_path = tasks_dir.join(format!("{task_id:03}-{slug}.md"));
        let content = format!(
            r#"---
id: {task_id}
title: "{title}"
status: {status}
claimed_by: {claimed_by}
---

Task body.
"#
        );
        fs::write(task_path, content).unwrap();
    }

    fn write_event_log(project_root: &Path, events: &[TeamEvent]) {
        let events_path = project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        fs::create_dir_all(events_path.parent().unwrap()).unwrap();
        let body = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(events_path, format!("{body}\n")).unwrap();
    }

    #[test]
    fn should_generate_retro_when_all_active_tasks_are_done() {
        let tmp = tempdir().unwrap();
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                at(TeamEvent::daemon_started(), 100),
                at(TeamEvent::task_assigned("eng-1", "45"), 110),
                at(TeamEvent::task_completed("eng-1", None), 150),
                at(TeamEvent::daemon_stopped(), 160),
            ],
        );

        let stats = should_generate_retro(tmp.path(), false, 60)
            .unwrap()
            .unwrap();
        assert_eq!(stats.run_start, 100);
        assert_eq!(stats.run_end, 160);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "45");
    }

    #[test]
    fn should_not_generate_retro_when_task_is_not_done() {
        let tmp = tempdir().unwrap();
        write_owned_task_file(tmp.path(), 45, "retro-task", "in-progress", "eng-1");
        write_event_log(tmp.path(), &[at(TeamEvent::daemon_started(), 100)]);

        let stats = should_generate_retro(tmp.path(), false, 60).unwrap();
        assert_eq!(stats, None);
    }

    #[test]
    fn should_not_generate_retro_twice() {
        let tmp = tempdir().unwrap();
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                at(TeamEvent::daemon_started(), 100),
                at(TeamEvent::task_assigned("eng-1", "45"), 110),
                at(TeamEvent::task_completed("eng-1", None), 150),
                at(TeamEvent::daemon_stopped(), 160),
            ],
        );

        let stats = should_generate_retro(tmp.path(), true, 60).unwrap();
        assert_eq!(stats, None);
    }

    #[test]
    fn skip_retro_for_short_run() {
        let tmp = tempdir().unwrap();
        write_owned_task_file(tmp.path(), 50, "short-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                at(TeamEvent::daemon_started(), 100),
                at(TeamEvent::daemon_stopped(), 104),
            ],
        );

        // 4-second run, 0 completions -> suppressed
        let stats = should_generate_retro(tmp.path(), false, 60).unwrap();
        assert_eq!(stats, None);
    }

    #[test]
    fn generate_retro_for_long_run() {
        let tmp = tempdir().unwrap();
        write_owned_task_file(tmp.path(), 51, "long-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                at(TeamEvent::daemon_started(), 100),
                at(TeamEvent::task_assigned("eng-1", "51"), 110),
                at(TeamEvent::task_completed("eng-1", None), 200),
                at(TeamEvent::task_assigned("eng-1", "52"), 210),
                at(TeamEvent::task_completed("eng-1", None), 300),
                at(TeamEvent::task_assigned("eng-1", "53"), 310),
                at(TeamEvent::task_completed("eng-1", None), 380),
                at(TeamEvent::daemon_stopped(), 400),
            ],
        );

        // 300-second run, 3 completions -> generates
        let stats = should_generate_retro(tmp.path(), false, 60).unwrap();
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert_eq!(stats.total_duration_secs, 300);
    }

    #[test]
    fn skip_retro_for_short_run_with_completions() {
        let tmp = tempdir().unwrap();
        write_owned_task_file(tmp.path(), 55, "quick-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                at(TeamEvent::daemon_started(), 100),
                at(TeamEvent::task_assigned("eng-1", "55"), 105),
                at(TeamEvent::task_completed("eng-1", None), 115),
                at(TeamEvent::task_assigned("eng-1", "56"), 118),
                at(TeamEvent::task_completed("eng-1", None), 125),
                at(TeamEvent::daemon_stopped(), 130),
            ],
        );

        // 30-second run but 2 completions -> generates (completions override)
        let stats = should_generate_retro(tmp.path(), false, 60).unwrap();
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert_eq!(stats.total_duration_secs, 30);
    }

    #[test]
    fn analyze_events_computes_review_stall_duration() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(
                TeamEvent::task_assigned("eng-1", "Task #10: fast task"),
                110,
            ),
            at(TeamEvent::task_completed("eng-1", None), 150),
            // 30s stall before auto-merge
            at(
                TeamEvent::task_auto_merged("eng-1", "Task #10: fast task", 0.9, 2, 10),
                180,
            ),
            at(
                TeamEvent::task_assigned("eng-2", "Task #20: slow task"),
                120,
            ),
            at(TeamEvent::task_completed("eng-2", None), 200),
            // 100s stall before manual merge
            at(TeamEvent::task_manual_merged("Task #20: slow task"), 300),
            at(TeamEvent::daemon_stopped_with_reason("signal", 210), 310),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.auto_merge_count, 1);
        assert_eq!(stats.manual_merge_count, 1);
        // avg of 30s and 100s = 65s
        assert_eq!(stats.avg_review_stall_secs, Some(65));
        // max is 100s for task 20
        assert_eq!(stats.max_review_stall_secs, Some(100));
        assert_eq!(stats.max_review_stall_task.as_deref(), Some("20"));
    }

    #[test]
    fn analyze_events_no_stall_without_merges() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 60), 160),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.avg_review_stall_secs, None);
        assert_eq!(stats.max_review_stall_secs, None);
        assert_eq!(stats.max_review_stall_task, None);
    }

    #[test]
    fn analyze_events_tracks_per_task_rework() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "Task #10: reworked"), 110),
            at(TeamEvent::task_reworked("eng-1", "Task #10: reworked"), 120),
            at(TeamEvent::task_reworked("eng-1", "Task #10: reworked"), 130),
            at(TeamEvent::task_assigned("eng-2", "Task #20: once"), 115),
            at(TeamEvent::task_reworked("eng-2", "Task #20: once"), 140),
            at(TeamEvent::daemon_stopped_with_reason("signal", 60), 160),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.rework_count, 3);
        // Sorted by count descending: task 10 (2), task 20 (1)
        assert_eq!(stats.task_rework_counts.len(), 2);
        assert_eq!(stats.task_rework_counts[0], ("10".to_string(), 2));
        assert_eq!(stats.task_rework_counts[1], ("20".to_string(), 1));
    }

    #[test]
    fn analyze_events_empty_rework_list() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 60), 160),
        ];

        let stats = analyze_events(&events).unwrap();

        assert!(stats.task_rework_counts.is_empty());
    }

    #[test]
    fn render_review_pipeline_section_includes_stall_and_rework() {
        let tmp = tempdir().unwrap();
        let stats = RunStats {
            run_start: 100,
            run_end: 500,
            total_duration_secs: 400,
            task_stats: Vec::new(),
            average_cycle_time_secs: None,
            fastest_task_id: None,
            fastest_cycle_time_secs: None,
            longest_task_id: None,
            longest_cycle_time_secs: None,
            idle_time_pct: 0.0,
            escalation_count: 0,
            message_count: 0,
            auto_merge_count: 3,
            manual_merge_count: 1,
            rework_count: 2,
            review_nudge_count: 1,
            review_escalation_count: 0,
            avg_review_stall_secs: Some(90),
            max_review_stall_secs: Some(180),
            max_review_stall_task: Some("T-5".to_string()),
            task_rework_counts: vec![("T-5".to_string(), 2)],
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = fs::read_to_string(path).unwrap();

        assert!(content.contains("## Review Pipeline"));
        assert!(content.contains("Auto-merged: 3"));
        assert!(content.contains("Manually merged: 1"));
        assert!(content.contains("Auto-merge rate: 75%"));
        assert!(content.contains("Avg review stall: 1m 30s"));
        assert!(content.contains("Max review stall: 3m 00s (T-5)"));
        assert!(content.contains("Rework cycles: 2"));
        assert!(content.contains("### Rework by Task"));
        assert!(content.contains("| T-5 | 2 |"));
    }

    #[test]
    fn render_review_pipeline_no_stall_data() {
        let tmp = tempdir().unwrap();
        let stats = RunStats {
            run_start: 100,
            run_end: 300,
            total_duration_secs: 200,
            task_stats: Vec::new(),
            average_cycle_time_secs: None,
            fastest_task_id: None,
            fastest_cycle_time_secs: None,
            longest_task_id: None,
            longest_cycle_time_secs: None,
            idle_time_pct: 0.0,
            escalation_count: 0,
            message_count: 0,
            auto_merge_count: 2,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = fs::read_to_string(path).unwrap();

        assert!(content.contains("## Review Pipeline"));
        assert!(content.contains("Avg review stall: -"));
        assert!(content.contains("Max review stall: -"));
        assert!(!content.contains("### Rework by Task"));
    }

    // --- New tests for content generation and event analysis ---

    #[test]
    fn task_reference_extracts_id_from_task_prefix() {
        assert_eq!(task_reference("Task #42: build feature"), "42");
    }

    #[test]
    fn task_reference_returns_full_line_when_no_prefix() {
        assert_eq!(task_reference("build feature"), "build feature");
    }

    #[test]
    fn task_reference_skips_blank_lines() {
        assert_eq!(task_reference("\n\n  Task #99: test\nbody"), "99");
    }

    #[test]
    fn task_reference_handles_whitespace_only_input() {
        assert_eq!(task_reference("   "), "");
    }

    #[test]
    fn task_id_from_assignment_line_valid() {
        assert_eq!(
            task_id_from_assignment_line("Task #123: some task"),
            Some("123".to_string())
        );
    }

    #[test]
    fn task_id_from_assignment_line_no_prefix() {
        assert_eq!(task_id_from_assignment_line("no prefix here"), None);
    }

    #[test]
    fn task_id_from_assignment_line_empty_digits() {
        assert_eq!(task_id_from_assignment_line("Task #abc: letters"), None);
    }

    #[test]
    fn cycle_time_metrics_no_completed_tasks() {
        let tasks = vec![sample_task("T-1", None, 1)];
        let (avg, fastest, fastest_time, longest, longest_time) = cycle_time_metrics(&tasks);
        assert_eq!(avg, None);
        assert_eq!(fastest, None);
        assert_eq!(fastest_time, None);
        assert_eq!(longest, None);
        assert_eq!(longest_time, None);
    }

    #[test]
    fn cycle_time_metrics_single_completed_task() {
        let tasks = vec![sample_task("T-1", Some(60), 1)];
        let (avg, fastest, fastest_time, longest, longest_time) = cycle_time_metrics(&tasks);
        assert_eq!(avg, Some(60));
        assert_eq!(fastest, Some("T-1".to_string()));
        assert_eq!(fastest_time, Some(60));
        assert_eq!(longest, Some("T-1".to_string()));
        assert_eq!(longest_time, Some(60));
    }

    #[test]
    fn cycle_time_metrics_multiple_tasks_picks_extremes() {
        let tasks = vec![
            sample_task("T-fast", Some(10), 1),
            sample_task("T-mid", Some(50), 1),
            sample_task("T-slow", Some(90), 1),
            sample_task("T-incomplete", None, 1),
        ];
        let (avg, fastest, fastest_time, longest, longest_time) = cycle_time_metrics(&tasks);
        assert_eq!(avg, Some(50)); // (10 + 50 + 90) / 3
        assert_eq!(fastest, Some("T-fast".to_string()));
        assert_eq!(fastest_time, Some(10));
        assert_eq!(longest, Some("T-slow".to_string()));
        assert_eq!(longest_time, Some(90));
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(0), "0s");
    }

    #[test]
    fn format_duration_exact_minute() {
        assert_eq!(format_duration(60), "1m 00s");
    }

    #[test]
    fn format_duration_exact_hour() {
        assert_eq!(format_duration(3600), "1h 00m 00s");
    }

    #[test]
    fn format_duration_large() {
        assert_eq!(format_duration(7322), "2h 02m 02s");
    }

    #[test]
    fn render_task_cycle_rows_empty() {
        let rows = render_task_cycle_rows(&[]);
        assert!(rows.contains("No tasks recorded"));
    }

    #[test]
    fn render_task_cycle_rows_completed_and_incomplete() {
        let tasks = vec![
            sample_task("T-1", Some(120), 1),
            sample_task("T-2", None, 2),
        ];
        let rows = render_task_cycle_rows(&tasks);
        assert!(rows.contains("| T-1 | eng-1 | completed | 2m 00s | 1 | no |"));
        assert!(rows.contains("| T-2 | eng-1 | incomplete | - | 2 | no |"));
    }

    #[test]
    fn render_task_cycle_rows_escalated_task() {
        let tasks = vec![sample_task("T-esc", Some(200), 4)]; // retry > 2 → escalated
        let rows = render_task_cycle_rows(&tasks);
        assert!(rows.contains("| T-esc | eng-1 | completed | 3m 20s | 4 | yes |"));
    }

    #[test]
    fn render_bottlenecks_no_completed_tasks() {
        let tasks = vec![sample_task("T-1", None, 1)];
        let output = render_bottlenecks(&tasks);
        assert!(output.contains("no completed tasks recorded"));
        assert!(output.contains("no task needed multiple attempts"));
    }

    #[test]
    fn render_bottlenecks_with_retries() {
        let tasks = vec![
            sample_task("T-1", Some(100), 1),
            sample_task("T-2", Some(200), 3),
        ];
        let output = render_bottlenecks(&tasks);
        assert!(output.contains("Longest task: `T-2`"));
        assert!(output.contains("Most retried: `T-2` retried 3 times"));
    }

    #[test]
    fn render_bottlenecks_single_retry_shows_no_retries_message() {
        let tasks = vec![sample_task("T-1", Some(60), 1)];
        let output = render_bottlenecks(&tasks);
        assert!(output.contains("no task needed multiple attempts"));
    }

    #[test]
    fn render_recommendations_low_idle_low_retries() {
        let stats = RunStats {
            run_start: 0,
            run_end: 100,
            total_duration_secs: 100,
            task_stats: vec![sample_task("T-1", Some(50), 1)],
            average_cycle_time_secs: Some(50),
            fastest_task_id: Some("T-1".to_string()),
            fastest_cycle_time_secs: Some(50),
            longest_task_id: Some("T-1".to_string()),
            longest_cycle_time_secs: Some(50),
            idle_time_pct: 0.1,
            escalation_count: 0,
            message_count: 1,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };
        let output = render_recommendations(&stats);
        assert!(output.contains("No major bottlenecks"));
    }

    #[test]
    fn render_recommendations_both_high_idle_and_high_retries() {
        let stats = RunStats {
            run_start: 0,
            run_end: 100,
            total_duration_secs: 100,
            task_stats: vec![sample_task("T-1", Some(50), 5)],
            average_cycle_time_secs: Some(50),
            fastest_task_id: Some("T-1".to_string()),
            fastest_cycle_time_secs: Some(50),
            longest_task_id: Some("T-1".to_string()),
            longest_cycle_time_secs: Some(50),
            idle_time_pct: 0.8,
            escalation_count: 0,
            message_count: 1,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };
        let output = render_recommendations(&stats);
        assert!(output.contains("Idle time stayed high"));
        assert!(output.contains("Several retries were needed"));
    }

    #[test]
    fn render_review_performance_empty_when_no_merges() {
        let stats = RunStats {
            run_start: 0,
            run_end: 100,
            total_duration_secs: 100,
            task_stats: Vec::new(),
            average_cycle_time_secs: None,
            fastest_task_id: None,
            fastest_cycle_time_secs: None,
            longest_task_id: None,
            longest_cycle_time_secs: None,
            idle_time_pct: 0.0,
            escalation_count: 0,
            message_count: 0,
            auto_merge_count: 0,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };
        let section = render_review_performance(&stats);
        assert!(section.is_empty());
    }

    #[test]
    fn render_review_performance_100_percent_auto_merge_rate() {
        let stats = RunStats {
            run_start: 0,
            run_end: 100,
            total_duration_secs: 100,
            task_stats: Vec::new(),
            average_cycle_time_secs: None,
            fastest_task_id: None,
            fastest_cycle_time_secs: None,
            longest_task_id: None,
            longest_cycle_time_secs: None,
            idle_time_pct: 0.0,
            escalation_count: 0,
            message_count: 0,
            auto_merge_count: 5,
            manual_merge_count: 0,
            rework_count: 0,
            review_nudge_count: 0,
            review_escalation_count: 0,
            avg_review_stall_secs: None,
            max_review_stall_secs: None,
            max_review_stall_task: None,
            task_rework_counts: Vec::new(),
        };
        let section = render_review_performance(&stats);
        assert!(section.contains("Auto-merge rate: 100%"));
        assert!(section.contains("Auto-merged: 5"));
        assert!(section.contains("Manually merged: 0"));
    }

    #[test]
    fn analyze_events_multiple_tasks_different_engineers() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "Task #10: task-a"), 110),
            at(TeamEvent::task_assigned("eng-2", "Task #20: task-b"), 115),
            at(TeamEvent::task_completed("eng-1", None), 160),
            at(TeamEvent::task_completed("eng-2", None), 200),
            at(TeamEvent::daemon_stopped_with_reason("signal", 110), 210),
        ];

        let stats = analyze_events(&events).unwrap();
        assert_eq!(stats.task_stats.len(), 2);

        let t10 = stats.task_stats.iter().find(|t| t.task_id == "10").unwrap();
        assert_eq!(t10.assigned_to, "eng-1");
        assert_eq!(t10.cycle_time_secs, Some(50));

        let t20 = stats.task_stats.iter().find(|t| t.task_id == "20").unwrap();
        assert_eq!(t20.assigned_to, "eng-2");
        assert_eq!(t20.cycle_time_secs, Some(85));
    }

    #[test]
    fn analyze_events_tracks_review_nudges_and_escalations() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(
                TeamEvent::review_nudge_sent("manager", "Task #5: reviewed"),
                120,
            ),
            at(
                TeamEvent::review_nudge_sent("manager", "Task #5: reviewed"),
                140,
            ),
            at(
                TeamEvent::review_escalated("Task #5: reviewed", "stale"),
                160,
            ),
            at(TeamEvent::daemon_stopped_with_reason("signal", 80), 180),
        ];

        let stats = analyze_events(&events).unwrap();
        assert_eq!(stats.review_nudge_count, 2);
        assert_eq!(stats.review_escalation_count, 1);
    }

    #[test]
    fn analyze_events_completion_without_assignment_is_ignored() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            // Completion without prior assignment
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 60), 160),
        ];

        let stats = analyze_events(&events).unwrap();
        assert!(stats.task_stats.is_empty());
        assert_eq!(stats.average_cycle_time_secs, None);
    }

    #[test]
    fn analyze_events_escalation_without_prior_assignment_creates_task() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(
                TeamEvent::task_escalated("eng-1", "Task #99: escalated-only", None),
                120,
            ),
            at(TeamEvent::daemon_stopped_with_reason("signal", 30), 130),
        ];

        let stats = analyze_events(&events).unwrap();
        assert_eq!(stats.escalation_count, 1);
        assert_eq!(stats.task_stats.len(), 1);
        assert!(stats.task_stats[0].was_escalated);
        // task_escalated stores the raw task string, not parsed through task_reference
        assert_eq!(stats.task_stats[0].task_id, "Task #99: escalated-only");
    }

    #[test]
    fn analyze_events_daemon_started_only() {
        let events = vec![at(TeamEvent::daemon_started(), 100)];
        let stats = analyze_events(&events).unwrap();
        assert_eq!(stats.run_start, 100);
        assert_eq!(stats.run_end, 100);
        assert_eq!(stats.total_duration_secs, 0);
        assert!(stats.task_stats.is_empty());
    }

    #[test]
    fn analyze_events_load_snapshot_all_working() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::load_snapshot(4, 4, true), 110),
            at(TeamEvent::load_snapshot(4, 4, true), 120),
            at(TeamEvent::daemon_stopped_with_reason("signal", 30), 130),
        ];

        let stats = analyze_events(&events).unwrap();
        assert!((stats.idle_time_pct - 0.0).abs() < 1e-9);
    }

    #[test]
    fn analyze_events_load_snapshot_all_idle() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::load_snapshot(0, 4, true), 110),
            at(TeamEvent::load_snapshot(0, 4, true), 120),
            at(TeamEvent::daemon_stopped_with_reason("signal", 30), 130),
        ];

        let stats = analyze_events(&events).unwrap();
        assert!((stats.idle_time_pct - 1.0).abs() < 1e-9);
    }

    #[test]
    fn analyze_events_load_snapshot_zero_members() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::load_snapshot(0, 0, true), 110),
            at(TeamEvent::daemon_stopped_with_reason("signal", 20), 120),
        ];

        let stats = analyze_events(&events).unwrap();
        assert!((stats.idle_time_pct - 1.0).abs() < 1e-9);
    }

    #[test]
    fn should_generate_retro_no_board_dir_returns_none() {
        let tmp = tempdir().unwrap();
        // No board dir at all
        let result = should_generate_retro(tmp.path(), false, 60).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn should_generate_retro_empty_board_returns_none() {
        let tmp = tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        // tasks dir exists but is empty
        let result = should_generate_retro(tmp.path(), false, 60).unwrap();
        assert_eq!(result, None);
    }

    // --- SQLite telemetry DB tests ---

    use crate::team::telemetry_db;

    /// Populate a telemetry DB with events matching the basic_run test scenario.
    fn populate_basic_run_db(conn: &Connection) {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::message_routed("manager", "eng-1"), 115),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 50), 160),
        ];
        for event in &events {
            telemetry_db::insert_event(conn, event).unwrap();
        }
    }

    #[test]
    fn retro_with_telemetry_db() {
        let conn = telemetry_db::open_in_memory().unwrap();
        populate_basic_run_db(&conn);

        let stats = analyze_from_db(&conn).unwrap();

        assert_eq!(stats.run_start, 100);
        assert_eq!(stats.run_end, 160);
        assert_eq!(stats.total_duration_secs, 60);
        assert_eq!(stats.message_count, 1);
        assert_eq!(stats.escalation_count, 0);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "42");
        assert_eq!(stats.task_stats[0].assigned_to, "eng-1");
        assert_eq!(stats.task_stats[0].cycle_time_secs, Some(40));
        assert_eq!(stats.average_cycle_time_secs, Some(40));
        assert_eq!(stats.fastest_task_id.as_deref(), Some("42"));
        assert_eq!(stats.longest_task_id.as_deref(), Some("42"));
    }

    #[test]
    fn retro_from_db_matches_jsonl_analysis() {
        // Same events through both paths should produce identical RunStats.
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::message_routed("manager", "eng-1"), 115),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 50), 160),
        ];

        let jsonl_stats = analyze_events(&events).unwrap();

        let conn = telemetry_db::open_in_memory().unwrap();
        for event in &events {
            telemetry_db::insert_event(&conn, event).unwrap();
        }
        let db_stats = analyze_from_db(&conn).unwrap();

        assert_eq!(jsonl_stats.run_start, db_stats.run_start);
        assert_eq!(jsonl_stats.run_end, db_stats.run_end);
        assert_eq!(
            jsonl_stats.total_duration_secs,
            db_stats.total_duration_secs
        );
        assert_eq!(jsonl_stats.task_stats.len(), db_stats.task_stats.len());
        assert_eq!(
            jsonl_stats.average_cycle_time_secs,
            db_stats.average_cycle_time_secs
        );
        assert_eq!(jsonl_stats.fastest_task_id, db_stats.fastest_task_id);
        assert_eq!(jsonl_stats.longest_task_id, db_stats.longest_task_id);
        assert_eq!(jsonl_stats.escalation_count, db_stats.escalation_count);
        assert_eq!(jsonl_stats.message_count, db_stats.message_count);
        assert_eq!(jsonl_stats.idle_time_pct, db_stats.idle_time_pct);
        assert_eq!(jsonl_stats.auto_merge_count, db_stats.auto_merge_count);
        assert_eq!(jsonl_stats.manual_merge_count, db_stats.manual_merge_count);
        assert_eq!(jsonl_stats.rework_count, db_stats.rework_count);
    }

    #[test]
    fn retro_without_db_falls_back() {
        // analyze_project with no DB file should fall back to JSONL.
        let tmp = tempdir().unwrap();
        let events_dir = tmp.path().join(".batty").join("team_config");
        fs::create_dir_all(&events_dir).unwrap();

        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped(), 160),
        ];
        write_event_log(tmp.path(), &events);

        // No telemetry.db exists — should fall back to JSONL.
        let stats = analyze_project(tmp.path()).unwrap().unwrap();
        assert_eq!(stats.run_start, 100);
        assert_eq!(stats.run_end, 160);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "42");
    }

    #[test]
    fn retro_report_format_unchanged() {
        // Verify DB-sourced stats produce identical Markdown structure.
        let conn = telemetry_db::open_in_memory().unwrap();
        populate_basic_run_db(&conn);

        let stats = analyze_from_db(&conn).unwrap();
        let report = render_retrospective(&stats);

        assert!(report.contains("# Batty Retrospective"));
        assert!(report.contains("## Summary"));
        assert!(report.contains("## Task Cycle Times"));
        assert!(report.contains("## Bottlenecks"));
        assert!(report.contains("## Recommendations"));
        assert!(report.contains("- Tasks completed: 1"));
        assert!(report.contains("- Average cycle time: 40s"));
        assert!(report.contains("| 42 | eng-1 | completed | 40s | 1 | no |"));
    }

    #[test]
    fn retro_from_db_empty_returns_none() {
        let conn = telemetry_db::open_in_memory().unwrap();
        assert!(analyze_from_db(&conn).is_none());
    }

    #[test]
    fn retro_from_db_with_retries_and_escalations() {
        let conn = telemetry_db::open_in_memory().unwrap();
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(
                TeamEvent::task_assigned("eng-1", "Task #42: retry task"),
                110,
            ),
            at(
                TeamEvent::task_assigned("eng-1", "Task #42: retry task"),
                130,
            ),
            at(TeamEvent::task_escalated("eng-1", "42", None), 135),
            at(TeamEvent::task_completed("eng-1", None), 170),
            at(TeamEvent::daemon_stopped_with_reason("signal", 70), 180),
        ];
        for event in &events {
            telemetry_db::insert_event(&conn, event).unwrap();
        }

        let stats = analyze_from_db(&conn).unwrap();
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "42");
        assert_eq!(stats.task_stats[0].retry_count, 2);
        assert!(stats.task_stats[0].was_escalated);
        assert_eq!(stats.task_stats[0].cycle_time_secs, Some(60));
        assert_eq!(stats.escalation_count, 1);
    }

    #[test]
    fn retro_from_db_with_review_pipeline() {
        let conn = telemetry_db::open_in_memory().unwrap();
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "Task #10: fast"), 110),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(
                TeamEvent::task_auto_merged("eng-1", "Task #10: fast", 0.9, 2, 10),
                180,
            ),
            at(TeamEvent::task_assigned("eng-2", "Task #20: slow"), 120),
            at(TeamEvent::task_completed("eng-2", None), 200),
            at(TeamEvent::task_manual_merged("Task #20: slow"), 300),
            at(TeamEvent::task_reworked("eng-1", "Task #10: fast"), 145),
            at(
                TeamEvent::review_nudge_sent("manager", "Task #10: fast"),
                155,
            ),
            at(TeamEvent::daemon_stopped_with_reason("signal", 210), 310),
        ];
        for event in &events {
            telemetry_db::insert_event(&conn, event).unwrap();
        }

        let stats = analyze_from_db(&conn).unwrap();
        assert_eq!(stats.auto_merge_count, 1);
        assert_eq!(stats.manual_merge_count, 1);
        assert_eq!(stats.rework_count, 1);
        assert_eq!(stats.review_nudge_count, 1);
        // avg of 30s and 100s = 65s
        assert_eq!(stats.avg_review_stall_secs, Some(65));
        assert_eq!(stats.max_review_stall_secs, Some(100));
        assert_eq!(stats.max_review_stall_task.as_deref(), Some("20"));
        assert_eq!(stats.task_rework_counts, vec![("10".to_string(), 1)]);
    }

    #[test]
    fn retro_from_db_multiple_runs_uses_last() {
        let conn = telemetry_db::open_in_memory().unwrap();
        let events = vec![
            // First run
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "old-task"), 105),
            at(TeamEvent::daemon_stopped_with_reason("signal", 10), 110),
            // Second run
            at(TeamEvent::daemon_started(), 200),
            at(TeamEvent::task_assigned("eng-2", "Task #12: new-task"), 210),
            at(TeamEvent::task_completed("eng-2", None), 240),
            at(TeamEvent::daemon_stopped_with_reason("signal", 45), 245),
        ];
        for event in &events {
            telemetry_db::insert_event(&conn, event).unwrap();
        }

        let stats = analyze_from_db(&conn).unwrap();
        assert_eq!(stats.run_start, 200);
        assert_eq!(stats.run_end, 245);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "12");
    }

    #[test]
    fn analyze_project_prefers_db_over_jsonl() {
        let tmp = tempdir().unwrap();
        // Set up JSONL with task "99"
        let jsonl_events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "99"), 110),
            at(TeamEvent::task_completed("eng-1", None), 150),
            at(TeamEvent::daemon_stopped(), 160),
        ];
        write_event_log(tmp.path(), &jsonl_events);

        // Set up DB with task "42"
        let db_path = tmp.path().join(".batty");
        fs::create_dir_all(&db_path).unwrap();
        let conn = telemetry_db::open(tmp.path()).unwrap();
        let db_events = vec![
            at(TeamEvent::daemon_started(), 200),
            at(TeamEvent::task_assigned("eng-1", "42"), 210),
            at(TeamEvent::task_completed("eng-1", None), 250),
            at(TeamEvent::daemon_stopped(), 260),
        ];
        for event in &db_events {
            telemetry_db::insert_event(&conn, event).unwrap();
        }
        drop(conn);

        // analyze_project should use DB (task "42"), not JSONL (task "99").
        let stats = analyze_project(tmp.path()).unwrap().unwrap();
        assert_eq!(stats.task_stats[0].task_id, "42");
    }
}
