//! SQLite-backed telemetry database for agent performance tracking.
//!
//! Stores events, per-agent metrics, per-task metrics, and session summaries
//! in `.batty/telemetry.db`. All tables use `CREATE TABLE IF NOT EXISTS` —
//! no migration framework needed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::events::TeamEvent;
use super::metrics::TaskCycleTimeRecord;
use super::test_results::{TestFailure, TestResults};

/// Database file name under `.batty/`.
const DB_FILENAME: &str = "telemetry.db";

struct SchemaColumn {
    name: &'static str,
    definition: &'static str,
}

const TASK_METRICS_COLUMNS: &[SchemaColumn] = &[
    SchemaColumn {
        name: "started_at",
        definition: "started_at INTEGER",
    },
    SchemaColumn {
        name: "completed_at",
        definition: "completed_at INTEGER",
    },
    SchemaColumn {
        name: "retries",
        definition: "retries INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "narration_rejections",
        definition: "narration_rejections INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "escalations",
        definition: "escalations INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "context_restart_count",
        definition: "context_restart_count INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "handoff_attempts",
        definition: "handoff_attempts INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "handoff_successes",
        definition: "handoff_successes INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "carry_forward_effective",
        definition: "carry_forward_effective INTEGER",
    },
    SchemaColumn {
        name: "merge_time_secs",
        definition: "merge_time_secs INTEGER",
    },
    SchemaColumn {
        name: "confidence_score",
        definition: "confidence_score REAL",
    },
    SchemaColumn {
        name: "orphan_reconciliation_branch_mismatch_count",
        definition: "orphan_reconciliation_branch_mismatch_count INTEGER NOT NULL DEFAULT 0",
    },
];

const SESSION_SUMMARY_COLUMNS: &[SchemaColumn] = &[
    SchemaColumn {
        name: "started_at",
        definition: "started_at INTEGER NOT NULL",
    },
    SchemaColumn {
        name: "ended_at",
        definition: "ended_at INTEGER",
    },
    SchemaColumn {
        name: "tasks_completed",
        definition: "tasks_completed INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "total_merges",
        definition: "total_merges INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "total_events",
        definition: "total_events INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "discord_events_sent",
        definition: "discord_events_sent INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "verification_passes",
        definition: "verification_passes INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "verification_failures",
        definition: "verification_failures INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "notification_isolations",
        definition: "notification_isolations INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "notification_latency_total_secs",
        definition: "notification_latency_total_secs INTEGER NOT NULL DEFAULT 0",
    },
    SchemaColumn {
        name: "notification_latency_samples",
        definition: "notification_latency_samples INTEGER NOT NULL DEFAULT 0",
    },
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct SchemaRepairReport {
    repaired_columns: BTreeMap<String, Vec<String>>,
}

impl SchemaRepairReport {
    fn record(&mut self, table: &str, column: &str) {
        self.repaired_columns
            .entry(table.to_string())
            .or_default()
            .push(column.to_string());
    }

    fn is_empty(&self) -> bool {
        self.repaired_columns.is_empty()
    }
}

/// Open or create the telemetry database, initializing the schema.
pub fn open(project_root: &Path) -> Result<Connection> {
    let db_path = project_root.join(".batty").join(DB_FILENAME);
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open telemetry db at {}", db_path.display()))?;

    // WAL mode for better concurrent read/write performance.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Prevent indefinite blocking when the daemon holds a write lock (#676).
    conn.pragma_update(None, "busy_timeout", "5000")?;

    init_schema(&conn)?;
    Ok(conn)
}

/// Open the telemetry database in read-only mode for CLI queries (#676).
///
/// Skips schema initialization (which requires a write lock) to avoid
/// blocking when the daemon is actively writing. Returns `None` if the
/// database file doesn't exist yet.
pub fn open_readonly(project_root: &Path) -> Result<Option<Connection>> {
    let db_path = project_root.join(".batty").join(DB_FILENAME);
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "failed to open telemetry db (read-only) at {}",
            db_path.display()
        )
    })?;
    // Short busy timeout — CLI should never block on a daemon write lock.
    conn.pragma_update(None, "busy_timeout", "2000")?;
    Ok(Some(conn))
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
            context_restart_count INTEGER NOT NULL DEFAULT 0,
            handoff_attempts INTEGER NOT NULL DEFAULT 0,
            handoff_successes INTEGER NOT NULL DEFAULT 0,
            carry_forward_effective INTEGER,
            merge_time_secs  INTEGER,
            confidence_score REAL,
            orphan_reconciliation_branch_mismatch_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS session_summary (
            session_id      TEXT PRIMARY KEY,
            started_at      INTEGER NOT NULL,
            ended_at        INTEGER,
            tasks_completed INTEGER NOT NULL DEFAULT 0,
            total_merges    INTEGER NOT NULL DEFAULT 0,
            total_events    INTEGER NOT NULL DEFAULT 0,
            discord_events_sent INTEGER NOT NULL DEFAULT 0,
            verification_passes INTEGER NOT NULL DEFAULT 0,
            verification_failures INTEGER NOT NULL DEFAULT 0,
            notification_isolations INTEGER NOT NULL DEFAULT 0,
            notification_latency_total_secs INTEGER NOT NULL DEFAULT 0,
            notification_latency_samples INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS test_case_metrics (
            framework       TEXT NOT NULL,
            test_name       TEXT NOT NULL,
            failures        INTEGER NOT NULL DEFAULT 0,
            flaky_passes    INTEGER NOT NULL DEFAULT 0,
            last_task_id    TEXT,
            last_engineer   TEXT,
            last_seen_at    INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (framework, test_name)
        );

        CREATE TABLE IF NOT EXISTS task_cycle_times (
            task_id          TEXT PRIMARY KEY,
            engineer         TEXT,
            priority         TEXT NOT NULL,
            cycle_time_mins  INTEGER,
            lead_time_mins   INTEGER,
            completed_at     INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS non_engineer_stall_metrics (
            role            TEXT NOT NULL,
            lane            TEXT NOT NULL,
            signal          TEXT NOT NULL,
            count           INTEGER NOT NULL DEFAULT 0,
            last_seen_at    INTEGER NOT NULL,
            max_stall_secs  INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (role, signal)
        );
        ",
    )
    .context("failed to initialize telemetry schema")?;
    let repairs = repair_legacy_schema(conn)?;
    if !repairs.is_empty() {
        record_schema_repair_event(conn, &repairs)?;
    }
    Ok(())
}

fn repair_legacy_schema(conn: &Connection) -> Result<SchemaRepairReport> {
    let mut repairs = SchemaRepairReport::default();
    ensure_table_columns(conn, "task_metrics", TASK_METRICS_COLUMNS, &mut repairs)?;
    ensure_table_columns(
        conn,
        "session_summary",
        SESSION_SUMMARY_COLUMNS,
        &mut repairs,
    )?;
    Ok(repairs)
}

