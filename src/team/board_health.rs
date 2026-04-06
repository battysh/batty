//! Board health dashboard — task counts, age, blocked chains, throughput.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::events::read_events;
use crate::task::{Task, load_tasks_from_dir};

/// Per-status statistics for the board health dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusStats {
    pub status: String,
    pub count: usize,
    pub avg_age_hours: f64,
}

/// Full board health snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardHealth {
    pub status_stats: Vec<StatusStats>,
    pub total_tasks: usize,
    pub max_blocked_chain: usize,
    pub review_queue_age_hours: f64,
    pub throughput_per_hour: f64,
}

/// Ordered list of statuses for display.
const STATUSES: &[&str] = &[
    "backlog",
    "todo",
    "in-progress",
    "review",
    "blocked",
    "done",
];

/// Compute board health from task files and event log.
pub fn compute_health(board_dir: &Path, events_path: &Path) -> Result<BoardHealth> {
    let tasks_dir = board_dir.join("tasks");
    let tasks = if tasks_dir.is_dir() {
        load_tasks_from_dir(&tasks_dir)?
    } else {
        Vec::new()
    };

    let now = Utc::now();
    let total_tasks = tasks.len();

    // Group tasks by status.
    let mut by_status: HashMap<String, Vec<&Task>> = HashMap::new();
    for task in &tasks {
        by_status.entry(task.status.clone()).or_default().push(task);
    }

    // Compute per-status counts and average age.
    let mut status_stats = Vec::new();
    for &status in STATUSES {
        let group = by_status.get(status);
        let count = group.map_or(0, |g| g.len());
        let avg_age_hours = if count == 0 {
            0.0
        } else {
            let total_hours: f64 = group.unwrap().iter().map(|t| task_age_hours(t, now)).sum();
            total_hours / count as f64
        };
        status_stats.push(StatusStats {
            status: status.to_string(),
            count,
            avg_age_hours,
        });
    }

    // Blocked chain depth: walk depends_on links to find the longest chain.
    let max_blocked_chain = compute_max_chain_depth(&tasks);

    // Review queue age: average age of tasks in "review" status.
    let review_queue_age_hours = status_stats
        .iter()
        .find(|s| s.status == "review")
        .map_or(0.0, |s| s.avg_age_hours);

    // Throughput: task_completed events in last hour.
    let throughput_per_hour = compute_throughput(events_path, now)?;

    Ok(BoardHealth {
        status_stats,
        total_tasks,
        max_blocked_chain,
        review_queue_age_hours,
        throughput_per_hour,
    })
}

/// Format the board health as a human-readable table.
pub fn format_health(health: &BoardHealth) -> String {
    let mut out = String::new();

    out.push_str("Board Health Dashboard\n");
    out.push_str("======================\n\n");

    // Status table.
    out.push_str(&format!(
        "{:<14} {:>5} {:>10}\n",
        "STATUS", "COUNT", "AVG AGE"
    ));
    out.push_str(&format!("{:-<14} {:->5} {:->10}\n", "", "", ""));
    for stat in &health.status_stats {
        let age_display = format_age(stat.avg_age_hours);
        out.push_str(&format!(
            "{:<14} {:>5} {:>10}\n",
            stat.status, stat.count, age_display
        ));
    }
    out.push_str(&format!("{:-<14} {:->5} {:->10}\n", "", "", ""));
    out.push_str(&format!("{:<14} {:>5}\n", "TOTAL", health.total_tasks));

    out.push('\n');

    // Metrics.
    out.push_str(&format!(
        "Blocked chain depth:  {}\n",
        health.max_blocked_chain
    ));
    out.push_str(&format!(
        "Review queue age:     {}\n",
        format_age(health.review_queue_age_hours)
    ));
    out.push_str(&format!(
        "Throughput (1h):      {:.1} tasks/hour\n",
        health.throughput_per_hour
    ));

    out
}

/// Compute age of a task in hours using filesystem mtime as fallback.
fn task_age_hours(task: &Task, now: DateTime<Utc>) -> f64 {
    let mtime = std::fs::metadata(&task.source_path)
        .and_then(|m| m.modified())
        .ok();

    match mtime {
        Some(mtime) => {
            let mtime_dt: DateTime<Utc> = mtime.into();
            let age = now.signed_duration_since(mtime_dt);
            age.num_minutes().max(0) as f64 / 60.0
        }
        None => 0.0,
    }
}

/// Walk depends_on links to find the longest dependency chain.
pub fn compute_max_chain_depth(tasks: &[Task]) -> usize {
    let id_to_deps: HashMap<u32, &[u32]> = tasks
        .iter()
        .map(|t| (t.id, t.depends_on.as_slice()))
        .collect();

    let mut max_depth = 0;
    let mut memo: HashMap<u32, usize> = HashMap::new();

    for task in tasks {
        let depth = chain_depth(task.id, &id_to_deps, &mut memo, &mut Vec::new());
        if depth > max_depth {
            max_depth = depth;
        }
    }

    max_depth
}

