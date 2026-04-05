//! SQLite-backed telemetry database for agent performance tracking.
//!
//! Stores events, per-agent metrics, per-task metrics, and session summaries
//! in `.batty/telemetry.db`. All tables use `CREATE TABLE IF NOT EXISTS` —
//! no migration framework needed.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::events::TeamEvent;

/// Database file name under `.batty/`.
const DB_FILENAME: &str = "telemetry.db";

/// Open or create the telemetry database, initializing the schema.
pub fn open(project_root: &Path) -> Result<Connection> {
    let db_path = project_root.join(".batty").join(DB_FILENAME);
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open telemetry db at {}", db_path.display()))?;

    // WAL mode for better concurrent read/write performance.
    conn.pragma_update(None, "journal_mode", "WAL")?;

    init_schema(&conn)?;
    Ok(conn)
}

/// Open an in-memory database (for tests).
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp   INTEGER NOT NULL,
            event_type  TEXT NOT NULL,
            role        TEXT,
            task_id     TEXT,
            payload     TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_events_ts ON events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
        CREATE INDEX IF NOT EXISTS idx_events_role ON events(role);

        CREATE TABLE IF NOT EXISTS agent_metrics (
            role            TEXT PRIMARY KEY,
            completions     INTEGER NOT NULL DEFAULT 0,
            failures        INTEGER NOT NULL DEFAULT 0,
            restarts        INTEGER NOT NULL DEFAULT 0,
            total_cycle_secs INTEGER NOT NULL DEFAULT 0,
            idle_polls      INTEGER NOT NULL DEFAULT 0,
            working_polls   INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS task_metrics (
            task_id          TEXT PRIMARY KEY,
            started_at       INTEGER,
            completed_at     INTEGER,
            retries          INTEGER NOT NULL DEFAULT 0,
            narration_rejections INTEGER NOT NULL DEFAULT 0,
            escalations      INTEGER NOT NULL DEFAULT 0,
            merge_time_secs  INTEGER,
            confidence_score REAL
        );

        CREATE TABLE IF NOT EXISTS session_summary (
            session_id      TEXT PRIMARY KEY,
            started_at      INTEGER NOT NULL,
            ended_at        INTEGER,
            tasks_completed INTEGER NOT NULL DEFAULT 0,
            total_merges    INTEGER NOT NULL DEFAULT 0,
            total_events    INTEGER NOT NULL DEFAULT 0
        );
        ",
    )
    .context("failed to initialize telemetry schema")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Insert helpers
// ---------------------------------------------------------------------------

/// Insert a raw event into the events table. Also updates derived metrics.
pub fn insert_event(conn: &Connection, event: &TeamEvent) -> Result<()> {
    let payload =
        serde_json::to_string(event).context("failed to serialize event for telemetry")?;

    conn.execute(
        "INSERT INTO events (timestamp, event_type, role, task_id, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event.ts as i64,
            event.event,
            event.role,
            event.task,
            payload,
        ],
    )
    .context("failed to insert telemetry event")?;

    // Update derived metrics based on event type (may create session row).
    update_metrics_for_event(conn, event)?;

    // Fix #3: Increment total_events on every insert (after update_metrics
    // so that daemon_started can create the session row first).
    conn.execute(
        "UPDATE session_summary SET total_events = total_events + 1
         WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
        [],
    )?;

    Ok(())
}

