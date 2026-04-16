//! Task time estimation from telemetry — median cycle time by tag set.
//!
//! Queries completed task durations from the telemetry database, groups them
//! by tag set (from the board), and computes median cycle times. For in-progress
//! tasks, finds the best-matching tag set to estimate remaining time.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use tracing::warn;

/// A completed task's cycle time (seconds) and associated tags.
#[derive(Debug, Clone)]
pub(crate) struct CompletedTaskSample {
    pub duration_secs: u64,
    pub tags: Vec<String>,
}

/// Estimated time for a single in-progress task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskEstimate {
    /// Estimated remaining seconds and total estimated seconds.
    Remaining {
        remaining_secs: i64,
        total_secs: u64,
    },
    /// No historical data to estimate from.
    NoData,
}

/// Load completed task cycle times from the telemetry database.
///
/// Returns tasks that have both `started_at` and `completed_at` set.
pub(crate) fn load_completed_samples(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, completed_at - started_at
         FROM task_metrics
         WHERE started_at IS NOT NULL AND completed_at IS NOT NULL
           AND completed_at > started_at",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let task_id: String = row.get(0)?;
            let duration: i64 = row.get(1)?;
            Ok((task_id, duration as u64))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Build completed task samples by joining telemetry durations with board tags.
///
/// `tag_map` maps task ID strings to their tag lists (from the board).
pub(crate) fn build_samples(
    durations: &[(String, u64)],
    tag_map: &HashMap<String, Vec<String>>,
) -> Vec<CompletedTaskSample> {
    durations
        .iter()
        .map(|(task_id, duration)| CompletedTaskSample {
            duration_secs: *duration,
            tags: tag_map.get(task_id).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Compute median cycle time (seconds) for each unique tag set.
///
/// Tags are sorted and joined with "," as the key. Tasks with no tags
/// use the key `""` (empty string).
pub(crate) fn median_by_tag_set(samples: &[CompletedTaskSample]) -> HashMap<String, u64> {
    let mut grouped: HashMap<String, Vec<u64>> = HashMap::new();
    for sample in samples {
        let key = tag_set_key(&sample.tags);
        grouped.entry(key).or_default().push(sample.duration_secs);
    }

    let mut result = HashMap::new();
    for (key, mut durations) in grouped {
        durations.sort_unstable();
        let median = compute_median(&durations);
        result.insert(key, median);
    }
    result
}

/// Compute the global median across all samples (fallback when no tag match).
pub(crate) fn global_median(samples: &[CompletedTaskSample]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut durations: Vec<u64> = samples.iter().map(|s| s.duration_secs).collect();
    durations.sort_unstable();
    Some(compute_median(&durations))
}

/// Estimate remaining time for an in-progress task.
///
/// Strategy: find median for exact tag set match, then fall back to global median.
/// `elapsed_secs` is how long the task has been running.
pub(crate) fn estimate_task(
    task_tags: &[String],
    elapsed_secs: u64,
    medians: &HashMap<String, u64>,
    fallback_median: Option<u64>,
) -> TaskEstimate {
    let key = tag_set_key(task_tags);

    // Try exact tag set match first.
    if let Some(&median) = medians.get(&key) {
        return TaskEstimate::Remaining {
            remaining_secs: median as i64 - elapsed_secs as i64,
            total_secs: median,
        };
    }

    // Fall back to global median.
    if let Some(median) = fallback_median {
        return TaskEstimate::Remaining {
            remaining_secs: median as i64 - elapsed_secs as i64,
            total_secs: median,
        };
    }

    TaskEstimate::NoData
}

/// Format a task estimate for display in the status table.
pub(crate) fn format_estimate(estimate: &TaskEstimate) -> String {
    match estimate {
        TaskEstimate::NoData => "n/a".to_string(),
        TaskEstimate::Remaining {
            remaining_secs,
            total_secs: _,
        } => {
            if *remaining_secs < 0 {
                let overdue = (-remaining_secs) as u64;
                format!("overdue +{}", format_duration(overdue))
            } else {
                format!("~{}", format_duration(*remaining_secs as u64))
            }
        }
    }
}

/// Build a tag-set-key map from loaded board tasks.
pub(crate) fn build_tag_map(project_root: &Path) -> HashMap<String, Vec<String>> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return HashMap::new();
    }

    let tasks = match crate::task::load_tasks_from_dir(&tasks_dir) {
        Ok(tasks) => tasks,
        Err(error) => {
            warn!(error = %error, "failed to load board tasks for estimation");
            return HashMap::new();
        }
    };

    tasks
        .into_iter()
        .map(|task| (task.id.to_string(), task.tags))
        .collect()
}

/// Compute ETA strings for a set of in-progress tasks.
///
/// Returns a map from task_id to formatted ETA string.
pub(crate) fn compute_etas(
    project_root: &Path,
    active_task_ids: &[(u32, u64)], // (task_id, elapsed_secs)
) -> HashMap<u32, String> {
    if active_task_ids.is_empty() {
        return HashMap::new();
    }

    let conn = match super::telemetry_db::open_readonly(project_root) {
        Ok(Some(conn)) => conn,
        Ok(None) | Err(_) => {
            return active_task_ids
                .iter()
                .map(|(id, _)| (*id, "n/a".to_string()))
                .collect();
        }
    };

    let durations = match load_completed_samples(&conn) {
        Ok(d) => d,
        Err(error) => {
            warn!(error = %error, "failed to load completed samples for estimation");
            return active_task_ids
                .iter()
                .map(|(id, _)| (*id, "n/a".to_string()))
                .collect();
        }
    };

    let tag_map = build_tag_map(project_root);
    let samples = build_samples(&durations, &tag_map);
    let medians = median_by_tag_set(&samples);
    let fallback = global_median(&samples);

    active_task_ids
        .iter()
        .map(|(task_id, elapsed)| {
            let tags = tag_map
                .get(&task_id.to_string())
                .cloned()
                .unwrap_or_default();
            let estimate = estimate_task(&tags, *elapsed, &medians, fallback);
            (*task_id, format_estimate(&estimate))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn tag_set_key(tags: &[String]) -> String {
    let mut sorted = tags.to_vec();
    sorted.sort();
    sorted.join(",")
}

fn compute_median(sorted: &[u64]) -> u64 {
    let len = sorted.len();
    if len == 0 {
        return 0;
    }
    if len % 2 == 0 {
        (sorted[len / 2 - 1] + sorted[len / 2]) / 2
    } else {
        sorted[len / 2]
    }
}

fn format_duration(secs: u64) -> String {
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
            format!("{hours}h{mins}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_count() {
        assert_eq!(compute_median(&[100, 200, 300]), 200);
    }

    #[test]
    fn median_of_even_count() {
        assert_eq!(compute_median(&[100, 200, 300, 400]), 250);
    }

    #[test]
    fn median_of_single() {
        assert_eq!(compute_median(&[42]), 42);
    }

    #[test]
    fn median_of_empty() {
        assert_eq!(compute_median(&[]), 0);
    }

    #[test]
    fn tag_set_key_sorts() {
        let tags = vec!["feature".into(), "daemon".into(), "bugfix".into()];
        assert_eq!(tag_set_key(&tags), "bugfix,daemon,feature");
    }

    #[test]
    fn tag_set_key_empty() {
        assert_eq!(tag_set_key(&[]), "");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(45), "45s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(300), "5m");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration(5400), "1h30m");
    }

    #[test]
    fn format_duration_exact_hours() {
        assert_eq!(format_duration(7200), "2h");
    }

    #[test]
    fn format_estimate_no_data() {
        assert_eq!(format_estimate(&TaskEstimate::NoData), "n/a");
    }

    #[test]
    fn format_estimate_remaining() {
        let est = TaskEstimate::Remaining {
            remaining_secs: 1800,
            total_secs: 3600,
        };
        assert_eq!(format_estimate(&est), "~30m");
    }

    #[test]
    fn format_estimate_overdue() {
        let est = TaskEstimate::Remaining {
            remaining_secs: -600,
            total_secs: 3600,
        };
        assert_eq!(format_estimate(&est), "overdue +10m");
    }

    #[test]
    fn estimate_task_exact_tag_match() {
        let mut medians = HashMap::new();
        medians.insert("bugfix,daemon".to_string(), 3600);
        let tags = vec!["daemon".to_string(), "bugfix".to_string()];
        let result = estimate_task(&tags, 1800, &medians, Some(7200));
        assert_eq!(
            result,
            TaskEstimate::Remaining {
                remaining_secs: 1800,
                total_secs: 3600,
            }
        );
    }

    #[test]
    fn estimate_task_falls_back_to_global() {
        let medians = HashMap::new(); // no match
        let tags = vec!["newfeature".to_string()];
        let result = estimate_task(&tags, 300, &medians, Some(1800));
        assert_eq!(
            result,
            TaskEstimate::Remaining {
                remaining_secs: 1500,
                total_secs: 1800,
            }
        );
    }

    #[test]
    fn estimate_task_no_data() {
        let medians = HashMap::new();
        let tags = vec!["newfeature".to_string()];
        let result = estimate_task(&tags, 300, &medians, None);
        assert_eq!(result, TaskEstimate::NoData);
    }

    #[test]
    fn estimate_task_overdue() {
        let mut medians = HashMap::new();
        medians.insert("bugfix".to_string(), 1000);
        let tags = vec!["bugfix".to_string()];
        let result = estimate_task(&tags, 2000, &medians, None);
        assert_eq!(
            result,
            TaskEstimate::Remaining {
                remaining_secs: -1000,
                total_secs: 1000,
            }
        );
    }

    #[test]
    fn build_samples_joins_tags() {
        let durations = vec![
            ("1".to_string(), 100),
            ("2".to_string(), 200),
            ("3".to_string(), 300),
        ];
        let mut tag_map = HashMap::new();
        tag_map.insert("1".to_string(), vec!["bugfix".into()]);
        tag_map.insert("2".to_string(), vec!["feature".into(), "daemon".into()]);
        // task 3 has no tags in the map

        let samples = build_samples(&durations, &tag_map);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].tags, vec!["bugfix"]);
        assert_eq!(samples[1].tags, vec!["feature", "daemon"]);
        assert!(samples[2].tags.is_empty());
    }

    #[test]
    fn median_by_tag_set_groups_correctly() {
        let samples = vec![
            CompletedTaskSample {
                duration_secs: 100,
                tags: vec!["bugfix".into()],
            },
            CompletedTaskSample {
                duration_secs: 300,
                tags: vec!["bugfix".into()],
            },
            CompletedTaskSample {
                duration_secs: 200,
                tags: vec!["bugfix".into()],
            },
            CompletedTaskSample {
                duration_secs: 1000,
                tags: vec!["feature".into()],
            },
        ];
        let medians = median_by_tag_set(&samples);
        assert_eq!(medians["bugfix"], 200); // median of [100, 200, 300]
        assert_eq!(medians["feature"], 1000); // single value
    }

    #[test]
    fn global_median_across_all_samples() {
        let samples = vec![
            CompletedTaskSample {
                duration_secs: 100,
                tags: vec![],
            },
            CompletedTaskSample {
                duration_secs: 500,
                tags: vec![],
            },
            CompletedTaskSample {
                duration_secs: 300,
                tags: vec![],
            },
        ];
        assert_eq!(global_median(&samples), Some(300));
    }

    #[test]
    fn global_median_empty() {
        assert_eq!(global_median(&[]), None);
    }

    #[test]
    fn load_completed_samples_from_telemetry() {
        let conn = super::super::telemetry_db::open_in_memory().unwrap();

        // Insert task_assigned and task_completed events to populate task_metrics.
        let mut assign = crate::team::events::TeamEvent::task_assigned("eng-1", "10");
        assign.ts = 1000;
        super::super::telemetry_db::insert_event(&conn, &assign).unwrap();

        let mut complete = crate::team::events::TeamEvent::task_completed("eng-1", Some("10"));
        complete.ts = 1600;
        super::super::telemetry_db::insert_event(&conn, &complete).unwrap();

        let samples = load_completed_samples(&conn).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].0, "10");
        assert_eq!(samples[0].1, 600);
    }

    #[test]
    fn load_completed_samples_skips_incomplete() {
        let conn = super::super::telemetry_db::open_in_memory().unwrap();

        // Only assigned, never completed.
        let assign = crate::team::events::TeamEvent::task_assigned("eng-1", "11");
        super::super::telemetry_db::insert_event(&conn, &assign).unwrap();

        let samples = load_completed_samples(&conn).unwrap();
        assert!(samples.is_empty());
    }

    #[test]
    fn format_estimate_zero_remaining() {
        let est = TaskEstimate::Remaining {
            remaining_secs: 0,
            total_secs: 3600,
        };
        // Exactly on time — show ~0s.
        assert_eq!(format_estimate(&est), "~0s");
    }

    #[test]
    fn median_by_tag_set_empty_tags_use_empty_key() {
        let samples = vec![
            CompletedTaskSample {
                duration_secs: 500,
                tags: vec![],
            },
            CompletedTaskSample {
                duration_secs: 700,
                tags: vec![],
            },
        ];
        let medians = median_by_tag_set(&samples);
        assert_eq!(medians[""], 600); // median of [500, 700]
    }
}