fn chain_depth(
    id: u32,
    id_to_deps: &HashMap<u32, &[u32]>,
    memo: &mut HashMap<u32, usize>,
    visiting: &mut Vec<u32>,
) -> usize {
    if let Some(&cached) = memo.get(&id) {
        return cached;
    }

    // Cycle detection.
    if visiting.contains(&id) {
        return 0;
    }

    let deps = match id_to_deps.get(&id) {
        Some(deps) => *deps,
        None => return 0,
    };

    if deps.is_empty() {
        memo.insert(id, 0);
        return 0;
    }

    visiting.push(id);
    let max_child = deps
        .iter()
        .map(|&dep| chain_depth(dep, id_to_deps, memo, visiting))
        .max()
        .unwrap_or(0);
    visiting.pop();

    let depth = 1 + max_child;
    memo.insert(id, depth);
    depth
}

/// Count task_completed events in the last hour.
fn compute_throughput(events_path: &Path, now: DateTime<Utc>) -> Result<f64> {
    let events = read_events(events_path)?;
    let one_hour_ago = now.timestamp() as u64 - 3600;

    let completed_count = events
        .iter()
        .filter(|e| e.event == "task_completed" && e.ts >= one_hour_ago)
        .count();

    Ok(completed_count as f64)
}

/// Format hours into a human-readable string.
fn format_age(hours: f64) -> String {
    if hours < 1.0 {
        format!("{:.0}m", hours * 60.0)
    } else if hours < 24.0 {
        format!("{:.1}h", hours)
    } else {
        let days = hours / 24.0;
        format!("{:.1}d", days)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn make_task(id: u32, status: &str, depends_on: Vec<u32>) -> Task {
        Task {
            id,
            title: format!("Task {id}"),
            status: status.to_string(),
            priority: "medium".to_string(),
            claimed_by: None,
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: Vec::new(),
            depends_on,
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: String::new(),
            batty_config: None,
            source_path: PathBuf::from("/tmp/fake.md"),
        }
    }

    fn write_task_file(dir: &Path, id: u32, status: &str, depends_on: &[u32]) {
        let deps_str = if depends_on.is_empty() {
            String::new()
        } else {
            let items: Vec<String> = depends_on.iter().map(|d| format!("  - {d}")).collect();
            format!("depends_on:\n{}\n", items.join("\n"))
        };
        let content = format!(
            "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: medium\n{deps_str}---\n\nTask body\n"
        );
        fs::write(dir.join(format!("{id:04}.md")), content).unwrap();
    }

    fn write_events(path: &Path, events: &[(&str, u64)]) {
        let lines: Vec<String> = events
            .iter()
            .map(|(event, ts)| format!(r#"{{"event":"{event}","ts":{ts}}}"#))
            .collect();
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn count_by_status_correct() {
        let tmp = tempdir().unwrap();
        let board_dir = tmp.path().to_path_buf();
        let tasks_dir = board_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(&tasks_dir, 1, "backlog", &[]);
        write_task_file(&tasks_dir, 2, "in-progress", &[]);
        write_task_file(&tasks_dir, 3, "in-progress", &[]);
        write_task_file(&tasks_dir, 4, "done", &[]);
        write_task_file(&tasks_dir, 5, "review", &[]);

        let events_path = board_dir.join("events.jsonl");
        fs::write(&events_path, "").unwrap();

        let health = compute_health(&board_dir, &events_path).unwrap();

        assert_eq!(health.total_tasks, 5);

        let find = |s: &str| {
            health
                .status_stats
                .iter()
                .find(|st| st.status == s)
                .unwrap()
        };
        assert_eq!(find("backlog").count, 1);
        assert_eq!(find("in-progress").count, 2);
        assert_eq!(find("done").count, 1);
        assert_eq!(find("review").count, 1);
        assert_eq!(find("todo").count, 0);
        assert_eq!(find("blocked").count, 0);
    }

    #[test]
    fn average_age_calculation() {
        let tmp = tempdir().unwrap();
        let board_dir = tmp.path().to_path_buf();
        let tasks_dir = board_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(&tasks_dir, 1, "in-progress", &[]);
        write_task_file(&tasks_dir, 2, "in-progress", &[]);

        let events_path = board_dir.join("events.jsonl");
        fs::write(&events_path, "").unwrap();

        let health = compute_health(&board_dir, &events_path).unwrap();

        let in_progress = health
            .status_stats
            .iter()
            .find(|s| s.status == "in-progress")
            .unwrap();
        assert!(
            in_progress.avg_age_hours < 1.0,
            "expected age < 1h, got {:.2}h",
            in_progress.avg_age_hours
        );
    }

    #[test]
    fn blocked_chain_depth_linear() {
        // Chain: 4 -> 3 -> 2 -> 1 (depth 3 from task 4).
        let tasks = vec![
            make_task(1, "backlog", vec![]),
            make_task(2, "backlog", vec![1]),
            make_task(3, "backlog", vec![2]),
            make_task(4, "backlog", vec![3]),
        ];

        assert_eq!(compute_max_chain_depth(&tasks), 3);
    }

    #[test]
    fn blocked_chain_with_cycle() {
        let tasks = vec![
            make_task(1, "backlog", vec![3]),
            make_task(2, "backlog", vec![1]),
            make_task(3, "backlog", vec![2]),
        ];

        let depth = compute_max_chain_depth(&tasks);
        assert!(depth <= 3);
    }

    #[test]
    fn throughput_from_events() {
        let tmp = tempdir().unwrap();
        let events_path = tmp.path().join("events.jsonl");
        let now = Utc::now();
        let now_ts = now.timestamp() as u64;

        write_events(
            &events_path,
            &[
                ("task_completed", now_ts - 100),
                ("task_completed", now_ts - 200),
                ("task_completed", now_ts - 300),
                ("task_completed", now_ts - 7200), // 2 hours ago
                ("daemon_started", now_ts - 50),   // Not task_completed
            ],
        );

        let throughput = compute_throughput(&events_path, now).unwrap();
        assert!((throughput - 3.0).abs() < 0.01);
    }

    #[test]
    fn handles_empty_board() {
        let tmp = tempdir().unwrap();
        let board_dir = tmp.path().to_path_buf();

        let events_path = board_dir.join("events.jsonl");
        fs::write(&events_path, "").unwrap();

        let health = compute_health(&board_dir, &events_path).unwrap();
        assert_eq!(health.total_tasks, 0);
        assert_eq!(health.max_blocked_chain, 0);
        assert!((health.throughput_per_hour - 0.0).abs() < 0.01);

        for stat in &health.status_stats {
            assert_eq!(stat.count, 0);
            assert!((stat.avg_age_hours - 0.0).abs() < 0.01);
        }
    }

    #[test]
    fn handles_missing_events_file() {
        let tmp = tempdir().unwrap();
        let board_dir = tmp.path().to_path_buf();
        let tasks_dir = board_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(&tasks_dir, 1, "todo", &[]);

        let events_path = board_dir.join("nonexistent.jsonl");

        let health = compute_health(&board_dir, &events_path).unwrap();
        assert_eq!(health.total_tasks, 1);
        assert!((health.throughput_per_hour - 0.0).abs() < 0.01);
    }

    #[test]
    fn format_health_output() {
        let health = BoardHealth {
            status_stats: vec![
                StatusStats {
                    status: "backlog".to_string(),
                    count: 5,
                    avg_age_hours: 48.0,
                },
                StatusStats {
                    status: "in-progress".to_string(),
                    count: 3,
                    avg_age_hours: 2.5,
                },
                StatusStats {
                    status: "review".to_string(),
                    count: 1,
                    avg_age_hours: 0.5,
                },
            ],
            total_tasks: 9,
            max_blocked_chain: 2,
            review_queue_age_hours: 0.5,
            throughput_per_hour: 4.0,
        };

        let output = format_health(&health);
        assert!(output.contains("Board Health Dashboard"));
        assert!(output.contains("backlog"));
        assert!(output.contains("in-progress"));
        assert!(output.contains("TOTAL"));
        assert!(output.contains("9"));
        assert!(output.contains("Blocked chain depth:  2"));
        assert!(output.contains("Throughput (1h):      4.0 tasks/hour"));
    }

    #[test]
    fn format_age_displays_correctly() {
        assert_eq!(format_age(0.0), "0m");
        assert_eq!(format_age(0.5), "30m");
        assert_eq!(format_age(2.5), "2.5h");
        assert_eq!(format_age(48.0), "2.0d");
    }

    #[test]
    fn chain_depth_no_deps() {
        let tasks = vec![make_task(1, "todo", vec![]), make_task(2, "todo", vec![])];
        assert_eq!(compute_max_chain_depth(&tasks), 0);
    }

    #[test]
    fn chain_depth_diamond() {
        // Diamond: 4 -> {2, 3} -> 1.
        let tasks = vec![
            make_task(1, "todo", vec![]),
            make_task(2, "todo", vec![1]),
            make_task(3, "todo", vec![1]),
            make_task(4, "todo", vec![2, 3]),
        ];
        assert_eq!(compute_max_chain_depth(&tasks), 2);
    }
}