fn update_metrics_for_event(conn: &Connection, event: &TeamEvent) -> Result<()> {
    match event.event.as_str() {
        "task_completed" => {
            if let Some(role) = &event.role {
                upsert_agent_counter(conn, role, "completions")?;
            }
            if let Some(task) = &event.task {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, completed_at) VALUES (?1, ?2)
                     ON CONFLICT(task_id) DO UPDATE SET completed_at = ?2",
                    params![task, event.ts as i64],
                )?;
            }
            // Fix #1: Increment tasks_completed on latest session.
            conn.execute(
                "UPDATE session_summary SET tasks_completed = tasks_completed + 1
                 WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                [],
            )?;
        }
        "task_assigned" => {
            if let Some(task) = &event.task {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, started_at) VALUES (?1, ?2)
                     ON CONFLICT(task_id) DO UPDATE SET started_at = COALESCE(task_metrics.started_at, ?2)",
                    params![task, event.ts as i64],
                )?;
            }
        }
        "task_escalated" => {
            if let Some(task) = &event.task {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, escalations) VALUES (?1, 1)
                     ON CONFLICT(task_id) DO UPDATE SET escalations = escalations + 1",
                    params![task],
                )?;
            }
        }
        "narration_rejection" => {
            if let Some(task) = &event.task {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, narration_rejections) VALUES (?1, 1)
                     ON CONFLICT(task_id) DO UPDATE SET narration_rejections = narration_rejections + 1",
                    params![task],
                )?;
            }
        }
        "member_crashed" | "pane_death" | "delivery_failed" => {
            if let Some(role) = &event.role {
                upsert_agent_counter(conn, role, "failures")?;
            }
        }
        "pane_respawned" | "agent_restarted" | "context_exhausted" => {
            if let Some(role) = &event.role {
                upsert_agent_counter(conn, role, "restarts")?;
            }
        }
        "task_auto_merged" | "task_manual_merged" => {
            if let Some(task) = &event.task {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, merge_time_secs) VALUES (?1, ?2)
                     ON CONFLICT(task_id) DO UPDATE SET merge_time_secs = ?2 - COALESCE(task_metrics.started_at, ?2)",
                    params![task, event.ts as i64],
                )?;
            }
            // Fix #2: Increment total_merges on latest session.
            conn.execute(
                "UPDATE session_summary SET total_merges = total_merges + 1
                 WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                [],
            )?;
        }
        "merge_confidence_scored" => {
            if let Some(task) = &event.task {
                if let Some(confidence) = event.load {
                    conn.execute(
                        "INSERT INTO task_metrics (task_id, confidence_score) VALUES (?1, ?2)
                         ON CONFLICT(task_id) DO UPDATE SET confidence_score = ?2",
                        params![task, confidence],
                    )?;
                }
            }
        }
        "daemon_started" => {
            let session_id = format!("session-{}", event.ts);
            conn.execute(
                "INSERT OR IGNORE INTO session_summary (session_id, started_at) VALUES (?1, ?2)",
                params![session_id, event.ts as i64],
            )?;
        }
        // Fix #4: Set ended_at on latest session when daemon stops.
        // Both daemon_stopped() and daemon_stopped_with_reason() use "daemon_stopped" as event name.
        "daemon_stopped" => {
            conn.execute(
                "UPDATE session_summary SET ended_at = ?1
                 WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                params![event.ts as i64],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn upsert_agent_counter(conn: &Connection, role: &str, column: &str) -> Result<()> {
    // column is a known static string, safe to interpolate.
    let sql = format!(
        "INSERT INTO agent_metrics (role, {column}) VALUES (?1, 1)
         ON CONFLICT(role) DO UPDATE SET {column} = {column} + 1"
    );
    conn.execute(&sql, params![role])?;
    Ok(())
}

/// Record an agent's poll state (idle or working) and accumulate cycle time.
///
/// Fix #5: Upserts idle_polls or working_polls for the given role.
/// Fix #6: Increments total_cycle_secs by `poll_interval_secs` when working.
pub fn record_agent_poll_state(
    conn: &Connection,
    role: &str,
    is_working: bool,
    poll_interval_secs: u64,
) -> Result<()> {
    if is_working {
        conn.execute(
            "INSERT INTO agent_metrics (role, working_polls, total_cycle_secs)
             VALUES (?1, 1, ?2)
             ON CONFLICT(role) DO UPDATE SET
                working_polls = working_polls + 1,
                total_cycle_secs = total_cycle_secs + ?2",
            params![role, poll_interval_secs as i64],
        )?;
    } else {
        conn.execute(
            "INSERT INTO agent_metrics (role, idle_polls) VALUES (?1, 1)
             ON CONFLICT(role) DO UPDATE SET idle_polls = idle_polls + 1",
            params![role],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Summary row for `batty telemetry summary`.
#[derive(Debug, Clone)]
pub struct SessionSummaryRow {
    pub session_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub tasks_completed: i64,
    pub total_merges: i64,
    pub total_events: i64,
}

pub fn query_session_summaries(conn: &Connection) -> Result<Vec<SessionSummaryRow>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, started_at, ended_at, tasks_completed, total_merges, total_events
         FROM session_summary ORDER BY started_at DESC LIMIT 20",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionSummaryRow {
                session_id: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
                tasks_completed: row.get(3)?,
                total_merges: row.get(4)?,
                total_events: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Agent metrics row for `batty telemetry agents`.
#[derive(Debug, Clone)]
pub struct AgentMetricsRow {
    pub role: String,
    pub completions: i64,
    pub failures: i64,
    pub restarts: i64,
    pub total_cycle_secs: i64,
    pub idle_polls: i64,
    pub working_polls: i64,
}

pub fn query_agent_metrics(conn: &Connection) -> Result<Vec<AgentMetricsRow>> {
    let mut stmt = conn.prepare(
        "SELECT role, completions, failures, restarts, total_cycle_secs, idle_polls, working_polls
         FROM agent_metrics ORDER BY role",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AgentMetricsRow {
                role: row.get(0)?,
                completions: row.get(1)?,
                failures: row.get(2)?,
                restarts: row.get(3)?,
                total_cycle_secs: row.get(4)?,
                idle_polls: row.get(5)?,
                working_polls: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Task metrics row for `batty telemetry tasks`.
#[derive(Debug, Clone)]
pub struct TaskMetricsRow {
    pub task_id: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub retries: i64,
    pub narration_rejections: i64,
    pub escalations: i64,
    pub merge_time_secs: Option<i64>,
    pub confidence_score: Option<f64>,
}

pub fn query_task_metrics(conn: &Connection) -> Result<Vec<TaskMetricsRow>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, started_at, completed_at, retries, narration_rejections, escalations, merge_time_secs, confidence_score
         FROM task_metrics ORDER BY started_at DESC NULLS LAST LIMIT 50",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TaskMetricsRow {
                task_id: row.get(0)?,
                started_at: row.get(1)?,
                completed_at: row.get(2)?,
                retries: row.get(3)?,
                narration_rejections: row.get(4)?,
                escalations: row.get(5)?,
                merge_time_secs: row.get(6)?,
                confidence_score: row.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Recent events row for `batty telemetry events`.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub timestamp: i64,
    pub event_type: String,
    pub role: Option<String>,
    pub task_id: Option<String>,
}

pub fn query_recent_events(conn: &Connection, limit: usize) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, event_type, role, task_id
         FROM events ORDER BY timestamp DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok(EventRow {
                timestamp: row.get(0)?,
                event_type: row.get(1)?,
                role: row.get(2)?,
                task_id: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Review pipeline metrics aggregated from the events table.
#[derive(Debug, Clone)]
pub struct ReviewMetricsRow {
    pub auto_merge_count: i64,
    pub manual_merge_count: i64,
    pub rework_count: i64,
    pub review_nudge_count: i64,
    pub review_escalation_count: i64,
    pub avg_review_latency_secs: Option<f64>,
}

/// Query aggregated review pipeline metrics from the events table.
pub fn query_review_metrics(conn: &Connection) -> Result<ReviewMetricsRow> {
    let count_event = |event_type: &str| -> Result<i64> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = ?1",
            params![event_type],
            |row| row.get(0),
        )?;
        Ok(n)
    };

    let auto_merge_count = count_event("task_auto_merged")?;
    let manual_merge_count = count_event("task_manual_merged")?;
    let rework_count = count_event("task_reworked")?;
    let review_nudge_count = count_event("review_nudge_sent")?;
    let review_escalation_count = count_event("review_escalated")?;

    // Compute average review latency: time between task_completed and its
    // corresponding merge event for each task.
    let avg_review_latency_secs: Option<f64> = conn
        .query_row(
            "SELECT AVG(m.timestamp - c.timestamp)
             FROM events c
             JOIN events m ON c.task_id = m.task_id
               AND m.event_type IN ('task_auto_merged', 'task_manual_merged')
             WHERE c.event_type = 'task_completed'
               AND c.task_id IS NOT NULL
               AND m.timestamp >= c.timestamp",
            [],
            |row| row.get(0),
        )
        .unwrap_or(None);

    Ok(ReviewMetricsRow {
        auto_merge_count,
        manual_merge_count,
        rework_count,
        review_nudge_count,
        review_escalation_count,
        avg_review_latency_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::events::TeamEvent;

    #[test]
    fn schema_creation_succeeds() {
        let conn = open_in_memory().unwrap();
        // Verify tables exist by querying them.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn idempotent_schema_creation() {
        let conn = open_in_memory().unwrap();
        // Running init_schema again should not fail.
        init_schema(&conn).unwrap();
    }

    #[test]
    fn insert_and_query_event_round_trip() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::daemon_started();
        insert_event(&conn, &event).unwrap();

        let events = query_recent_events(&conn, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "daemon_started");
    }

    #[test]
    fn task_assigned_creates_task_metric() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::task_assigned("eng-1", "42");
        insert_event(&conn, &event).unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
        assert!(tasks[0].started_at.is_some());
    }

    #[test]
    fn task_completed_updates_agent_and_task_metrics() {
        let conn = open_in_memory().unwrap();

        let assign = TeamEvent::task_assigned("eng-1", "42");
        insert_event(&conn, &assign).unwrap();

        let complete = TeamEvent::task_completed("eng-1", Some("42"));
        insert_event(&conn, &complete).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].role, "eng-1");
        assert_eq!(agents[0].completions, 1);

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].completed_at.is_some());
    }

    #[test]
    fn escalation_increments_task_escalations() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::task_escalated("eng-1", "42", None);
        insert_event(&conn, &event).unwrap();
        insert_event(&conn, &event).unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks[0].escalations, 2);
    }

    #[test]
    fn narration_rejection_increments_task_metric() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::narration_rejection("eng-1", 42, 1)).unwrap();
        insert_event(&conn, &TeamEvent::narration_rejection("eng-1", 42, 2)).unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
        assert_eq!(tasks[0].narration_rejections, 2);
    }

    #[test]
    fn pane_death_increments_failures() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::pane_death("eng-1");
        insert_event(&conn, &event).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].failures, 1);
    }

    #[test]
    fn pane_respawned_increments_restarts() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::pane_respawned("eng-1");
        insert_event(&conn, &event).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].restarts, 1);
    }

    #[test]
    fn delivery_failed_increments_failures() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::delivery_failed("eng-1", "manager", "pane not ready");
        insert_event(&conn, &event).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].role, "eng-1");
        assert_eq!(agents[0].failures, 1);
    }

    #[test]
    fn context_exhausted_increments_restarts() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::context_exhausted("eng-1", Some(42), Some(500_000));
        insert_event(&conn, &event).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].role, "eng-1");
        assert_eq!(agents[0].restarts, 1);
    }

    #[test]
    fn all_failure_event_types_accumulate() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::pane_death("eng-1")).unwrap();
        insert_event(&conn, &TeamEvent::member_crashed("eng-1", true)).unwrap();
        insert_event(
            &conn,
            &TeamEvent::delivery_failed("eng-1", "manager", "timeout"),
        )
        .unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].failures, 3);
    }

    #[test]
    fn all_restart_event_types_accumulate() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::pane_respawned("eng-1")).unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "stall", 1),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::context_exhausted("eng-1", Some(42), None),
        )
        .unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].restarts, 3);
    }

    #[test]
    fn daemon_started_creates_session_summary() {
        let conn = open_in_memory().unwrap();
        let event = TeamEvent::daemon_started();
        insert_event(&conn, &event).unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].session_id.starts_with("session-"));
    }

    #[test]
    fn multiple_events_for_same_agent_accumulate() {
        let conn = open_in_memory().unwrap();
        let c1 = TeamEvent::task_completed("eng-1", Some("1"));
        let c2 = TeamEvent::task_completed("eng-1", Some("2"));
        insert_event(&conn, &c1).unwrap();
        insert_event(&conn, &c2).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].completions, 2);
    }

    #[test]
    fn query_recent_events_respects_limit() {
        let conn = open_in_memory().unwrap();
        for _ in 0..5 {
            insert_event(&conn, &TeamEvent::daemon_started()).unwrap();
        }
        let events = query_recent_events(&conn, 3).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn concurrent_writes_to_same_connection() {
        // rusqlite Connection is not Send/Sync, but we verify sequential
        // writes to the same connection work without errors.
        let conn = open_in_memory().unwrap();
        for i in 0..50 {
            let event = TeamEvent::task_assigned("eng-1", &i.to_string());
            insert_event(&conn, &event).unwrap();
        }
        let events = query_recent_events(&conn, 100).unwrap();
        assert_eq!(events.len(), 50);
    }

    #[test]
    fn review_metrics_empty_db() {
        let conn = open_in_memory().unwrap();
        let row = query_review_metrics(&conn).unwrap();
        assert_eq!(row.auto_merge_count, 0);
        assert_eq!(row.manual_merge_count, 0);
        assert_eq!(row.rework_count, 0);
        assert_eq!(row.review_nudge_count, 0);
        assert_eq!(row.review_escalation_count, 0);
        assert!(row.avg_review_latency_secs.is_none());
    }

    #[test]
    fn review_metrics_counts_all_event_types() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "1", 0.9, 2, 30),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "2", 0.9, 2, 30),
        )
        .unwrap();
        insert_event(&conn, &TeamEvent::task_manual_merged("3")).unwrap();
        insert_event(&conn, &TeamEvent::task_reworked("eng-1", "4")).unwrap();
        insert_event(&conn, &TeamEvent::review_nudge_sent("manager", "5")).unwrap();
        insert_event(&conn, &TeamEvent::review_nudge_sent("manager", "6")).unwrap();
        insert_event(&conn, &TeamEvent::review_escalated_by_role("manager", "7")).unwrap();

        let row = query_review_metrics(&conn).unwrap();
        assert_eq!(row.auto_merge_count, 2);
        assert_eq!(row.manual_merge_count, 1);
        assert_eq!(row.rework_count, 1);
        assert_eq!(row.review_nudge_count, 2);
        assert_eq!(row.review_escalation_count, 1);
    }

    #[test]
    fn review_metrics_computes_avg_latency() {
        let conn = open_in_memory().unwrap();

        // Task 10: completed at ts=1000, merged at ts=1100 → 100s latency
        let mut c1 = TeamEvent::task_completed("eng-1", Some("10"));
        c1.ts = 1000;
        insert_event(&conn, &c1).unwrap();
        let mut m1 = TeamEvent::task_auto_merged("eng-1", "10", 0.9, 2, 30);
        m1.ts = 1100;
        insert_event(&conn, &m1).unwrap();

        // Task 20: completed at ts=2000, merged at ts=2300 → 300s latency
        let mut c2 = TeamEvent::task_completed("eng-2", Some("20"));
        c2.ts = 2000;
        insert_event(&conn, &c2).unwrap();
        let mut m2 = TeamEvent::task_manual_merged("20");
        m2.ts = 2300;
        insert_event(&conn, &m2).unwrap();

        let row = query_review_metrics(&conn).unwrap();
        // avg = (100 + 300) / 2 = 200
        let avg = row.avg_review_latency_secs.unwrap();
        assert!((avg - 200.0).abs() < 0.01);
    }

    // --- Fix #1: tasks_completed incremented on task_completed ---

    #[test]
    fn tasks_completed_increments_on_task_completed() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();

        insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("1"))).unwrap();
        insert_event(&conn, &TeamEvent::task_completed("eng-2", Some("2"))).unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries[0].tasks_completed, 2);
    }

    // --- Fix #2: total_merges incremented on merge events ---

    #[test]
    fn total_merges_increments_on_auto_and_manual_merge() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();

        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "1", 0.9, 2, 30),
        )
        .unwrap();
        insert_event(&conn, &TeamEvent::task_manual_merged("2")).unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "3", 0.8, 1, 10),
        )
        .unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries[0].total_merges, 3);
    }

    // --- Fix #3: total_events incremented on every insert ---

    #[test]
    fn total_events_increments_on_every_insert() {
        let conn = open_in_memory().unwrap();
        // daemon_started is the first event, creating the session and then incrementing.
        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();
        insert_event(&conn, &TeamEvent::task_assigned("eng-1", "1")).unwrap();
        insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("1"))).unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        // 3 events inserted after session was created (daemon_started creates the row
        // then total_events is incremented for it too).
        assert_eq!(summaries[0].total_events, 3);
    }

    // --- Fix #4: ended_at set on daemon_stopped ---

    #[test]
    fn ended_at_set_on_daemon_stopped() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert!(summaries[0].ended_at.is_none());

        let mut stop = TeamEvent::daemon_stopped_with_reason("shutdown", 3600);
        stop.ts = 9999;
        insert_event(&conn, &stop).unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries[0].ended_at, Some(9999));
    }

    #[test]
    fn ended_at_set_on_plain_daemon_stopped() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();

        let mut stop = TeamEvent::daemon_stopped();
        stop.ts = 5000;
        insert_event(&conn, &stop).unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries[0].ended_at, Some(5000));
    }

    // --- Fix #5: idle_polls / working_polls updated ---

    #[test]
    fn record_agent_poll_state_tracks_idle_polls() {
        let conn = open_in_memory().unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].idle_polls, 3);
        assert_eq!(agents[0].working_polls, 0);
        assert_eq!(agents[0].total_cycle_secs, 0);
    }

    #[test]
    fn record_agent_poll_state_tracks_working_polls() {
        let conn = open_in_memory().unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].working_polls, 2);
        assert_eq!(agents[0].idle_polls, 0);
    }

    // --- Fix #6: total_cycle_secs incremented for working agents ---

    #[test]
    fn record_agent_poll_state_accumulates_cycle_secs_for_working() {
        let conn = open_in_memory().unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].total_cycle_secs, 15); // 3 * 5
    }

    #[test]
    fn record_agent_poll_state_idle_does_not_accumulate_cycle_secs() {
        let conn = open_in_memory().unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].total_cycle_secs, 0);
    }

    #[test]
    fn record_agent_poll_state_mixed_idle_and_working() {
        let conn = open_in_memory().unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].working_polls, 2);
        assert_eq!(agents[0].idle_polls, 2);
        assert_eq!(agents[0].total_cycle_secs, 10); // 2 * 5
    }

    #[test]
    fn record_agent_poll_state_multiple_agents() {
        let conn = open_in_memory().unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-2", false, 5).unwrap();
        record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        record_agent_poll_state(&conn, "eng-2", true, 5).unwrap();

        let agents = query_agent_metrics(&conn).unwrap();
        let eng1 = agents.iter().find(|a| a.role == "eng-1").unwrap();
        let eng2 = agents.iter().find(|a| a.role == "eng-2").unwrap();
        assert_eq!(eng1.working_polls, 2);
        assert_eq!(eng1.total_cycle_secs, 10);
        assert_eq!(eng2.idle_polls, 1);
        assert_eq!(eng2.working_polls, 1);
        assert_eq!(eng2.total_cycle_secs, 5);
    }

    // --- Edge cases: session counters without a session ---

    #[test]
    fn session_counters_noop_without_session() {
        // If no daemon_started event has been emitted, no session row exists.
        // The UPDATE statements should just affect 0 rows — no error.
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("1"))).unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "1", 0.9, 2, 30),
        )
        .unwrap();
        let summaries = query_session_summaries(&conn).unwrap();
        assert!(summaries.is_empty());
    }
}
