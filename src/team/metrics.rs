pub use super::status::{
    WorkflowMetrics, compute_metrics, compute_metrics_with_events, compute_metrics_with_telemetry,
};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, TimeZone, Utc};

use crate::task::{Task, load_tasks_from_dir};

use super::board::read_task_lifecycle_timestamps;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCycleTimeRecord {
    pub task_id: u32,
    pub title: String,
    pub engineer: Option<String>,
    pub priority: String,
    pub status: String,
    pub created_at: Option<i64>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub cycle_time_minutes: Option<i64>,
    pub lead_time_minutes: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriorityCycleTimeSummary {
    pub priority: String,
    pub average_cycle_time_minutes: f64,
    pub completed_tasks: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EngineerThroughputSummary {
    pub engineer: String,
    pub completed_tasks: usize,
    pub average_cycle_time_minutes: Option<f64>,
    pub average_lead_time_minutes: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HourlyCompletionCount {
    pub hour_start: i64,
    pub completed_tasks: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgressTaskSummary {
    pub task_id: u32,
    pub title: String,
    pub engineer: Option<String>,
    pub priority: String,
    pub minutes_in_progress: i64,
}

pub fn build_task_cycle_time_record(
    task_id: u32,
    title: impl Into<String>,
    engineer: Option<&str>,
    priority: impl Into<String>,
    status: impl Into<String>,
    created_at: Option<DateTime<FixedOffset>>,
    started_at: Option<DateTime<FixedOffset>>,
    completed_at: Option<DateTime<FixedOffset>>,
) -> TaskCycleTimeRecord {
    let created_ts = created_at.map(|value| value.timestamp());
    let started_ts = started_at.map(|value| value.timestamp());
    let completed_ts = completed_at.map(|value| value.timestamp());
    let cycle_time_minutes = duration_minutes(started_at, completed_at);
    let lead_time_minutes = duration_minutes(created_at, completed_at);

    TaskCycleTimeRecord {
        task_id,
        title: title.into(),
        engineer: engineer.map(str::to_string),
        priority: priority.into(),
        status: status.into(),
        created_at: created_ts,
        started_at: started_ts,
        completed_at: completed_ts,
        cycle_time_minutes,
        lead_time_minutes,
    }
}

pub fn collect_task_cycle_time_records(board_dir: &Path) -> Result<Vec<TaskCycleTimeRecord>> {
    let mut records = Vec::new();
    for task in load_tasks_from_paths(&task_data_dirs(board_dir))? {
        let lifecycle = read_task_lifecycle_timestamps(&task.source_path)?;
        records.push(build_task_cycle_time_record(
            task.id,
            &task.title,
            task.claimed_by.as_deref(),
            normalized_priority(&task.priority),
            &task.status,
            lifecycle.created,
            lifecycle.started,
            lifecycle.completed,
        ));
    }
    records.sort_by_key(|record| record.task_id);
    Ok(records)
}

pub fn average_cycle_time_by_priority(
    records: &[TaskCycleTimeRecord],
) -> Vec<PriorityCycleTimeSummary> {
    let mut buckets = BTreeMap::<String, (i64, usize)>::new();
    for record in records {
        let Some(cycle_time_minutes) = record.cycle_time_minutes else {
            continue;
        };
        let entry = buckets.entry(normalized_priority(&record.priority)).or_default();
        entry.0 += cycle_time_minutes;
        entry.1 += 1;
    }

    buckets
        .into_iter()
        .filter(|(_, (_, count))| *count > 0)
        .map(|(priority, (sum, count))| PriorityCycleTimeSummary {
            priority,
            average_cycle_time_minutes: sum as f64 / count as f64,
            completed_tasks: count,
        })
        .collect()
}

pub fn engineer_throughput_ranking(
    records: &[TaskCycleTimeRecord],
) -> Vec<EngineerThroughputSummary> {
    #[derive(Default)]
    struct Totals {
        completed_tasks: usize,
        cycle_minutes_sum: i64,
        cycle_samples: usize,
        lead_minutes_sum: i64,
        lead_samples: usize,
    }

    let mut by_engineer = BTreeMap::<String, Totals>::new();
    for record in records {
        let Some(engineer) = record.engineer.as_deref() else {
            continue;
        };
        if record.completed_at.is_none() {
            continue;
        }

        let entry = by_engineer.entry(engineer.to_string()).or_default();
        entry.completed_tasks += 1;
        if let Some(cycle_time_minutes) = record.cycle_time_minutes {
            entry.cycle_minutes_sum += cycle_time_minutes;
            entry.cycle_samples += 1;
        }
        if let Some(lead_time_minutes) = record.lead_time_minutes {
            entry.lead_minutes_sum += lead_time_minutes;
            entry.lead_samples += 1;
        }
    }

    let mut summaries = by_engineer
        .into_iter()
        .map(|(engineer, totals)| EngineerThroughputSummary {
            engineer,
            completed_tasks: totals.completed_tasks,
            average_cycle_time_minutes: (totals.cycle_samples > 0)
                .then(|| totals.cycle_minutes_sum as f64 / totals.cycle_samples as f64),
            average_lead_time_minutes: (totals.lead_samples > 0)
                .then(|| totals.lead_minutes_sum as f64 / totals.lead_samples as f64),
        })
        .collect::<Vec<_>>();

    summaries.sort_by(|left, right| {
        right
            .completed_tasks
            .cmp(&left.completed_tasks)
            .then_with(|| left.engineer.cmp(&right.engineer))
    });
    summaries
}

pub fn tasks_completed_per_hour(
    records: &[TaskCycleTimeRecord],
    now: DateTime<Utc>,
    window_hours: i64,
) -> Vec<HourlyCompletionCount> {
    let window_hours = window_hours.max(1);
    let current_hour = now.timestamp().div_euclid(3600) * 3600;
    let start_hour = current_hour - (window_hours - 1) * 3600;
    let mut counts = BTreeMap::<i64, i64>::new();

    for offset in 0..window_hours {
        counts.insert(start_hour + offset * 3600, 0);
    }

    for record in records {
        let Some(completed_at) = record.completed_at else {
            continue;
        };
        let bucket = completed_at.div_euclid(3600) * 3600;
        if let Some(count) = counts.get_mut(&bucket) {
            *count += 1;
        }
    }

    counts
        .into_iter()
        .map(|(hour_start, completed_tasks)| HourlyCompletionCount {
            hour_start,
            completed_tasks,
        })
        .collect()
}

pub fn longest_running_in_progress_tasks(
    records: &[TaskCycleTimeRecord],
    now: DateTime<Utc>,
    limit: usize,
) -> Vec<InProgressTaskSummary> {
    let mut tasks = records
        .iter()
        .filter(|record| record.status == "in-progress" && record.completed_at.is_none())
        .filter_map(|record| {
            let started_at = record.started_at?;
            let started_at = Utc.timestamp_opt(started_at, 0).single()?;
            let elapsed = now.signed_duration_since(started_at).num_minutes().max(0);
            Some(InProgressTaskSummary {
                task_id: record.task_id,
                title: record.title.clone(),
                engineer: record.engineer.clone(),
                priority: normalized_priority(&record.priority),
                minutes_in_progress: elapsed,
            })
        })
        .collect::<Vec<_>>();

    tasks.sort_by(|left, right| {
        right
            .minutes_in_progress
            .cmp(&left.minutes_in_progress)
            .then_with(|| left.task_id.cmp(&right.task_id))
    });
    tasks.truncate(limit);
    tasks
}

fn duration_minutes(
    start: Option<DateTime<FixedOffset>>,
    end: Option<DateTime<FixedOffset>>,
) -> Option<i64> {
    let (start, end) = (start?, end?);
    let duration = end.signed_duration_since(start);
    (duration >= Duration::zero()).then(|| duration.num_minutes())
}

fn normalized_priority(priority: &str) -> String {
    let trimmed = priority.trim();
    if trimmed.is_empty() {
        "unspecified".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

fn task_data_dirs(board_dir: &Path) -> Vec<PathBuf> {
    [board_dir.join("tasks"), board_dir.join("archive")]
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

fn load_tasks_from_paths(paths: &[PathBuf]) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    for path in paths {
        tasks.extend(load_tasks_from_dir(path)?);
    }
    Ok(tasks)
}

#[cfg(test)]
use super::status::format_metrics;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::team::config::RoleType;
    use crate::team::hierarchy::MemberInstance;

    fn make_member(name: &str, role_type: RoleType) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        }
    }

    fn write_task(
        board_dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        blocked: Option<&str>,
        depends_on: &[u32],
    ) {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        if let Some(blocked) = blocked {
            content.push_str(&format!("blocked: {blocked}\n"));
        }
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep in depends_on {
                content.push_str(&format!("  - {dep}\n"));
            }
        }
        content.push_str("class: standard\n---\n\nTask body.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    #[test]
    fn compute_metrics_handles_empty_board() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();

        let metrics = compute_metrics(&board_dir, &[]).unwrap();
        assert_eq!(metrics, WorkflowMetrics::default());
    }

    #[test]
    fn compute_metrics_counts_mixed_workflow_states() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        write_task(&board_dir, 1, "done-dep", "done", None, None, &[]);
        write_task(&board_dir, 2, "runnable", "todo", None, None, &[1]);
        write_task(
            &board_dir,
            3,
            "blocked",
            "blocked",
            Some("eng-1"),
            Some("waiting"),
            &[],
        );
        write_task(&board_dir, 4, "review", "review", Some("eng-2"), None, &[]);
        write_task(
            &board_dir,
            5,
            "active",
            "in-progress",
            Some("eng-1"),
            None,
            &[],
        );

        let members = vec![
            make_member("eng-1", RoleType::Engineer),
            make_member("eng-2", RoleType::Engineer),
            make_member("eng-3", RoleType::Engineer),
        ];
        let metrics = compute_metrics(&board_dir, &members).unwrap();

        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.blocked_count, 1);
        assert_eq!(metrics.in_review_count, 1);
        assert_eq!(metrics.in_progress_count, 1);
        assert_eq!(metrics.idle_with_runnable, vec!["eng-3"]);
        assert!(metrics.oldest_review_age_secs.is_some());
        assert!(metrics.oldest_assignment_age_secs.is_some());
    }

    #[test]
    fn format_metrics_produces_readable_summary() {
        let text = format_metrics(&WorkflowMetrics {
            runnable_count: 2,
            blocked_count: 1,
            in_review_count: 3,
            in_progress_count: 4,
            stale_in_progress_count: 1,
            aged_todo_count: 2,
            stale_review_count: 3,
            idle_with_runnable: vec!["eng-1".to_string(), "eng-2".to_string()],
            oldest_review_age_secs: Some(120),
            oldest_assignment_age_secs: Some(360),
            ..Default::default()
        });

        assert!(text.contains("Workflow Metrics"));
        assert!(text.contains("Runnable: 2"));
        assert!(text.contains("Blocked: 1"));
        assert!(text.contains("In Review: 3"));
        assert!(text.contains("In Progress: 4"));
        assert!(text.contains("Aging Alerts: stale in-progress 1 | aged todo 2 | stale review 3"));
        assert!(text.contains("Idle With Runnable: eng-1, eng-2"));
        assert!(text.contains("Oldest Review Age: 120s"));
        assert!(text.contains("Oldest Assignment Age: 360s"));
        assert!(text.contains("Review Pipeline"));
    }

    fn write_events(path: &Path, events: &[crate::team::events::TeamEvent]) {
        let mut lines = Vec::new();
        for event in events {
            lines.push(serde_json::to_string(event).unwrap());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, lines.join("\n")).unwrap();
    }

    #[test]
    fn review_metrics_count_events() {
        use crate::team::events::TeamEvent;

        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let events_path = tmp.path().join("events.jsonl");
        write_task(&board_dir, 1, "t1", "done", None, None, &[]);

        write_events(
            &events_path,
            &[
                TeamEvent::task_auto_merged("eng-1", "1", 0.9, 2, 30),
                TeamEvent::task_auto_merged("eng-1", "2", 0.9, 2, 30),
                TeamEvent::task_auto_merged("eng-1", "3", 0.9, 2, 30),
                TeamEvent::task_manual_merged("4"),
                TeamEvent::task_manual_merged("5"),
                TeamEvent::task_reworked("eng-1", "6"),
                TeamEvent::review_nudge_sent("manager", "7"),
                TeamEvent::review_escalated_by_role("manager", "8"),
                TeamEvent::review_escalated_by_role("manager", "9"),
            ],
        );

        let metrics = compute_metrics_with_events(&board_dir, &[], Some(&events_path)).unwrap();

        assert_eq!(metrics.auto_merge_count, 3);
        assert_eq!(metrics.manual_merge_count, 2);
        assert_eq!(metrics.rework_count, 1);
        assert_eq!(metrics.review_nudge_count, 1);
        assert_eq!(metrics.review_escalation_count, 2);

        // auto_merge_rate = 3 / (3 + 2) = 0.6
        let rate = metrics.auto_merge_rate.unwrap();
        assert!((rate - 0.6).abs() < 0.01);

        // rework_rate = 1 / (5 + 1) ≈ 0.167
        let rework = metrics.rework_rate.unwrap();
        assert!((rework - 1.0 / 6.0).abs() < 0.01);
    }

    #[test]
    fn review_metrics_compute_latency() {
        use crate::team::events::TeamEvent;

        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let events_path = tmp.path().join("events.jsonl");
        write_task(&board_dir, 1, "t1", "done", None, None, &[]);

        // task_completed marks review entry, task_auto/manual_merged marks exit
        let mut e1 = TeamEvent::task_completed("eng-1", Some("10"));
        e1.ts = 1000;
        let mut e2 = TeamEvent::task_auto_merged("eng-1", "10", 0.9, 2, 30);
        e2.ts = 1100; // 100s latency

        let mut e3 = TeamEvent::task_completed("eng-2", Some("20"));
        e3.ts = 2000;
        let mut e4 = TeamEvent::task_manual_merged("20");
        e4.ts = 2300; // 300s latency

        write_events(&events_path, &[e1, e2, e3, e4]);

        let metrics = compute_metrics_with_events(&board_dir, &[], Some(&events_path)).unwrap();

        // avg = (100 + 300) / 2 = 200
        let avg = metrics.avg_review_latency_secs.unwrap();
        assert!((avg - 200.0).abs() < 0.01);
    }

    #[test]
    fn review_metrics_handle_no_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let events_path = tmp.path().join("events.jsonl");
        write_task(&board_dir, 1, "t1", "done", None, None, &[]);

        // Empty event file — no merge events
        std::fs::write(&events_path, "").unwrap();

        let metrics = compute_metrics_with_events(&board_dir, &[], Some(&events_path)).unwrap();

        assert_eq!(metrics.auto_merge_count, 0);
        assert_eq!(metrics.manual_merge_count, 0);
        assert!(metrics.auto_merge_rate.is_none());
        assert!(metrics.rework_rate.is_none());
        assert!(metrics.avg_review_latency_secs.is_none());
    }

    #[test]
    fn status_includes_review_pipeline() {
        let text = format_metrics(&WorkflowMetrics {
            in_review_count: 2,
            auto_merge_count: 3,
            manual_merge_count: 2,
            auto_merge_rate: Some(0.6),
            rework_count: 1,
            rework_rate: Some(1.0 / 6.0),
            review_nudge_count: 1,
            review_escalation_count: 0,
            avg_review_latency_secs: Some(272.0),
            ..Default::default()
        });

        assert!(text.contains("Review Pipeline"));
        assert!(text.contains("Queue: 2"));
        assert!(text.contains("Auto-merge Rate: 60%"));
        assert!(text.contains("Auto: 3"));
        assert!(text.contains("Manual: 2"));
        assert!(text.contains("Rework: 1"));
        assert!(text.contains("Nudges: 1"));
        assert!(text.contains("Escalations: 0"));
    }

    #[test]
    fn retro_includes_review_section() {
        use crate::team::retrospective::{RunStats, generate_retrospective};

        let tmp = tempfile::tempdir().unwrap();
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
            auto_merge_count: 5,
            manual_merge_count: 2,
            rework_count: 1,
            review_nudge_count: 3,
            review_escalation_count: 0,
            avg_review_stall_secs: Some(120),
            max_review_stall_secs: Some(200),
            max_review_stall_task: Some("T-1".to_string()),
            task_rework_counts: vec![("T-2".to_string(), 1)],
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = std::fs::read_to_string(path).unwrap();

        assert!(content.contains("## Review Pipeline"));
        assert!(content.contains("Auto-merged: 5"));
        assert!(content.contains("Manually merged: 2"));
        assert!(content.contains("Auto-merge rate: 71%"));
        assert!(content.contains("Rework cycles: 1"));
        assert!(content.contains("Review nudges: 3"));
        assert!(content.contains("Review escalations: 0"));
        assert!(content.contains("Avg review stall: 2m 00s"));
        assert!(content.contains("Max review stall: 3m 20s (T-1)"));
    }

    #[test]
    fn compute_cycle_time_from_mock_timestamps() {
        let offset = FixedOffset::west_opt(4 * 3600).unwrap();
        let created_at = offset.with_ymd_and_hms(2026, 4, 5, 10, 0, 0).unwrap();
        let started_at = offset.with_ymd_and_hms(2026, 4, 5, 11, 0, 0).unwrap();
        let completed_at = offset.with_ymd_and_hms(2026, 4, 5, 13, 30, 0).unwrap();

        let record = build_task_cycle_time_record(
            473,
            "Track cycle time",
            Some("eng-1-3"),
            "high",
            "done",
            Some(created_at),
            Some(started_at),
            Some(completed_at),
        );

        assert_eq!(record.cycle_time_minutes, Some(150));
    }

    #[test]
    fn compute_lead_time_from_mock_timestamps() {
        let offset = FixedOffset::west_opt(4 * 3600).unwrap();
        let created_at = offset.with_ymd_and_hms(2026, 4, 5, 9, 0, 0).unwrap();
        let started_at = offset.with_ymd_and_hms(2026, 4, 5, 11, 0, 0).unwrap();
        let completed_at = offset.with_ymd_and_hms(2026, 4, 5, 13, 30, 0).unwrap();

        let record = build_task_cycle_time_record(
            474,
            "Track lead time",
            Some("eng-1-4"),
            "medium",
            "done",
            Some(created_at),
            Some(started_at),
            Some(completed_at),
        );

        assert_eq!(record.lead_time_minutes, Some(270));
    }

    #[test]
    fn cycle_time_metrics_aggregation_groups_priority_and_engineer() {
        let records = vec![
            TaskCycleTimeRecord {
                task_id: 1,
                title: "One".to_string(),
                engineer: Some("eng-1".to_string()),
                priority: "high".to_string(),
                status: "done".to_string(),
                created_at: Some(100),
                started_at: Some(160),
                completed_at: Some(460),
                cycle_time_minutes: Some(5),
                lead_time_minutes: Some(6),
            },
            TaskCycleTimeRecord {
                task_id: 2,
                title: "Two".to_string(),
                engineer: Some("eng-1".to_string()),
                priority: "high".to_string(),
                status: "done".to_string(),
                created_at: Some(200),
                started_at: Some(260),
                completed_at: Some(860),
                cycle_time_minutes: Some(10),
                lead_time_minutes: Some(11),
            },
            TaskCycleTimeRecord {
                task_id: 3,
                title: "Three".to_string(),
                engineer: Some("eng-2".to_string()),
                priority: "low".to_string(),
                status: "done".to_string(),
                created_at: Some(300),
                started_at: Some(360),
                completed_at: Some(540),
                cycle_time_minutes: Some(3),
                lead_time_minutes: Some(4),
            },
        ];

        let by_priority = average_cycle_time_by_priority(&records);
        assert_eq!(by_priority.len(), 2);
        assert_eq!(by_priority[0].priority, "high");
        assert!((by_priority[0].average_cycle_time_minutes - 7.5).abs() < f64::EPSILON);

        let by_engineer = engineer_throughput_ranking(&records);
        assert_eq!(by_engineer[0].engineer, "eng-1");
        assert_eq!(by_engineer[0].completed_tasks, 2);
        assert!((by_engineer[0].average_cycle_time_minutes.unwrap() - 7.5).abs() < f64::EPSILON);
    }

    #[test]
    fn cycle_time_edge_cases_handle_missing_started_and_created() {
        let no_started = TaskCycleTimeRecord {
            task_id: 10,
            title: "No started".to_string(),
            engineer: Some("eng-1".to_string()),
            priority: "medium".to_string(),
            status: "done".to_string(),
            created_at: Some(100),
            started_at: None,
            completed_at: Some(400),
            cycle_time_minutes: None,
            lead_time_minutes: Some(5),
        };
        let no_created = TaskCycleTimeRecord {
            task_id: 11,
            title: "No created".to_string(),
            engineer: Some("eng-2".to_string()),
            priority: "medium".to_string(),
            status: "done".to_string(),
            created_at: None,
            started_at: Some(100),
            completed_at: Some(400),
            cycle_time_minutes: Some(5),
            lead_time_minutes: None,
        };

        let now = Utc.timestamp_opt(7200, 0).single().unwrap();
        let in_progress = longest_running_in_progress_tasks(
            &[TaskCycleTimeRecord {
                task_id: 12,
                title: "Active".to_string(),
                engineer: Some("eng-3".to_string()),
                priority: "high".to_string(),
                status: "in-progress".to_string(),
                created_at: Some(100),
                started_at: Some(3600),
                completed_at: None,
                cycle_time_minutes: None,
                lead_time_minutes: None,
            }],
            now,
            5,
        );

        assert!(no_started.cycle_time_minutes.is_none());
        assert!(no_created.lead_time_minutes.is_none());
        assert_eq!(in_progress[0].minutes_in_progress, 60);
    }
}
