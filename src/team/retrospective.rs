//! Pure event-log analysis for retrospective metrics.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use super::events::{TeamEvent, read_events};

#[derive(Debug, Clone, PartialEq)]
pub struct RunStats {
    pub run_start: u64,
    pub run_end: u64,
    pub total_duration_secs: u64,
    pub task_stats: Vec<TaskStats>,
    pub idle_time_pct: f64,
    pub escalation_count: u32,
    pub message_count: u32,
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

    for event in run_events {
        match event.event.as_str() {
            "task_assigned" => {
                let Some(role) = event.role.as_deref() else {
                    continue;
                };
                let Some(task_id) = event.task.as_deref() else {
                    continue;
                };

                let entry = tasks.entry(task_id.to_string()).or_insert_with(|| {
                    TaskAccumulator::new(task_id.to_string(), role.to_string(), event.ts, 0)
                });
                entry.retry_count += 1;
                entry.assigned_to = role.to_string();
                active_task_by_role.insert(role.to_string(), task_id.to_string());
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

    Some(RunStats {
        run_start,
        run_end,
        total_duration_secs: run_end.saturating_sub(run_start),
        task_stats,
        idle_time_pct,
        escalation_count,
        message_count,
    })
}

/// Parse the events file and analyze.
#[allow(dead_code)]
pub fn analyze_event_log(path: &Path) -> Result<Option<RunStats>> {
    let events = read_events(path)?;
    Ok(analyze_events(&events))
}

#[cfg(test)]
mod tests {
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
            at(TeamEvent::task_completed("eng-1"), 150),
            at(TeamEvent::daemon_stopped_with_reason("signal", 50), 160),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.run_start, 100);
        assert_eq!(stats.run_end, 160);
        assert_eq!(stats.total_duration_secs, 60);
        assert_eq!(stats.escalation_count, 0);
        assert_eq!(stats.message_count, 1);
        assert_eq!(stats.task_stats.len(), 1);
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
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::task_assigned("eng-1", "42"), 130),
            at(TeamEvent::task_completed("eng-1"), 170),
            at(TeamEvent::daemon_stopped_with_reason("signal", 70), 180),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].retry_count, 2);
        assert_eq!(stats.task_stats[0].assigned_at, 110);
        assert_eq!(stats.task_stats[0].cycle_time_secs, Some(60));
    }

    #[test]
    fn test_analyze_events_with_escalation() {
        let events = vec![
            at(TeamEvent::daemon_started(), 100),
            at(TeamEvent::task_assigned("eng-1", "42"), 110),
            at(TeamEvent::task_escalated("eng-1", "42"), 125),
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
            at(TeamEvent::task_assigned("eng-2", "new-task"), 210),
            at(TeamEvent::task_completed("eng-2"), 240),
            at(TeamEvent::daemon_stopped_with_reason("signal", 45), 245),
        ];

        let stats = analyze_events(&events).unwrap();

        assert_eq!(stats.run_start, 200);
        assert_eq!(stats.run_end, 245);
        assert_eq!(stats.task_stats.len(), 1);
        assert_eq!(stats.task_stats[0].task_id, "new-task");
        assert_eq!(stats.task_stats[0].assigned_to, "eng-2");
        assert_eq!(stats.task_stats[0].cycle_time_secs, Some(30));
    }
}
