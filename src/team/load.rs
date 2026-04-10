//! Team load monitoring and historical load graphing.

use std::path::Path;

use anyhow::{Result, bail};
use tracing::warn;

use super::{config, events, hierarchy, now_unix, status, team_config_path, team_events_path};
use crate::tmux;

/// Default duration window for load graph rendering, in seconds (1 hour).
const LOAD_GRAPH_WINDOW_SECONDS: u64 = 3_600;
const LOAD_GRAPH_WIDTH: usize = 30;

#[derive(Debug, Clone, Copy)]
pub struct TeamLoadSnapshot {
    pub timestamp: u64,
    pub total_members: usize,
    pub working_members: usize,
    pub load: f64,
    pub session_running: bool,
}

/// Show an estimated team load value from live state, store it, and show recent load trends.
pub fn show_load(project_root: &Path) -> Result<()> {
    let current = capture_team_load(project_root)?;
    if let Err(error) = log_team_load_snapshot(project_root, &current) {
        warn!(error = %error, "failed to append load snapshot to team event log");
    }

    let mut history = read_team_load_history(project_root)?;
    history.push(current);
    history.sort_by_key(|snapshot| snapshot.timestamp);

    println!(
        "Current load: {:.1}% ({} / {} members working)",
        current.load * 100.0,
        current.working_members,
        current.total_members.max(1)
    );
    println!(
        "Session: {}",
        if current.session_running {
            "running"
        } else {
            "stopped"
        }
    );

    if let Some(avg) = average_load(&history, current.timestamp, 10 * 60) {
        println!("10m avg: {:.1}%", avg * 100.0);
    } else {
        println!("10m avg: n/a");
    }
    println!(
        "30m avg: {}",
        average_load(&history, current.timestamp, 30 * 60)
            .map(|avg| format!("{:.1}%", avg * 100.0))
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "60m avg: {}",
        average_load(&history, current.timestamp, 60 * 60)
            .map(|avg| format!("{:.1}%", avg * 100.0))
            .unwrap_or_else(|| "n/a".to_string())
    );

    println!("Load graph (1h):");
    println!("{}", render_load_graph(&history, current.timestamp));
    Ok(())
}

fn capture_team_load(project_root: &Path) -> Result<TeamLoadSnapshot> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = config::TeamConfig::load(&config_path)?;
    let members = hierarchy::resolve_hierarchy(&team_config)?;
    let session = format!("batty-{}", team_config.name);
    let session_running = tmux::session_exists(&session);
    let runtime_statuses = if session_running {
        match status::list_runtime_member_statuses(&session) {
            Ok(statuses) => statuses,
            Err(error) => {
                warn!(session = %session, error = %error, "failed to read runtime statuses for load sampling");
                std::collections::HashMap::new()
            }
        }
    } else {
        std::collections::HashMap::new()
    };

    let triage_backlog_counts = status::triage_backlog_counts(project_root, &members);
    let owned_task_buckets = status::owned_task_buckets(project_root, &members);
    let branch_mismatches = status::branch_mismatch_by_member(project_root, &members);
    let rows = status::build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &Default::default(),
        &triage_backlog_counts,
        &owned_task_buckets,
        &branch_mismatches,
        &Default::default(),
        &Default::default(),
    );
    let mut total_members = 0usize;
    let mut working_members = 0usize;

    for row in &rows {
        if row.role_type == "User" {
            continue;
        }
        total_members += 1;
        if counts_as_active_load(row) {
            working_members += 1;
        }
    }

    let load = if total_members == 0 {
        0.0
    } else {
        working_members as f64 / total_members as f64
    };

    Ok(TeamLoadSnapshot {
        timestamp: now_unix(),
        total_members,
        working_members: working_members.min(total_members),
        load,
        session_running,
    })
}

fn counts_as_active_load(row: &status::TeamStatusRow) -> bool {
    matches!(row.state.as_str(), "working" | "triaging" | "reviewing")
}

fn log_team_load_snapshot(project_root: &Path, snapshot: &TeamLoadSnapshot) -> Result<()> {
    let events_path = team_events_path(project_root);
    let mut sink = events::EventSink::new(&events_path)?;
    let event = events::TeamEvent::load_snapshot(
        snapshot.working_members as u32,
        snapshot.total_members as u32,
        snapshot.session_running,
    );
    sink.emit(event)?;
    Ok(())
}

fn read_team_load_history(project_root: &Path) -> Result<Vec<TeamLoadSnapshot>> {
    let events_path = team_events_path(project_root);
    let events = events::read_events(&events_path)?;
    let mut history = Vec::new();
    for event in events {
        if event.event != "load_snapshot" {
            continue;
        }
        let Some(load) = event.load else {
            continue;
        };
        let Some(working_members) = event.working_members else {
            continue;
        };
        let Some(total_members) = event.total_members else {
            continue;
        };

        history.push(TeamLoadSnapshot {
            timestamp: event.ts,
            total_members: total_members as usize,
            working_members: working_members as usize,
            load,
            session_running: event.session_running.unwrap_or(false),
        });
    }
    Ok(history)
}