fn ensure_table_columns(
    conn: &Connection,
    table: &str,
    columns: &[SchemaColumn],
    repairs: &mut SchemaRepairReport,
) -> Result<()> {
    let mut existing = query_table_columns(conn, table)?;
    for column in columns {
        if existing.contains(column.name) {
            continue;
        }
        let sql = format!("ALTER TABLE {table} ADD COLUMN {}", column.definition);
        conn.execute(&sql, []).with_context(|| {
            format!(
                "failed to add {}.{} to telemetry schema",
                table, column.name
            )
        })?;
        existing.insert(column.name.to_string());
        repairs.record(table, column.name);
    }
    Ok(())
}

fn query_table_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("failed to inspect telemetry schema for table {table}"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    Ok(columns)
}

fn record_schema_repair_event(conn: &Connection, repairs: &SchemaRepairReport) -> Result<()> {
    let payload = serde_json::to_string(repairs).context("failed to serialize schema repair")?;
    conn.execute(
        "INSERT INTO events (timestamp, event_type, role, task_id, payload)
         VALUES (?1, ?2, NULL, NULL, ?3)",
        params![
            chrono::Utc::now().timestamp(),
            "telemetry_schema_repaired",
            payload
        ],
    )
    .context("failed to record telemetry schema repair event")?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn install_legacy_schema_for_tests(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS events;
        DROP TABLE IF EXISTS agent_metrics;
        DROP TABLE IF EXISTS task_metrics;
        DROP TABLE IF EXISTS session_summary;
        DROP TABLE IF EXISTS test_case_metrics;
        DROP TABLE IF EXISTS task_cycle_times;

        CREATE TABLE events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp   INTEGER NOT NULL,
            event_type  TEXT NOT NULL,
            role        TEXT,
            task_id     TEXT,
            payload     TEXT NOT NULL
        );

        CREATE TABLE agent_metrics (
            role            TEXT PRIMARY KEY,
            completions     INTEGER NOT NULL DEFAULT 0,
            failures        INTEGER NOT NULL DEFAULT 0,
            restarts        INTEGER NOT NULL DEFAULT 0,
            total_cycle_secs INTEGER NOT NULL DEFAULT 0,
            idle_polls      INTEGER NOT NULL DEFAULT 0,
            working_polls   INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE task_metrics (
            task_id         TEXT PRIMARY KEY,
            started_at      INTEGER,
            completed_at    INTEGER,
            retries         INTEGER NOT NULL DEFAULT 0,
            escalations     INTEGER NOT NULL DEFAULT 0,
            merge_time_secs INTEGER
        );

        CREATE TABLE session_summary (
            session_id      TEXT PRIMARY KEY,
            started_at      INTEGER NOT NULL,
            ended_at        INTEGER,
            tasks_completed INTEGER NOT NULL DEFAULT 0,
            total_merges    INTEGER NOT NULL DEFAULT 0,
            total_events    INTEGER NOT NULL DEFAULT 0
        );
        ",
    )
    .context("failed to install legacy telemetry schema fixture")?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCycleTimeRow {
    pub task_id: String,
    pub engineer: Option<String>,
    pub priority: String,
    pub cycle_time_mins: Option<i64>,
    pub lead_time_mins: Option<i64>,
    pub completed_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriorityCycleTimeRow {
    pub priority: String,
    pub average_cycle_time_mins: f64,
    pub completed_tasks: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EngineerThroughputRow {
    pub engineer: String,
    pub completed_tasks: i64,
    pub average_cycle_time_mins: Option<f64>,
    pub average_lead_time_mins: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HourlyThroughputRow {
    pub hour_start: i64,
    pub completed_tasks: i64,
}

pub fn record_test_results(
    conn: &Connection,
    task_id: u32,
    engineer: &str,
    results: &TestResults,
    flaky_failures: &[TestFailure],
) -> Result<()> {
    let task_id = task_id.to_string();
    let now = chrono::Utc::now().timestamp();

    for failure in flaky_failures {
        conn.execute(
            "INSERT INTO test_case_metrics (framework, test_name, flaky_passes, last_task_id, last_engineer, last_seen_at)
             VALUES (?1, ?2, 1, ?3, ?4, ?5)
             ON CONFLICT(framework, test_name) DO UPDATE SET
               flaky_passes = flaky_passes + 1,
               last_task_id = excluded.last_task_id,
               last_engineer = excluded.last_engineer,
               last_seen_at = excluded.last_seen_at",
            params![results.framework, failure.test_name, task_id, engineer, now],
        )?;
    }

    for failure in &results.failures {
        conn.execute(
            "INSERT INTO test_case_metrics (framework, test_name, failures, last_task_id, last_engineer, last_seen_at)
             VALUES (?1, ?2, 1, ?3, ?4, ?5)
             ON CONFLICT(framework, test_name) DO UPDATE SET
               failures = failures + 1,
               last_task_id = excluded.last_task_id,
               last_engineer = excluded.last_engineer,
               last_seen_at = excluded.last_seen_at",
            params![results.framework, failure.test_name, task_id, engineer, now],
        )?;
    }

    Ok(())
}

#[cfg(test)]
fn query_test_case_metric(
    conn: &Connection,
    framework: &str,
    test_name: &str,
) -> Result<(u32, u32)> {
    conn.query_row(
        "SELECT failures, flaky_passes FROM test_case_metrics WHERE framework = ?1 AND test_name = ?2",
        params![framework, test_name],
        |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
    )
    .context("failed to query test case metric")
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
                conn.execute(
                    "UPDATE task_metrics
                     SET carry_forward_effective = CASE
                         WHEN context_restart_count = 0 THEN carry_forward_effective
                         WHEN context_restart_count = 1 THEN 1
                         ELSE 0
                     END
                     WHERE task_id = ?1",
                    params![task],
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
        "task_escalated" | "narration_restart" => {
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
            if event.event == "agent_restarted"
                && event.reason.as_deref() == Some("context_exhausted")
                && let Some(task) = &event.task
            {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, context_restart_count) VALUES (?1, 1)
                     ON CONFLICT(task_id) DO UPDATE SET context_restart_count = context_restart_count + 1",
                    params![task],
                )?;
            }
        }
        "agent_handoff" => {
            if let Some(task) = &event.task {
                let success = if event.success == Some(true) { 1 } else { 0 };
                conn.execute(
                    "INSERT INTO task_metrics (task_id, handoff_attempts, handoff_successes) VALUES (?1, 1, ?2)
                     ON CONFLICT(task_id) DO UPDATE SET
                       handoff_attempts = handoff_attempts + 1,
                       handoff_successes = handoff_successes + ?2",
                    params![task, success],
                )?;
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
        "state_reconciliation" => {
            if let Some(task) = &event.task
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("orphan_branch_mismatch"))
            {
                conn.execute(
                    "INSERT INTO task_metrics (task_id, orphan_reconciliation_branch_mismatch_count) VALUES (?1, 1)
                     ON CONFLICT(task_id) DO UPDATE SET orphan_reconciliation_branch_mismatch_count = orphan_reconciliation_branch_mismatch_count + 1",
                    params![task],
                )?;
            }
        }
        "stall_detected" => {
            if let Some(metric) = non_engineer_stall_metric_from_event(event) {
                upsert_non_engineer_stall_metric(conn, &metric)?;
            }
        }
        "daemon_binary_stale" => {
            let metric = NonEngineerStallMetricEvent {
                role: "daemon",
                lane: "daemon",
                signal: "stale_daemon_binary_pressure",
                ts: event.ts,
                stall_secs: 0,
            };
            upsert_non_engineer_stall_metric(conn, &metric)?;
        }
        "daemon_started" => {
            let session_id = format!("session-{}", event.ts);
            conn.execute(
                "INSERT OR IGNORE INTO session_summary (session_id, started_at) VALUES (?1, ?2)",
                params![session_id, event.ts as i64],
            )?;
        }
        "discord_event_sent" => {
            conn.execute(
                "UPDATE session_summary SET discord_events_sent = discord_events_sent + 1
                 WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                [],
            )?;
        }
        "auto_merge_post_verify_result" | "github_verification_feedback" => match event.success {
            Some(true) => {
                conn.execute(
                    "UPDATE session_summary SET verification_passes = verification_passes + 1
                     WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                    [],
                )?;
            }
            Some(false) => {
                conn.execute(
                    "UPDATE session_summary SET verification_failures = verification_failures + 1
                     WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                    [],
                )?;
            }
            None => {}
        },
        "inbox_message_deduplicated" | "inbox_batch_delivered" => {
            conn.execute(
                "UPDATE session_summary SET notification_isolations = notification_isolations + 1
                 WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                [],
            )?;
        }
        "notification_delivery_sample" => {
            let latency_secs = event.uptime_secs.unwrap_or(0) as i64;
            conn.execute(
                "UPDATE session_summary
                 SET notification_latency_total_secs = notification_latency_total_secs + ?1,
                     notification_latency_samples = notification_latency_samples + 1
                 WHERE rowid = (SELECT rowid FROM session_summary ORDER BY started_at DESC LIMIT 1)",
                params![latency_secs],
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

struct NonEngineerStallMetricEvent<'a> {
    role: &'a str,
    lane: &'a str,
    signal: &'static str,
    ts: u64,
    stall_secs: u64,
}

fn non_engineer_stall_metric_from_event(
    event: &TeamEvent,
) -> Option<NonEngineerStallMetricEvent<'_>> {
    let reason = event.reason.as_deref()?;
    let is_supervisory_task = event
        .task
        .as_deref()
        .is_some_and(|task| task.starts_with("supervisory::"));
    if !is_supervisory_task && !reason.starts_with("supervisory_stalled_") {
        return None;
    }

    let lane = if reason.contains("_architect_")
        || event
            .task
            .as_deref()
            .is_some_and(|task| task == "supervisory::architect")
    {
        "architect"
    } else if reason.contains("_manager_")
        || event
            .task
            .as_deref()
            .is_some_and(|task| task == "supervisory::manager")
    {
        "manager"
    } else {
        return None;
    };
    let role = event.role.as_deref()?;
    Some(NonEngineerStallMetricEvent {
        role,
        lane,
        signal: non_engineer_stall_signal(reason),
        ts: event.ts,
        stall_secs: event.uptime_secs.unwrap_or_default(),
    })
}

fn non_engineer_stall_signal(reason: &str) -> &'static str {
    if reason.ends_with("review_waiting") || reason.ends_with("review_backlog") {
        "review_wait_timeout"
    } else if reason.ends_with("dispatch_gap") {
        "dispatch_gap_pressure"
    } else if reason.ends_with("direct_report_packets")
        || reason.ends_with("inbox_batching")
        || reason.ends_with("planning_inbox")
    {
        "inbox_backlog_pressure"
    } else {
        "working_timeout"
    }
}

fn upsert_non_engineer_stall_metric(
    conn: &Connection,
    metric: &NonEngineerStallMetricEvent<'_>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO non_engineer_stall_metrics
            (role, lane, signal, count, last_seen_at, max_stall_secs)
         VALUES (?1, ?2, ?3, 1, ?4, ?5)
         ON CONFLICT(role, signal) DO UPDATE SET
            count = count + 1,
            lane = excluded.lane,
            last_seen_at = excluded.last_seen_at,
            max_stall_secs = MAX(max_stall_secs, excluded.max_stall_secs)",
        params![
            metric.role,
            metric.lane,
            metric.signal,
            metric.ts as i64,
            metric.stall_secs as i64,
        ],
    )?;
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
    pub discord_events_sent: i64,
    pub verification_passes: i64,
    pub verification_failures: i64,
    pub notification_isolations: i64,
    pub notification_latency_total_secs: i64,
    pub notification_latency_samples: i64,
}

pub fn query_session_summaries(conn: &Connection) -> Result<Vec<SessionSummaryRow>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, started_at, ended_at, tasks_completed, total_merges, total_events,
                discord_events_sent, verification_passes, verification_failures,
                notification_isolations, notification_latency_total_secs, notification_latency_samples
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
                discord_events_sent: row.get(6)?,
                verification_passes: row.get(7)?,
                verification_failures: row.get(8)?,
                notification_isolations: row.get(9)?,
                notification_latency_total_secs: row.get(10)?,
                notification_latency_samples: row.get(11)?,
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
    pub context_restart_count: i64,
    pub handoff_attempts: i64,
    pub handoff_successes: i64,
    pub carry_forward_effective: Option<bool>,
    pub merge_time_secs: Option<i64>,
    pub confidence_score: Option<f64>,
    pub orphan_reconciliation_branch_mismatch_count: i64,
}

pub fn query_task_metrics(conn: &Connection) -> Result<Vec<TaskMetricsRow>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, started_at, completed_at, retries, narration_rejections, escalations,
                context_restart_count, handoff_attempts, handoff_successes,
                carry_forward_effective, merge_time_secs, confidence_score,
                orphan_reconciliation_branch_mismatch_count
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
                context_restart_count: row.get(6)?,
                handoff_attempts: row.get(7)?,
                handoff_successes: row.get(8)?,
                carry_forward_effective: row.get::<_, Option<i64>>(9)?.map(|value| value != 0),
                merge_time_secs: row.get(10)?,
                confidence_score: row.get(11)?,
                orphan_reconciliation_branch_mismatch_count: row.get(12)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEngineerStallMetricRow {
    pub role: String,
    pub lane: String,
    pub signal: String,
    pub count: i64,
    pub last_seen_at: i64,
    pub max_stall_secs: i64,
}

pub fn query_non_engineer_stall_metrics(
    conn: &Connection,
) -> Result<Vec<NonEngineerStallMetricRow>> {
    let mut stmt = conn.prepare(
        "SELECT role, lane, signal, count, last_seen_at, max_stall_secs
         FROM non_engineer_stall_metrics
         ORDER BY last_seen_at DESC, role, signal",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(NonEngineerStallMetricRow {
                role: row.get(0)?,
                lane: row.get(1)?,
                signal: row.get(2)?,
                count: row.get(3)?,
                last_seen_at: row.get(4)?,
                max_stall_secs: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn replace_task_cycle_times(conn: &Connection, rows: &[TaskCycleTimeRecord]) -> Result<()> {
    conn.execute("DELETE FROM task_cycle_times", [])?;

    let mut insert = conn.prepare(
        "INSERT INTO task_cycle_times
         (task_id, engineer, priority, cycle_time_mins, lead_time_mins, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;

    for row in rows {
        let Some(completed_at) = row.completed_at else {
            continue;
        };
        insert.execute(params![
            row.task_id.to_string(),
            row.engineer.as_deref(),
            row.priority.as_str(),
            row.cycle_time_minutes,
            row.lead_time_minutes,
            completed_at,
        ])?;
    }

    Ok(())
}

pub fn query_task_cycle_times(conn: &Connection) -> Result<Vec<TaskCycleTimeRow>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, engineer, priority, cycle_time_mins, lead_time_mins, completed_at
         FROM task_cycle_times
         ORDER BY completed_at DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TaskCycleTimeRow {
                task_id: row.get(0)?,
                engineer: row.get(1)?,
                priority: row.get(2)?,
                cycle_time_mins: row.get(3)?,
                lead_time_mins: row.get(4)?,
                completed_at: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_average_cycle_time_by_priority(
    conn: &Connection,
) -> Result<Vec<PriorityCycleTimeRow>> {
    let mut stmt = conn.prepare(
        "SELECT priority,
                AVG(cycle_time_mins) AS avg_cycle_time_mins,
                COUNT(*) AS completed_tasks
         FROM task_cycle_times
         WHERE cycle_time_mins IS NOT NULL
         GROUP BY priority
         ORDER BY CASE priority
             WHEN 'critical' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
         END, priority",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PriorityCycleTimeRow {
                priority: row.get(0)?,
                average_cycle_time_mins: row.get(1)?,
                completed_tasks: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_engineer_throughput(conn: &Connection) -> Result<Vec<EngineerThroughputRow>> {
    let mut stmt = conn.prepare(
        "SELECT engineer,
                COUNT(*) AS completed_tasks,
                AVG(cycle_time_mins) AS avg_cycle_time_mins,
                AVG(lead_time_mins) AS avg_lead_time_mins
         FROM task_cycle_times
         WHERE engineer IS NOT NULL
         GROUP BY engineer
         ORDER BY completed_tasks DESC, engineer ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(EngineerThroughputRow {
                engineer: row.get(0)?,
                completed_tasks: row.get(1)?,
                average_cycle_time_mins: row.get(2)?,
                average_lead_time_mins: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_hourly_throughput(
    conn: &Connection,
    window_start: i64,
) -> Result<Vec<HourlyThroughputRow>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE hours(hour_start) AS (
            SELECT (?1 / 3600) * 3600
            UNION ALL
            SELECT hour_start + 3600
            FROM hours
            WHERE hour_start + 3600 <= (strftime('%s', 'now', 'localtime') / 3600) * 3600
         )
         SELECT hours.hour_start,
                COALESCE(counts.completed_tasks, 0) AS completed_tasks
         FROM hours
         LEFT JOIN (
            SELECT (completed_at / 3600) * 3600 AS hour_start,
                   COUNT(*) AS completed_tasks
            FROM task_cycle_times
            WHERE completed_at >= ?1
            GROUP BY 1
         ) counts ON counts.hour_start = hours.hour_start
         ORDER BY hours.hour_start",
    )?;
    let rows = stmt
        .query_map(params![window_start], |row| {
            Ok(HourlyThroughputRow {
                hour_start: row.get(0)?,
                completed_tasks: row.get(1)?,
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
    pub direct_root_merge_count: i64,
    pub isolated_integration_merge_count: i64,
    pub direct_root_failure_count: i64,
    pub isolated_integration_failure_count: i64,
    pub rework_count: i64,
    pub review_nudge_count: i64,
    pub review_escalation_count: i64,
    pub avg_review_latency_secs: Option<f64>,
    pub accepted_decision_count: i64,
    pub rejected_decision_count: i64,
    pub rejection_reasons: Vec<AutoMergeReasonRow>,
    pub post_merge_verify_pass_count: i64,
    pub post_merge_verify_fail_count: i64,
    pub post_merge_verify_skip_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoMergeReasonRow {
    pub reason: String,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineerPerformanceProfileRow {
    pub role: String,
    pub completed_tasks: i64,
    pub avg_task_completion_secs: Option<f64>,
    pub lines_per_hour: Option<f64>,
    pub first_pass_test_rate: Option<f64>,
    pub context_exhaustion_frequency: Option<f64>,
}

pub fn query_engineer_performance_profiles(
    conn: &Connection,
) -> Result<Vec<EngineerPerformanceProfileRow>> {
    let mut completion_stmt = conn.prepare(
        "SELECT role, task_id,
                json_extract(payload, '$.time_to_completion_secs') AS time_to_completion_secs,
                json_extract(payload, '$.first_pass_test_rate') AS first_pass_test_rate
         FROM events
         WHERE event_type = 'quality_metrics_recorded'
           AND role IS NOT NULL
           AND task_id IS NOT NULL
         ORDER BY role, task_id",
    )?;

    #[derive(Default)]
    struct Accumulator {
        completed_tasks: i64,
        total_completion_secs: f64,
        completion_secs_samples: i64,
        first_pass_sum: f64,
        first_pass_samples: i64,
        context_exhausted_tasks: i64,
        loc_lines: i64,
        loc_hours: f64,
    }

    let mut by_role = std::collections::BTreeMap::<String, Accumulator>::new();
    let mut task_durations = std::collections::HashMap::<String, (String, f64)>::new();

    let completion_rows = completion_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<f64>>(2)?,
            row.get::<_, Option<f64>>(3)?,
        ))
    })?;

    for row in completion_rows {
        let (role, task_id, completion_secs, first_pass_rate) = row?;
        let entry = by_role.entry(role.clone()).or_default();
        entry.completed_tasks += 1;
        if let Some(completion_secs) = completion_secs {
            entry.total_completion_secs += completion_secs;
            entry.completion_secs_samples += 1;
            task_durations.insert(task_id.clone(), (role.clone(), completion_secs));
        }
        if let Some(first_pass_rate) = first_pass_rate {
            entry.first_pass_sum += first_pass_rate;
            entry.first_pass_samples += 1;
        }
    }

    let mut ctx_stmt = conn.prepare(
        "SELECT task_id, context_restart_count
         FROM task_metrics
         WHERE context_restart_count > 0",
    )?;
    let ctx_rows = ctx_stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in ctx_rows {
        let (task_id, _) = row?;
        if let Some((role, _)) = task_durations.get(&task_id) {
            by_role
                .entry(role.clone())
                .or_default()
                .context_exhausted_tasks += 1;
        }
    }

    let mut merge_stmt = conn.prepare(
        "SELECT role, task_id, json_extract(payload, '$.reason') AS reason
         FROM events
         WHERE event_type = 'task_auto_merged'
           AND role IS NOT NULL
           AND task_id IS NOT NULL",
    )?;
    let merge_rows = merge_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in merge_rows {
        let (role, task_id, reason) = row?;
        let Some(lines_changed) = reason
            .as_deref()
            .and_then(parse_lines_changed_from_merge_reason)
        else {
            continue;
        };
        let Some((task_role, completion_secs)) = task_durations.get(&task_id) else {
            continue;
        };
        if task_role != &role {
            continue;
        }
        let entry = by_role.entry(role).or_default();
        entry.loc_lines += lines_changed as i64;
        entry.loc_hours += completion_secs / 3600.0;
    }

    Ok(by_role
        .into_iter()
        .map(|(role, acc)| EngineerPerformanceProfileRow {
            role,
            completed_tasks: acc.completed_tasks,
            avg_task_completion_secs: (acc.completion_secs_samples > 0)
                .then(|| acc.total_completion_secs / acc.completion_secs_samples as f64),
            lines_per_hour: (acc.loc_hours > 0.0).then(|| acc.loc_lines as f64 / acc.loc_hours),
            first_pass_test_rate: (acc.first_pass_samples > 0)
                .then(|| acc.first_pass_sum / acc.first_pass_samples as f64),
            context_exhaustion_frequency: (acc.completed_tasks > 0)
                .then(|| acc.context_exhausted_tasks as f64 / acc.completed_tasks as f64),
        })
        .collect())
}

fn parse_lines_changed_from_merge_reason(reason: &str) -> Option<u64> {
    reason
        .split_whitespace()
        .find_map(|token| token.strip_prefix("lines="))
        .and_then(|value| value.parse::<u64>().ok())
}

fn load_events_by_type(conn: &Connection, event_type: &str) -> Result<Vec<TeamEvent>> {
    let mut stmt =
        conn.prepare("SELECT payload FROM events WHERE event_type = ?1 ORDER BY timestamp ASC")?;
    let rows = stmt.query_map(params![event_type], |row| row.get::<_, String>(0))?;
    rows.map(|row| {
        let payload = row?;
        serde_json::from_str::<TeamEvent>(&payload)
            .context("failed to deserialize telemetry event payload")
    })
    .collect()
}

fn extract_auto_merge_reasons(event: &TeamEvent) -> Vec<String> {
    if let Some(details) = event.details.as_deref()
        && let Ok(record) =
            serde_json::from_str::<crate::team::auto_merge::AutoMergeDecisionRecord>(details)
    {
        return record.reasons;
    }

    event
        .reason
        .as_deref()
        .and_then(|reason| reason.split("reasons: ").nth(1))
        .map(|reasons| {
            reasons
                .split("; ")
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn increment_merge_mode_counts(
    direct_root_count: &mut i64,
    isolated_integration_count: &mut i64,
    merge_mode: Option<&str>,
) {
    match merge_mode {
        Some("direct_root") => *direct_root_count += 1,
        Some("isolated_integration") => *isolated_integration_count += 1,
        _ => {}
    }
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
    let decision_events = load_events_by_type(conn, "auto_merge_decision_recorded")?;
    let post_verify_events = load_events_by_type(conn, "auto_merge_post_verify_result")?;
    let auto_merge_events = load_events_by_type(conn, "task_auto_merged")?;
    let manual_merge_events = load_events_by_type(conn, "task_manual_merged")?;
    let merge_failure_events = load_events_by_type(conn, "task_merge_failed")?;

    let mut direct_root_merge_count = 0;
    let mut isolated_integration_merge_count = 0;
    for event in auto_merge_events.iter().chain(manual_merge_events.iter()) {
        increment_merge_mode_counts(
            &mut direct_root_merge_count,
            &mut isolated_integration_merge_count,
            event.merge_mode.as_deref(),
        );
    }

    let mut direct_root_failure_count = 0;
    let mut isolated_integration_failure_count = 0;
    for event in &merge_failure_events {
        increment_merge_mode_counts(
            &mut direct_root_failure_count,
            &mut isolated_integration_failure_count,
            event.merge_mode.as_deref(),
        );
    }

    let mut accepted_decision_count = 0;
    let mut rejected_decision_count = 0;
    let mut rejection_reasons = BTreeMap::<String, i64>::new();
    for event in decision_events {
        match event.action_type.as_deref() {
            Some("accepted") => accepted_decision_count += 1,
            Some("manual_review") => {
                rejected_decision_count += 1;
                for reason in extract_auto_merge_reasons(&event) {
                    *rejection_reasons.entry(reason).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }

    let mut post_merge_verify_pass_count = 0;
    let mut post_merge_verify_fail_count = 0;
    let mut post_merge_verify_skip_count = 0;
    for event in post_verify_events {
        match event.success {
            Some(true) => post_merge_verify_pass_count += 1,
            Some(false) => post_merge_verify_fail_count += 1,
            None => post_merge_verify_skip_count += 1,
        }
    }

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

    let mut rejection_reasons = rejection_reasons
        .into_iter()
        .map(|(reason, count)| AutoMergeReasonRow { reason, count })
        .collect::<Vec<_>>();
    rejection_reasons.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.reason.cmp(&right.reason))
    });

    Ok(ReviewMetricsRow {
        auto_merge_count,
        manual_merge_count,
        direct_root_merge_count,
        isolated_integration_merge_count,
        direct_root_failure_count,
        isolated_integration_failure_count,
        rework_count,
        review_nudge_count,
        review_escalation_count,
        avg_review_latency_secs,
        accepted_decision_count,
        rejected_decision_count,
        rejection_reasons,
        post_merge_verify_pass_count,
        post_merge_verify_fail_count,
        post_merge_verify_skip_count,
    })
}

pub fn query_merge_queue_depth(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*)
         FROM task_metrics tm
         WHERE tm.completed_at IS NOT NULL
           AND NOT EXISTS (
               SELECT 1
               FROM events e
               WHERE e.task_id = tm.task_id
                 AND e.event_type IN ('task_auto_merged', 'task_manual_merged', 'task_reworked')
                 AND e.timestamp >= tm.completed_at
           )",
        [],
        |row| row.get(0),
    )
    .context("failed to query merge queue depth")
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
    fn legacy_schema_repairs_missing_columns_once() {
        let conn = Connection::open_in_memory().unwrap();
        install_legacy_schema_for_tests(&conn).unwrap();

        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();

        let task_columns = query_table_columns(&conn, "task_metrics").unwrap();
        assert!(task_columns.contains("narration_rejections"));
        assert!(task_columns.contains("confidence_score"));
        assert!(task_columns.contains("context_restart_count"));

        let session_columns = query_table_columns(&conn, "session_summary").unwrap();
        assert!(session_columns.contains("verification_passes"));
        assert!(session_columns.contains("notification_latency_samples"));

        let repair_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'telemetry_schema_repaired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repair_events, 1);

        let payload: String = conn
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'telemetry_schema_repaired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(payload.contains("narration_rejections"));
        assert!(payload.contains("confidence_score"));
        assert!(payload.contains("verification_passes"));
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
    fn non_engineer_stall_metrics_persist_supervisory_slo_signals() {
        let conn = open_in_memory().unwrap();

        let mut review = TeamEvent::stall_detected_with_reason(
            "manager",
            None,
            600,
            Some("supervisory_stalled_manager_review_backlog"),
        );
        review.task = Some("supervisory::manager".to_string());
        review.ts = 2000;
        insert_event(&conn, &review).unwrap();

        let mut dispatch = TeamEvent::stall_detected_with_reason(
            "manager",
            None,
            420,
            Some("supervisory_stalled_manager_dispatch_gap"),
        );
        dispatch.task = Some("supervisory::manager".to_string());
        dispatch.ts = 2100;
        insert_event(&conn, &dispatch).unwrap();

        let mut inbox = TeamEvent::stall_detected_with_reason(
            "architect",
            None,
            300,
            Some("supervisory_stalled_architect_direct_report_packets"),
        );
        inbox.task = Some("supervisory::architect".to_string());
        inbox.ts = 2200;
        insert_event(&conn, &inbox).unwrap();

        let mut legacy_inbox = TeamEvent::stall_detected_with_reason(
            "manager",
            None,
            240,
            Some("supervisory_inbox_batching"),
        );
        legacy_inbox.task = Some("supervisory::manager".to_string());
        legacy_inbox.ts = 2250;
        insert_event(&conn, &legacy_inbox).unwrap();

        insert_event(
            &conn,
            &TeamEvent::daemon_binary_stale(3, "stale daemon", "abc123", 1000, 2200),
        )
        .unwrap();

        let rows = query_non_engineer_stall_metrics(&conn).unwrap();
        assert!(rows.iter().any(|row| {
            row.role == "manager"
                && row.lane == "manager"
                && row.signal == "review_wait_timeout"
                && row.count == 1
                && row.max_stall_secs == 600
        }));
        assert!(rows.iter().any(|row| {
            row.role == "manager" && row.lane == "manager" && row.signal == "dispatch_gap_pressure"
        }));
        assert!(rows.iter().any(|row| {
            row.role == "architect"
                && row.lane == "architect"
                && row.signal == "inbox_backlog_pressure"
        }));
        assert!(rows.iter().any(|row| {
            row.role == "manager"
                && row.lane == "manager"
                && row.signal == "inbox_backlog_pressure"
                && row.max_stall_secs == 240
        }));
        assert!(rows.iter().any(|row| {
            row.role == "daemon"
                && row.lane == "daemon"
                && row.signal == "stale_daemon_binary_pressure"
        }));
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
    fn meta_conversation_escalation_increments_task_escalations() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::narration_restart("eng-1", Some(42))).unwrap();
        insert_event(&conn, &TeamEvent::narration_restart("eng-1", Some(42))).unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
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
    fn legacy_schema_accepts_new_task_metric_event_writes_after_repair() {
        let conn = Connection::open_in_memory().unwrap();
        install_legacy_schema_for_tests(&conn).unwrap();
        init_schema(&conn).unwrap();

        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();
        insert_event(&conn, &TeamEvent::narration_rejection("eng-1", 42, 1)).unwrap();
        insert_event(
            &conn,
            &TeamEvent::merge_confidence_scored(&crate::team::events::MergeConfidenceInfo {
                engineer: "eng-1",
                task: "42",
                confidence: 0.82,
                files_changed: 3,
                lines_changed: 40,
                has_migrations: false,
                has_config_changes: false,
                rename_count: 0,
            }),
        )
        .unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
        assert_eq!(tasks[0].narration_rejections, 1);
        assert_eq!(tasks[0].confidence_score, Some(0.82));
    }

    #[test]
    fn legacy_schema_accepts_new_session_summary_counters_after_repair() {
        let conn = Connection::open_in_memory().unwrap();
        install_legacy_schema_for_tests(&conn).unwrap();
        init_schema(&conn).unwrap();

        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result("eng-1", "42", Some(true), "passed", None),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::github_verification_feedback(
                &crate::team::events::GithubVerificationFeedbackInfo {
                    task: "43",
                    branch: Some("eng-1/43"),
                    commit: Some("abcdef1"),
                    check_name: "ci/test",
                    success: Some(false),
                    reason: "failure",
                    next_action: Some("fix CI"),
                    details: None,
                },
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::notification_delivery_sample("daemon", "manager", 12, "digest"),
        )
        .unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].verification_passes, 1);
        assert_eq!(summaries[0].verification_failures, 1);
        assert_eq!(summaries[0].notification_latency_total_secs, 12);
        assert_eq!(summaries[0].notification_latency_samples, 1);
    }

    #[test]
    fn orphan_branch_mismatch_reconciliation_increments_task_metric() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::state_reconciliation(
                Some("eng-1"),
                Some("124"),
                "orphan_branch_mismatch_recovered",
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::state_reconciliation(
                Some("eng-1"),
                Some("124"),
                "orphan_branch_mismatch_requeued",
            ),
        )
        .unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "124");
        assert_eq!(tasks[0].orphan_reconciliation_branch_mismatch_count, 2);
    }

    #[test]
    fn context_exhausted_restart_increments_task_restart_count() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 2),
        )
        .unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
        assert_eq!(tasks[0].context_restart_count, 2);
        assert_eq!(tasks[0].handoff_attempts, 0);
        assert_eq!(tasks[0].carry_forward_effective, None);
    }

    #[test]
    fn agent_handoff_updates_attempt_and_success_counts() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_handoff("eng-1", "42", "stalled", true),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_handoff("eng-1", "42", "shim_crash", false),
        )
        .unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
        assert_eq!(tasks[0].handoff_attempts, 2);
        assert_eq!(tasks[0].handoff_successes, 1);
    }

    #[test]
    fn task_completion_marks_single_restart_carry_forward_effective() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
        )
        .unwrap();
        insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("42"))).unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks[0].context_restart_count, 1);
        assert_eq!(tasks[0].carry_forward_effective, Some(true));
    }

    #[test]
    fn task_completion_marks_multi_restart_carry_forward_ineffective() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 2),
        )
        .unwrap();
        insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("42"))).unwrap();

        let tasks = query_task_metrics(&conn).unwrap();
        assert_eq!(tasks[0].context_restart_count, 2);
        assert_eq!(tasks[0].carry_forward_effective, Some(false));
    }

    #[test]
    fn engineer_performance_profiles_aggregate_completion_quality_and_context() {
        let conn = open_in_memory().unwrap();

        insert_event(
            &conn,
            &TeamEvent::quality_metrics_recorded(&crate::team::events::QualityMetricsInfo {
                backend: "codex",
                role: "eng-1",
                task: "41",
                narration_ratio: 0.1,
                commit_frequency: 1.0,
                first_pass_test_rate: 1.0,
                retry_rate: 0.0,
                time_to_completion_secs: 3_600,
            }),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::quality_metrics_recorded(&crate::team::events::QualityMetricsInfo {
                backend: "codex",
                role: "eng-1",
                task: "42",
                narration_ratio: 0.2,
                commit_frequency: 2.0,
                first_pass_test_rate: 0.0,
                retry_rate: 1.0,
                time_to_completion_secs: 1_800,
            }),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "41", 0.9, 2, 90),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "42", 0.9, 2, 30),
        )
        .unwrap();

        let rows = query_engineer_performance_profiles(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.role, "eng-1");
        assert_eq!(row.completed_tasks, 2);
        assert_eq!(row.avg_task_completion_secs, Some(2_700.0));
        assert_eq!(row.first_pass_test_rate, Some(0.5));
        assert_eq!(row.context_exhaustion_frequency, Some(0.5));
        assert_eq!(row.lines_per_hour, Some(80.0));
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
        assert_eq!(row.direct_root_merge_count, 0);
        assert_eq!(row.isolated_integration_merge_count, 0);
        assert_eq!(row.direct_root_failure_count, 0);
        assert_eq!(row.isolated_integration_failure_count, 0);
        assert_eq!(row.rework_count, 0);
        assert_eq!(row.review_nudge_count, 0);
        assert_eq!(row.review_escalation_count, 0);
        assert!(row.avg_review_latency_secs.is_none());
        assert_eq!(row.accepted_decision_count, 0);
        assert_eq!(row.rejected_decision_count, 0);
        assert!(row.rejection_reasons.is_empty());
        assert_eq!(row.post_merge_verify_pass_count, 0);
        assert_eq!(row.post_merge_verify_fail_count, 0);
        assert_eq!(row.post_merge_verify_skip_count, 0);
    }

    #[test]
    fn review_metrics_counts_all_event_types() {
        let conn = open_in_memory().unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged_with_mode(
                "eng-1",
                "1",
                0.9,
                2,
                30,
                Some(crate::team::merge::MergeMode::DirectRoot),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged_with_mode(
                "eng-1",
                "2",
                0.9,
                2,
                30,
                Some(crate::team::merge::MergeMode::IsolatedIntegration),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_manual_merged_with_mode(
                "3",
                Some(crate::team::merge::MergeMode::DirectRoot),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_merge_failed(
                "eng-1",
                "8",
                Some(crate::team::merge::MergeMode::IsolatedIntegration),
                "isolated merge path failed: integration checkout broke",
            ),
        )
        .unwrap();
        insert_event(&conn, &TeamEvent::task_reworked("eng-1", "4")).unwrap();
        insert_event(&conn, &TeamEvent::review_nudge_sent("manager", "5")).unwrap();
        insert_event(&conn, &TeamEvent::review_nudge_sent("manager", "6")).unwrap();
        insert_event(&conn, &TeamEvent::review_escalated_by_role("manager", "7")).unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_decision_recorded(&crate::team::events::AutoMergeDecisionInfo {
                engineer: "eng-1",
                task: "1",
                action_type: "accepted",
                confidence: 0.9,
                reason: "accepted for auto-merge: confidence 0.90; 2 files, 30 lines, 1 modules; reasons: confidence 0.90 meets threshold 0.80",
                details: r#"{"decision":"accepted","reasons":["confidence 0.90 meets threshold 0.80"],"files_changed":2,"lines_changed":30,"modules_touched":1,"has_migrations":false,"has_config_changes":false,"has_unsafe":false,"has_conflicts":false,"rename_count":0,"tests_passed":true,"override_forced":null,"diff_available":true}"#,
            }),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_decision_recorded(&crate::team::events::AutoMergeDecisionInfo {
                engineer: "eng-1",
                task: "3",
                action_type: "manual_review",
                confidence: 0.52,
                reason: "routed to manual review: confidence 0.52; 6 files, 220 lines, 3 modules; reasons: touches sensitive paths; 6 files changed (max 5)",
                details: r#"{"decision":"manual_review","reasons":["touches sensitive paths","6 files changed (max 5)"],"files_changed":6,"lines_changed":220,"modules_touched":3,"has_migrations":false,"has_config_changes":false,"has_unsafe":false,"has_conflicts":false,"rename_count":0,"tests_passed":true,"override_forced":null,"diff_available":true}"#,
            }),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result(
                "eng-1",
                "1",
                Some(true),
                "passed",
                Some("post-merge verification on main passed"),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result(
                "eng-1",
                "2",
                Some(false),
                "failed",
                Some("post-merge verification on main failed"),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result(
                "eng-1",
                "3",
                None,
                "skipped",
                Some("post-merge verification was not requested for this merge"),
            ),
        )
        .unwrap();

        let row = query_review_metrics(&conn).unwrap();
        assert_eq!(row.auto_merge_count, 2);
        assert_eq!(row.manual_merge_count, 1);
        assert_eq!(row.direct_root_merge_count, 2);
        assert_eq!(row.isolated_integration_merge_count, 1);
        assert_eq!(row.direct_root_failure_count, 0);
        assert_eq!(row.isolated_integration_failure_count, 1);
        assert_eq!(row.rework_count, 1);
        assert_eq!(row.review_nudge_count, 2);
        assert_eq!(row.review_escalation_count, 1);
        assert_eq!(row.accepted_decision_count, 1);
        assert_eq!(row.rejected_decision_count, 1);
        assert_eq!(
            row.rejection_reasons,
            vec![
                AutoMergeReasonRow {
                    reason: "6 files changed (max 5)".to_string(),
                    count: 1,
                },
                AutoMergeReasonRow {
                    reason: "touches sensitive paths".to_string(),
                    count: 1,
                },
            ]
        );
        assert_eq!(row.post_merge_verify_pass_count, 1);
        assert_eq!(row.post_merge_verify_fail_count, 1);
        assert_eq!(row.post_merge_verify_skip_count, 1);
    }

    #[test]
    fn review_metrics_computes_avg_latency() {
        let conn = open_in_memory().unwrap();

        // Task 10: completed at ts=1000, merged at ts=1100 → 100s latency
        let mut c1 = TeamEvent::task_completed("eng-1", Some("10"));
        c1.ts = 1000;
        insert_event(&conn, &c1).unwrap();
        let mut m1 = TeamEvent::task_auto_merged_with_mode(
            "eng-1",
            "10",
            0.9,
            2,
            30,
            Some(crate::team::merge::MergeMode::DirectRoot),
        );
        m1.ts = 1100;
        insert_event(&conn, &m1).unwrap();

        // Task 20: completed at ts=2000, merged at ts=2300 → 300s latency
        let mut c2 = TeamEvent::task_completed("eng-2", Some("20"));
        c2.ts = 2000;
        insert_event(&conn, &c2).unwrap();
        let mut m2 = TeamEvent::task_manual_merged_with_mode(
            "20",
            Some(crate::team::merge::MergeMode::IsolatedIntegration),
        );
        m2.ts = 2300;
        insert_event(&conn, &m2).unwrap();

        let row = query_review_metrics(&conn).unwrap();
        // avg = (100 + 300) / 2 = 200
        let avg = row.avg_review_latency_secs.unwrap();
        assert!((avg - 200.0).abs() < 0.01);
    }

    #[test]
    fn task_cycle_time_rows_replace_existing_snapshot() {
        let conn = open_in_memory().unwrap();
        let rows = vec![
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
                engineer: Some("eng-2".to_string()),
                priority: "low".to_string(),
                status: "done".to_string(),
                created_at: Some(200),
                started_at: Some(260),
                completed_at: Some(560),
                cycle_time_minutes: Some(5),
                lead_time_minutes: Some(6),
            },
        ];

        replace_task_cycle_times(&conn, &rows).unwrap();
        let stored = query_task_cycle_times(&conn).unwrap();
        assert_eq!(stored.len(), 2);

        replace_task_cycle_times(&conn, &rows[..1]).unwrap();
        let stored = query_task_cycle_times(&conn).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].task_id, "1");
    }

    #[test]
    fn task_cycle_time_queries_aggregate_priority_and_engineer() {
        let conn = open_in_memory().unwrap();
        replace_task_cycle_times(
            &conn,
            &[
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
                    priority: "medium".to_string(),
                    status: "done".to_string(),
                    created_at: Some(300),
                    started_at: Some(360),
                    completed_at: Some(660),
                    cycle_time_minutes: Some(5),
                    lead_time_minutes: Some(6),
                },
            ],
        )
        .unwrap();

        let by_priority = query_average_cycle_time_by_priority(&conn).unwrap();
        assert_eq!(by_priority[0].priority, "high");
        assert!((by_priority[0].average_cycle_time_mins - 7.5).abs() < f64::EPSILON);

        let by_engineer = query_engineer_throughput(&conn).unwrap();
        assert_eq!(by_engineer[0].engineer, "eng-1");
        assert_eq!(by_engineer[0].completed_tasks, 2);
    }

    #[test]
    fn record_test_results_tracks_failures_and_flakes() {
        let conn = open_in_memory().unwrap();
        let failed_results = TestResults {
            framework: "cargo".to_string(),
            total: Some(2),
            passed: 1,
            failed: 1,
            ignored: 0,
            failures: vec![super::super::test_results::TestFailure {
                test_name: "tests::fails".to_string(),
                message: Some("assertion failed".to_string()),
                location: Some("src/lib.rs:9".to_string()),
            }],
            summary: None,
        };
        record_test_results(&conn, 42, "eng-1", &failed_results, &[]).unwrap();

        let (failures, flaky_passes) =
            query_test_case_metric(&conn, "cargo", "tests::fails").unwrap();
        assert_eq!(failures, 1);
        assert_eq!(flaky_passes, 0);

        let passed_results = TestResults {
            framework: "cargo".to_string(),
            total: Some(2),
            passed: 2,
            failed: 0,
            ignored: 0,
            failures: vec![],
            summary: None,
        };
        record_test_results(
            &conn,
            42,
            "eng-1",
            &passed_results,
            &failed_results.failures,
        )
        .unwrap();

        let (failures, flaky_passes) =
            query_test_case_metric(&conn, "cargo", "tests::fails").unwrap();
        assert_eq!(failures, 1);
        assert_eq!(flaky_passes, 1);
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
            &TeamEvent::task_auto_merged_with_mode(
                "eng-1",
                "1",
                0.9,
                2,
                30,
                Some(crate::team::merge::MergeMode::DirectRoot),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_manual_merged_with_mode(
                "2",
                Some(crate::team::merge::MergeMode::IsolatedIntegration),
            ),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::task_auto_merged_with_mode(
                "eng-1",
                "3",
                0.8,
                1,
                10,
                Some(crate::team::merge::MergeMode::DirectRoot),
            ),
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

    #[test]
    fn session_summary_tracks_discord_verification_and_notification_counters() {
        let conn = open_in_memory().unwrap();
        insert_event(&conn, &TeamEvent::daemon_started()).unwrap();
        insert_event(
            &conn,
            &TeamEvent::discord_event_sent("events", "task_completed"),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result("eng-1", "42", Some(true), "passed", None),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result("eng-1", "43", Some(false), "failed", None),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::inbox_message_deduplicated("manager", "eng-1", 0xfeed),
        )
        .unwrap();
        insert_event(
            &conn,
            &TeamEvent::notification_delivery_sample("daemon", "manager", 12, "digest"),
        )
        .unwrap();

        let summaries = query_session_summaries(&conn).unwrap();
        assert_eq!(summaries[0].discord_events_sent, 1);
        assert_eq!(summaries[0].verification_passes, 1);
        assert_eq!(summaries[0].verification_failures, 1);
        assert_eq!(summaries[0].notification_isolations, 1);
        assert_eq!(summaries[0].notification_latency_total_secs, 12);
        assert_eq!(summaries[0].notification_latency_samples, 1);
    }

    #[test]
    fn merge_queue_depth_counts_completed_tasks_awaiting_merge() {
        let conn = open_in_memory().unwrap();
        let mut started = TeamEvent::daemon_started();
        started.ts = 100;
        insert_event(&conn, &started).unwrap();

        let mut completed = TeamEvent::task_completed("eng-1", Some("42"));
        completed.ts = 200;
        insert_event(&conn, &completed).unwrap();

        assert_eq!(query_merge_queue_depth(&conn).unwrap(), 1);

        let mut merged = TeamEvent::task_auto_merged("eng-1", "42", 0.9, 2, 10);
        merged.ts = 250;
        insert_event(&conn, &merged).unwrap();

        assert_eq!(query_merge_queue_depth(&conn).unwrap(), 0);
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