fn average_load(samples: &[TeamLoadSnapshot], now: u64, window_seconds: u64) -> Option<f64> {
    let cutoff = now.saturating_sub(window_seconds);
    let mut values = Vec::new();
    for sample in samples {
        if sample.timestamp >= cutoff && sample.timestamp <= now {
            values.push(sample.load);
        }
    }
    if values.is_empty() {
        return None;
    }
    let sum: f64 = values.iter().copied().sum();
    Some(sum / values.len() as f64)
}

fn render_load_graph(samples: &[TeamLoadSnapshot], now: u64) -> String {
    if samples.is_empty() {
        return "(no historical load data yet)".to_string();
    }

    let bucket_size = (LOAD_GRAPH_WINDOW_SECONDS / LOAD_GRAPH_WIDTH as u64).max(1);
    let window_start = now.saturating_sub(LOAD_GRAPH_WINDOW_SECONDS);
    let mut history = String::new();
    let mut previous = 0.0;
    for index in 0..LOAD_GRAPH_WIDTH {
        let bucket_start = window_start + (index as u64 * bucket_size);
        let bucket_end = if index + 1 == LOAD_GRAPH_WIDTH {
            now + 1
        } else {
            bucket_start + bucket_size
        };

        let mut sum = 0.0;
        let mut count = 0usize;
        for sample in samples {
            if sample.timestamp >= bucket_start && sample.timestamp < bucket_end {
                sum += sample.load;
                count += 1;
            }
        }

        let value = if count == 0 {
            previous
        } else {
            sum / count as f64
        };
        previous = value;
        history.push(load_point_char(value));
    }

    history
}

fn load_point_char(value: f64) -> char {
    let clamped = value.clamp(0.0, 1.0);
    match (clamped * 5.0).round() as usize {
        0 => ' ',
        1 => '.',
        2 => ':',
        3 => '=',
        4 => '#',
        _ => '@',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_as_active_load_treats_triaging_as_working() {
        let triaging = status::TeamStatusRow {
            name: "lead".to_string(),
            role: "lead".to_string(),
            role_type: "Manager".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("architect".to_string()),
            state: "triaging".to_string(),
            pending_inbox: 0,
            triage_backlog: 2,
            active_owned_tasks: vec![191],
            review_owned_tasks: vec![193],
            signal: Some("needs triage (2)".to_string()),
            runtime_label: Some("idle".to_string()),
            worktree_staleness: None,
            health: status::AgentHealthSummary::default(),
            health_summary: "-".to_string(),
            eta: "-".to_string(),
        };
        let reviewing = status::TeamStatusRow {
            state: "reviewing".to_string(),
            triage_backlog: 0,
            signal: Some("needs review (1)".to_string()),
            runtime_label: Some("idle".to_string()),
            ..triaging.clone()
        };
        let idle = status::TeamStatusRow {
            state: "idle".to_string(),
            triage_backlog: 0,
            signal: None,
            runtime_label: Some("idle".to_string()),
            ..triaging.clone()
        };

        assert!(counts_as_active_load(&triaging));
        assert!(counts_as_active_load(&reviewing));
        assert!(!counts_as_active_load(&idle));
    }

    #[test]
    fn average_load_ignores_points_older_than_window() {
        let now = 10_000u64;
        let samples = vec![
            TeamLoadSnapshot {
                timestamp: now - 3_000,
                total_members: 10,
                working_members: 0,
                load: 0.8,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 10,
                total_members: 10,
                working_members: 0,
                load: 0.4,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 20,
                total_members: 10,
                working_members: 0,
                load: 0.6,
                session_running: true,
            },
        ];

        let avg_60s = average_load(&samples, now, 60).unwrap();
        assert!((avg_60s - 0.5).abs() < 0.0001);
        assert!(average_load(&samples, now, 5).is_none());
    }

    #[test]
    fn render_load_graph_returns_expected_width() {
        let now = 10_000u64;
        let samples = vec![
            TeamLoadSnapshot {
                timestamp: now - 3_600,
                total_members: 10,
                working_members: 2,
                load: 0.2,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 1_800,
                total_members: 10,
                working_members: 5,
                load: 0.5,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 900,
                total_members: 10,
                working_members: 10,
                load: 1.0,
                session_running: true,
            },
            TeamLoadSnapshot {
                timestamp: now - 600,
                total_members: 10,
                working_members: 0,
                load: 0.0,
                session_running: true,
            },
        ];

        let graph = render_load_graph(&samples, now);
        assert_eq!(graph.len(), LOAD_GRAPH_WIDTH);
        assert!(graph.chars().all(|c| " .:=#@".contains(c)));
    }
}
