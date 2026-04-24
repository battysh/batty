//! `batty metrics` — consolidated telemetry dashboard.
//!
//! Queries the SQLite telemetry database and prints a single-screen summary
//! covering session totals, per-agent performance, cycle-time statistics,
//! and review pipeline health. Handles missing/empty databases gracefully.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;

use super::{metrics, telemetry_db};

/// Aggregated dashboard metrics produced by [`query_dashboard`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DashboardMetrics {
    // Session totals
    pub total_tasks_completed: i64,
    pub total_merges: i64,
    pub total_events: i64,
    pub sessions_count: i64,
    pub discord_events_sent: i64,
    pub verification_pass_count: i64,
    pub verification_fail_count: i64,
    pub verification_pass_rate: Option<f64>,
    pub notification_isolation_count: i64,
    pub avg_notification_delivery_latency_secs: Option<f64>,
    pub merge_queue_depth: i64,
    pub non_engineer_stalls: Vec<telemetry_db::NonEngineerStallMetricRow>,

    // Cycle time
    pub avg_cycle_time_secs: Option<f64>,
    pub min_cycle_time_secs: Option<i64>,
    pub max_cycle_time_secs: Option<i64>,

    // Rates
    pub completion_rate: Option<f64>,
    pub failure_rate: Option<f64>,
    pub merge_success_rate: Option<f64>,

    // Review pipeline
    pub auto_merge_count: i64,
    pub manual_merge_count: i64,
    pub direct_root_merge_count: i64,
    pub isolated_integration_merge_count: i64,
    pub direct_root_failure_count: i64,
    pub isolated_integration_failure_count: i64,
    pub auto_merge_rate: Option<f64>,
    pub accepted_decision_count: i64,
    pub rejected_decision_count: i64,
    pub decision_accept_rate: Option<f64>,
    pub rejection_reasons: Vec<telemetry_db::AutoMergeReasonRow>,
    pub post_merge_verify_pass_count: i64,
    pub post_merge_verify_fail_count: i64,
    pub post_merge_verify_skip_count: i64,
    pub rework_count: i64,
    pub rework_rate: Option<f64>,
    pub avg_review_latency_secs: Option<f64>,

    // Per-agent breakdown
    pub agent_rows: Vec<AgentRow>,

    // Task cycle time tracking
    pub cycle_time_by_priority: Vec<telemetry_db::PriorityCycleTimeRow>,
    pub engineer_throughput: Vec<telemetry_db::EngineerThroughputRow>,
    pub tasks_completed_per_hour: Vec<telemetry_db::HourlyThroughputRow>,
    pub longest_running_tasks: Vec<metrics::InProgressTaskSummary>,
    pub latest_release: Option<crate::release::ReleaseRecord>,
}

/// Per-agent row in the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub role: String,
    pub completions: i64,
    pub failures: i64,
    pub restarts: i64,
    pub total_cycle_secs: i64,
    pub idle_pct: Option<f64>,
}

/// Query the telemetry database and build the aggregated dashboard.
pub fn query_dashboard(conn: &Connection) -> Result<DashboardMetrics> {
    let mut m = DashboardMetrics::default();

    // Session totals
    let sessions = telemetry_db::query_session_summaries(conn)?;
    m.sessions_count = sessions.len() as i64;
    for s in &sessions {
        m.total_tasks_completed += s.tasks_completed;
        m.total_merges += s.total_merges;
        m.total_events += s.total_events;
        m.discord_events_sent += s.discord_events_sent;
        m.verification_pass_count += s.verification_passes;
        m.verification_fail_count += s.verification_failures;
        m.notification_isolation_count += s.notification_isolations;
    }
    let total_notification_latency_secs: i64 = sessions
        .iter()
        .map(|row| row.notification_latency_total_secs)
        .sum();
    let total_notification_latency_samples: i64 = sessions
        .iter()
        .map(|row| row.notification_latency_samples)
        .sum();
    if total_notification_latency_samples > 0 {
        m.avg_notification_delivery_latency_secs = Some(
            total_notification_latency_secs as f64 / total_notification_latency_samples as f64,
        );
    }

    // Per-agent metrics
    let agents = telemetry_db::query_agent_metrics(conn)?;
    let mut total_completions: i64 = 0;
    let mut total_failures: i64 = 0;
    for a in &agents {
        total_completions += a.completions;
        total_failures += a.failures;
        let total_polls = a.idle_polls + a.working_polls;
        let idle_pct = if total_polls > 0 {
            Some(a.idle_polls as f64 / total_polls as f64 * 100.0)
        } else {
            None
        };
        m.agent_rows.push(AgentRow {
            role: a.role.clone(),
            completions: a.completions,
            failures: a.failures,
            restarts: a.restarts,
            total_cycle_secs: a.total_cycle_secs,
            idle_pct,
        });
    }

    // Rates
    let total_outcomes = total_completions + total_failures;
    if total_outcomes > 0 {
        m.completion_rate = Some(total_completions as f64 / total_outcomes as f64 * 100.0);
        m.failure_rate = Some(total_failures as f64 / total_outcomes as f64 * 100.0);
    }
    if m.total_tasks_completed > 0 {
        m.merge_success_rate = Some(m.total_merges as f64 / m.total_tasks_completed as f64 * 100.0);
    }

    // Cycle time from task_metrics
    let tasks = telemetry_db::query_task_metrics(conn)?;
    let cycle_times: Vec<i64> = tasks
        .iter()
        .filter_map(|t| match (t.started_at, t.completed_at) {
            (Some(s), Some(c)) if c > s => Some(c - s),
            _ => None,
        })
        .collect();
    if !cycle_times.is_empty() {
        let sum: i64 = cycle_times.iter().sum();
        m.avg_cycle_time_secs = Some(sum as f64 / cycle_times.len() as f64);
        m.min_cycle_time_secs = cycle_times.iter().copied().min();
        m.max_cycle_time_secs = cycle_times.iter().copied().max();
    }

    // Review pipeline
    let review = telemetry_db::query_review_metrics(conn)?;
    m.auto_merge_count = review.auto_merge_count;
    m.manual_merge_count = review.manual_merge_count;
    m.direct_root_merge_count = review.direct_root_merge_count;
    m.isolated_integration_merge_count = review.isolated_integration_merge_count;
    m.direct_root_failure_count = review.direct_root_failure_count;
    m.isolated_integration_failure_count = review.isolated_integration_failure_count;
    m.rework_count = review.rework_count;
    m.avg_review_latency_secs = review.avg_review_latency_secs;
    m.accepted_decision_count = review.accepted_decision_count;
    m.rejected_decision_count = review.rejected_decision_count;
    m.rejection_reasons = review.rejection_reasons;
    m.post_merge_verify_pass_count = review.post_merge_verify_pass_count;
    m.post_merge_verify_fail_count = review.post_merge_verify_fail_count;
    m.post_merge_verify_skip_count = review.post_merge_verify_skip_count;

    let total_merge = m.auto_merge_count + m.manual_merge_count;
    if total_merge > 0 {
        m.auto_merge_rate = Some(m.auto_merge_count as f64 / total_merge as f64 * 100.0);
    }
    let total_decisions = m.accepted_decision_count + m.rejected_decision_count;
    if total_decisions > 0 {
        m.decision_accept_rate =
            Some(m.accepted_decision_count as f64 / total_decisions as f64 * 100.0);
    }
    let total_reviewed = total_merge + m.rework_count;
    if total_reviewed > 0 {
        m.rework_rate = Some(m.rework_count as f64 / total_reviewed as f64 * 100.0);
    }
    let verification_total = m.verification_pass_count + m.verification_fail_count;
    if verification_total > 0 {
        m.verification_pass_rate =
            Some(m.verification_pass_count as f64 / verification_total as f64 * 100.0);
    }

    m.merge_queue_depth = telemetry_db::query_merge_queue_depth(conn)?;
    m.non_engineer_stalls = telemetry_db::query_non_engineer_stall_metrics(conn)?;

    m.cycle_time_by_priority = telemetry_db::query_average_cycle_time_by_priority(conn)?;
    m.engineer_throughput = telemetry_db::query_engineer_throughput(conn)?;
    if !telemetry_db::query_task_cycle_times(conn)?.is_empty() {
        let last_24h = Utc::now().timestamp() - (24 * 3600);
        m.tasks_completed_per_hour = telemetry_db::query_hourly_throughput(conn, last_24h)?;
    }

    Ok(m)
}

/// Format a duration in seconds as a human-readable string.
fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs)
    } else if secs < 3600.0 {
        let m = (secs / 60.0).floor();
        let s = secs - m * 60.0;
        format!("{:.0}m {:.0}s", m, s)
    } else {
        let h = (secs / 3600.0).floor();
        let rem = secs - h * 3600.0;
        let m = (rem / 60.0).floor();
        format!("{:.0}h {:.0}m", h, m)
    }
}

/// Format the dashboard for terminal display.
pub fn format_dashboard(m: &DashboardMetrics) -> String {
    let mut out = String::new();
    let na = "n/a".to_string();

    // Header
    out.push_str("Telemetry Dashboard\n");
    out.push_str(&"=".repeat(60));
    out.push('\n');

    // Session totals
    out.push_str("\nSession Totals\n");
    out.push_str(&"-".repeat(40));
    out.push('\n');
    out.push_str(&format!("  Sessions:        {}\n", m.sessions_count));
    out.push_str(&format!("  Tasks Completed: {}\n", m.total_tasks_completed));
    out.push_str(&format!("  Total Merges:    {}\n", m.total_merges));
    out.push_str(&format!("  Total Events:    {}\n", m.total_events));
    out.push_str(&format!("  Discord Events:  {}\n", m.discord_events_sent));

    // Cycle time
    out.push_str("\nCycle Time\n");
    out.push_str(&"-".repeat(40));
    out.push('\n');
    let avg = m
        .avg_cycle_time_secs
        .map(format_duration)
        .unwrap_or_else(|| na.clone());
    let min = m
        .min_cycle_time_secs
        .map(|s| format_duration(s as f64))
        .unwrap_or_else(|| na.clone());
    let max = m
        .max_cycle_time_secs
        .map(|s| format_duration(s as f64))
        .unwrap_or_else(|| na.clone());
    out.push_str(&format!("  Average: {}\n", avg));
    out.push_str(&format!("  Min:     {}\n", min));
    out.push_str(&format!("  Max:     {}\n", max));

    // Rates
    out.push_str("\nRates\n");
    out.push_str(&"-".repeat(40));
    out.push('\n');
    let cr = m
        .completion_rate
        .map(|r| format!("{:.0}%", r))
        .unwrap_or_else(|| na.clone());
    let fr = m
        .failure_rate
        .map(|r| format!("{:.0}%", r))
        .unwrap_or_else(|| na.clone());
    let mr = m
        .merge_success_rate
        .map(|r| format!("{:.0}%", r))
        .unwrap_or_else(|| na.clone());
    out.push_str(&format!("  Completion Rate:    {}\n", cr));
    out.push_str(&format!("  Failure Rate:       {}\n", fr));
    out.push_str(&format!("  Merge Success Rate: {}\n", mr));

    // Review pipeline
    out.push_str("\nReview Pipeline\n");
    out.push_str(&"-".repeat(40));
    out.push('\n');
    let amr = m
        .auto_merge_rate
        .map(|r| format!("{:.0}%", r))
        .unwrap_or_else(|| na.clone());
    let dar = m
        .decision_accept_rate
        .map(|r| format!("{:.0}%", r))
        .unwrap_or_else(|| na.clone());
    let rr = m
        .rework_rate
        .map(|r| format!("{:.0}%", r))
        .unwrap_or_else(|| na.clone());
    let latency = m
        .avg_review_latency_secs
        .map(format_duration)
        .unwrap_or_else(|| na.clone());
    out.push_str(&format!("  Auto-merge Rate: {}\n", amr));
    out.push_str(&format!(
        "  Auto: {}  Manual: {}  Rework: {}\n",
        m.auto_merge_count, m.manual_merge_count, m.rework_count
    ));
    out.push_str(&format!(
        "  Merge Modes: direct ok {} / fail {}  isolated ok {} / fail {}\n",
        m.direct_root_merge_count,
        m.direct_root_failure_count,
        m.isolated_integration_merge_count,
        m.isolated_integration_failure_count
    ));
    out.push_str(&format!(
        "  Decision Accept Rate: {} (accepted {} / rejected {})\n",
        dar, m.accepted_decision_count, m.rejected_decision_count
    ));
    out.push_str(&format!(
        "  Post-merge Verify: pass {}  fail {}  skipped {}\n",
        m.post_merge_verify_pass_count,
        m.post_merge_verify_fail_count,
        m.post_merge_verify_skip_count
    ));
    out.push_str(&format!("  Rework Rate:     {}\n", rr));
    out.push_str(&format!("  Avg Review Latency: {}\n", latency));
    if !m.rejection_reasons.is_empty() {
        out.push_str("  Rejection Reasons:\n");
        for row in m.rejection_reasons.iter().take(5) {
            out.push_str(&format!("    - {} ({})\n", row.reason, row.count));
        }
    }

    out.push_str("\nSubsystem Health\n");
    out.push_str(&"-".repeat(40));
    out.push('\n');
    let verify_rate = m
        .verification_pass_rate
        .map(|rate| format!("{rate:.0}%"))
        .unwrap_or_else(|| na.clone());
    let notification_latency = m
        .avg_notification_delivery_latency_secs
        .map(format_duration)
        .unwrap_or_else(|| na.clone());
    out.push_str(&format!(
        "  Verification: pass {}  fail {}  pass-rate {}\n",
        m.verification_pass_count, m.verification_fail_count, verify_rate
    ));
    out.push_str(&format!("  Merge Queue Depth: {}\n", m.merge_queue_depth));
    out.push_str(&format!(
        "  Notification Isolation: {}\n",
        m.notification_isolation_count
    ));
    out.push_str(&format!(
        "  Notification Latency: {}\n",
        notification_latency
    ));

    if !m.non_engineer_stalls.is_empty() {
        out.push_str("\nNon-Engineer Stall SLOs\n");
        out.push_str(&"-".repeat(60));
        out.push('\n');
        out.push_str(&format!(
            "  {:<16} {:<9} {:<28} {:>5} {:>8}\n",
            "ROLE", "LANE", "SIGNAL", "COUNT", "MAX"
        ));
        for row in &m.non_engineer_stalls {
            out.push_str(&format!(
                "  {:<16} {:<9} {:<28} {:>5} {:>8}\n",
                row.role,
                row.lane,
                row.signal,
                row.count,
                format_duration(row.max_stall_secs as f64)
            ));
        }
    }

    if !m.cycle_time_by_priority.is_empty() {
        out.push_str("\nAverage Cycle Time By Priority\n");
        out.push_str(&"-".repeat(50));
        out.push('\n');
        for row in &m.cycle_time_by_priority {
            out.push_str(&format!(
                "  {:<10} {:>8.1} min ({:>2} tasks)\n",
                row.priority, row.average_cycle_time_mins, row.completed_tasks
            ));
        }
    }

    if !m.tasks_completed_per_hour.is_empty() {
        out.push_str("\nTasks Completed Per Hour (Last 24h)\n");
        out.push_str(&"-".repeat(50));
        out.push('\n');
        for row in &m.tasks_completed_per_hour {
            let label = chrono::DateTime::<Utc>::from_timestamp(row.hour_start, 0)
                .map(|ts| ts.format("%m-%d %H:00").to_string())
                .unwrap_or_else(|| row.hour_start.to_string());
            out.push_str(&format!("  {}  {:>2}\n", label, row.completed_tasks));
        }
    }

    if !m.engineer_throughput.is_empty() {
        out.push_str("\nEngineer Throughput Ranking\n");
        out.push_str(&"-".repeat(60));
        out.push('\n');
        for (index, row) in m.engineer_throughput.iter().enumerate() {
            let avg_cycle = row
                .average_cycle_time_mins
                .map(|value| format!("{value:.1}m"))
                .unwrap_or_else(|| "n/a".to_string());
            let avg_lead = row
                .average_lead_time_mins
                .map(|value| format!("{value:.1}m"))
                .unwrap_or_else(|| "n/a".to_string());
            out.push_str(&format!(
                "  {}. {}  completed={}  avg_cycle={}  avg_lead={}\n",
                index + 1,
                row.engineer,
                row.completed_tasks,
                avg_cycle,
                avg_lead
            ));
        }
    }

    if !m.longest_running_tasks.is_empty() {
        out.push_str("\nLongest-Running In-Progress Tasks\n");
        out.push_str(&"-".repeat(60));
        out.push('\n');
        for row in &m.longest_running_tasks {
            out.push_str(&format!(
                "  #{} {} [{}] owner={} age={}m\n",
                row.task_id,
                row.title,
                row.priority,
                row.engineer.as_deref().unwrap_or("unassigned"),
                row.minutes_in_progress
            ));
        }
    }

    if let Some(release) = &m.latest_release {
        out.push_str("\nLatest Release\n");
        out.push_str(&"-".repeat(40));
        out.push('\n');
        let status = if release.success {
            "success"
        } else {
            "failure"
        };
        out.push_str(&format!(
            "  {} {} ({})\n",
            release.tag.as_deref().unwrap_or("unversioned"),
            release.git_ref.as_deref().unwrap_or("unknown-ref"),
            status
        ));
        out.push_str(&format!("  Reason: {}\n", release.reason));
        if let Some(details) = release.details.as_deref() {
            out.push_str(&format!("  Details: {}\n", details));
        }
    }

    // Per-agent table
    if !m.agent_rows.is_empty() {
        out.push_str("\nPer-Agent Breakdown\n");
        out.push_str(&"-".repeat(60));
        out.push('\n');
        out.push_str(&format!(
            "  {:<16} {:>6} {:>6} {:>6} {:>10} {:>8}\n",
            "ROLE", "DONE", "FAIL", "RESTART", "CYCLE_S", "IDLE%"
        ));
        for a in &m.agent_rows {
            let idle = a
                .idle_pct
                .map(|p| format!("{:.0}%", p))
                .unwrap_or_else(|| "-".to_string());
            out.push_str(&format!(
                "  {:<16} {:>6} {:>6} {:>6} {:>10} {:>8}\n",
                a.role, a.completions, a.failures, a.restarts, a.total_cycle_secs, idle
            ));
        }
    }

    out
}

/// Run the `batty metrics` command against the project root.
///
/// Opens the telemetry DB, queries the dashboard, and prints it.
/// Returns gracefully when the DB is missing or empty.
pub fn run(project_root: &Path) -> Result<()> {
    let db_path = project_root.join(".batty").join("telemetry.db");
    if !db_path.exists() {
        println!("Telemetry Dashboard\n{}", "=".repeat(60));
        println!("\nNo telemetry database found. Run `batty start` to begin collecting data.");
        return Ok(());
    }

    let conn = telemetry_db::open(project_root).context("failed to open telemetry database")?;
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let records = metrics::collect_task_cycle_time_records(&board_dir).unwrap_or_default();
    telemetry_db::replace_task_cycle_times(&conn, &records)?;

    let mut metrics = query_dashboard(&conn)?;
    metrics.longest_running_tasks =
        metrics::longest_running_in_progress_tasks(&records, Utc::now(), 5);
    metrics.latest_release = crate::release::latest_record(project_root)?;
    print!("{}", format_dashboard(&metrics));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::team::events::TeamEvent;
    use crate::team::telemetry_db;

    fn setup_db_with_data() -> Connection {
        let conn = telemetry_db::open_in_memory().unwrap();

        // Create a session
        let mut started = TeamEvent::daemon_started();
        started.ts = 1000;
        telemetry_db::insert_event(&conn, &started).unwrap();

        // Assign and complete tasks
        let mut a1 = TeamEvent::task_assigned("eng-1", "10");
        a1.ts = 1100;
        telemetry_db::insert_event(&conn, &a1).unwrap();

        let mut c1 = TeamEvent::task_completed("eng-1", Some("10"));
        c1.ts = 1400; // 300s cycle
        telemetry_db::insert_event(&conn, &c1).unwrap();

        let mut a2 = TeamEvent::task_assigned("eng-2", "20");
        a2.ts = 1200;
        telemetry_db::insert_event(&conn, &a2).unwrap();

        let mut c2 = TeamEvent::task_completed("eng-2", Some("20"));
        c2.ts = 1700; // 500s cycle
        telemetry_db::insert_event(&conn, &c2).unwrap();

        // Merge events
        let mut m1 = TeamEvent::task_auto_merged_with_mode(
            "eng-1",
            "10",
            0.9,
            2,
            30,
            Some(crate::team::merge::MergeMode::DirectRoot),
        );
        m1.ts = 1500;
        telemetry_db::insert_event(&conn, &m1).unwrap();
        telemetry_db::insert_event(
            &conn,
            &TeamEvent::auto_merge_decision_recorded(&crate::team::events::AutoMergeDecisionInfo {
                engineer: "eng-1",
                task: "10",
                action_type: "accepted",
                confidence: 0.9,
                reason: "accepted for auto-merge: confidence 0.90; 2 files, 30 lines, 1 modules; reasons: confidence 0.90 meets threshold 0.80",
                details: r#"{"decision":"accepted","reasons":["confidence 0.90 meets threshold 0.80"],"files_changed":2,"lines_changed":30,"modules_touched":1,"has_migrations":false,"has_config_changes":false,"has_unsafe":false,"has_conflicts":false,"rename_count":0,"tests_passed":true,"override_forced":null,"diff_available":true}"#,
            }),
        )
        .unwrap();
        telemetry_db::insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result(
                "eng-1",
                "10",
                Some(true),
                "passed",
                Some("post-merge verification on main passed"),
            ),
        )
        .unwrap();

        let mut m2 = TeamEvent::task_manual_merged_with_mode(
            "20",
            Some(crate::team::merge::MergeMode::DirectRoot),
        );
        m2.ts = 1800;
        telemetry_db::insert_event(&conn, &m2).unwrap();
        telemetry_db::insert_event(
            &conn,
            &TeamEvent::task_merge_failed(
                "eng-2",
                "30",
                Some(crate::team::merge::MergeMode::IsolatedIntegration),
                "isolated merge path failed: integration checkout broke",
            ),
        )
        .unwrap();
        telemetry_db::insert_event(
            &conn,
            &TeamEvent::auto_merge_decision_recorded(&crate::team::events::AutoMergeDecisionInfo {
                engineer: "eng-2",
                task: "20",
                action_type: "manual_review",
                confidence: 0.6,
                reason: "routed to manual review: confidence 0.60; 4 files, 120 lines, 3 modules; reasons: touches sensitive paths",
                details: r#"{"decision":"manual_review","reasons":["touches sensitive paths"],"files_changed":4,"lines_changed":120,"modules_touched":3,"has_migrations":false,"has_config_changes":false,"has_unsafe":false,"has_conflicts":false,"rename_count":0,"tests_passed":true,"override_forced":null,"diff_available":true}"#,
            }),
        )
        .unwrap();
        telemetry_db::insert_event(
            &conn,
            &TeamEvent::auto_merge_post_verify_result(
                "eng-2",
                "20",
                None,
                "skipped",
                Some("post-merge verification was not requested for this merge"),
            ),
        )
        .unwrap();

        // A failure
        telemetry_db::insert_event(&conn, &TeamEvent::pane_death("eng-1")).unwrap();

        let mut stall = TeamEvent::stall_detected_with_reason(
            "manager",
            None,
            600,
            Some("supervisory_stalled_manager_dispatch_gap"),
        );
        stall.task = Some("supervisory::manager".to_string());
        telemetry_db::insert_event(&conn, &stall).unwrap();

        conn
    }

    fn create_legacy_project_db(tmp: &tempfile::TempDir) -> Connection {
        let batty_dir = tmp.path().join(".batty");
        fs::create_dir_all(&batty_dir).unwrap();
        let db_path = batty_dir.join("telemetry.db");
        let conn = Connection::open(&db_path).unwrap();
        telemetry_db::install_legacy_schema_for_tests(&conn).unwrap();
        conn
    }

    #[test]
    fn metrics_with_data() {
        let conn = setup_db_with_data();
        let m = query_dashboard(&conn).unwrap();

        assert_eq!(m.sessions_count, 1);
        assert_eq!(m.total_tasks_completed, 2);
        assert_eq!(m.total_merges, 2);
        assert!(m.total_events > 0);

        // Cycle times: 300s and 500s → avg 400
        let avg = m.avg_cycle_time_secs.unwrap();
        assert!((avg - 400.0).abs() < 0.01);
        assert_eq!(m.min_cycle_time_secs, Some(300));
        assert_eq!(m.max_cycle_time_secs, Some(500));

        // Completion rate: 2 completions, 1 failure → 2/3 ≈ 66.7%
        let cr = m.completion_rate.unwrap();
        assert!((cr - 66.67).abs() < 1.0);

        // Failure rate: 1/3 ≈ 33.3%
        let fr = m.failure_rate.unwrap();
        assert!((fr - 33.33).abs() < 1.0);

        // Merge success rate: 2 merges / 2 completed → 100%
        let mr = m.merge_success_rate.unwrap();
        assert!((mr - 100.0).abs() < 0.01);

        // Review pipeline
        assert_eq!(m.auto_merge_count, 1);
        assert_eq!(m.manual_merge_count, 1);
        assert_eq!(m.direct_root_merge_count, 2);
        assert_eq!(m.isolated_integration_merge_count, 0);
        assert_eq!(m.direct_root_failure_count, 0);
        assert_eq!(m.isolated_integration_failure_count, 1);
        let amr = m.auto_merge_rate.unwrap();
        assert!((amr - 50.0).abs() < 0.01);
        assert_eq!(m.accepted_decision_count, 1);
        assert_eq!(m.rejected_decision_count, 1);
        let dar = m.decision_accept_rate.unwrap();
        assert!((dar - 50.0).abs() < 0.01);
        assert_eq!(m.post_merge_verify_pass_count, 1);
        assert_eq!(m.post_merge_verify_fail_count, 0);
        assert_eq!(m.post_merge_verify_skip_count, 1);
        assert_eq!(
            m.rejection_reasons,
            vec![telemetry_db::AutoMergeReasonRow {
                reason: "touches sensitive paths".to_string(),
                count: 1,
            }]
        );

        // Agents present
        assert_eq!(m.agent_rows.len(), 2);
        assert_eq!(m.non_engineer_stalls.len(), 1);
        assert_eq!(m.non_engineer_stalls[0].role, "manager");
        assert_eq!(m.non_engineer_stalls[0].signal, "dispatch_gap_pressure");
    }

    #[test]
    fn empty_db() {
        let conn = telemetry_db::open_in_memory().unwrap();
        let m = query_dashboard(&conn).unwrap();

        assert_eq!(m, DashboardMetrics::default());
        assert_eq!(m.sessions_count, 0);
        assert_eq!(m.total_tasks_completed, 0);
        assert!(m.avg_cycle_time_secs.is_none());
        assert!(m.completion_rate.is_none());
        assert!(m.failure_rate.is_none());
        assert!(m.merge_success_rate.is_none());
    }

    #[test]
    fn query_dashboard_reads_repaired_legacy_schema_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = create_legacy_project_db(&tmp);
        legacy
            .execute(
                "INSERT INTO session_summary (session_id, started_at, tasks_completed, total_merges, total_events)
                 VALUES ('legacy-session', 100, 2, 1, 5)",
                [],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO task_metrics (task_id, started_at, completed_at, retries, escalations, merge_time_secs)
                 VALUES ('42', 100, 160, 3, 1, 60)",
                [],
            )
            .unwrap();
        drop(legacy);

        let conn = telemetry_db::open(tmp.path()).unwrap();
        let metrics = query_dashboard(&conn).unwrap();

        assert_eq!(metrics.sessions_count, 1);
        assert_eq!(metrics.total_tasks_completed, 2);
        assert_eq!(metrics.total_merges, 1);
        assert_eq!(metrics.total_events, 5);
        assert_eq!(metrics.verification_pass_count, 0);
        assert_eq!(metrics.notification_isolation_count, 0);
        assert_eq!(metrics.avg_cycle_time_secs, Some(60.0));
    }

    #[test]
    fn missing_db_shows_message() {
        let tmp = tempfile::tempdir().unwrap();
        // No .batty/ directory → no DB file
        let result = run(tmp.path());
        assert!(result.is_ok());
    }

    #[test]
    fn rate_calculations() {
        let conn = telemetry_db::open_in_memory().unwrap();

        // 3 completions, 1 failure
        let mut started = TeamEvent::daemon_started();
        started.ts = 100;
        telemetry_db::insert_event(&conn, &started).unwrap();

        for i in 1..=3 {
            let mut a = TeamEvent::task_assigned("eng-1", &i.to_string());
            a.ts = 200 + i as u64 * 100;
            telemetry_db::insert_event(&conn, &a).unwrap();

            let mut c = TeamEvent::task_completed("eng-1", Some(&i.to_string()));
            c.ts = 200 + i as u64 * 100 + 60;
            telemetry_db::insert_event(&conn, &c).unwrap();

            let mut m = TeamEvent::task_auto_merged_with_mode(
                "eng-1",
                &i.to_string(),
                0.9,
                2,
                30,
                Some(crate::team::merge::MergeMode::DirectRoot),
            );
            m.ts = 200 + i as u64 * 100 + 120;
            telemetry_db::insert_event(&conn, &m).unwrap();
        }
        telemetry_db::insert_event(&conn, &TeamEvent::pane_death("eng-1")).unwrap();

        let m = query_dashboard(&conn).unwrap();

        // completion_rate: 3 / (3+1) = 75%
        let cr = m.completion_rate.unwrap();
        assert!((cr - 75.0).abs() < 0.01);

        // failure_rate: 1 / (3+1) = 25%
        let fr = m.failure_rate.unwrap();
        assert!((fr - 25.0).abs() < 0.01);

        // merge_success_rate: 3 merges / 3 completed = 100%
        let mr = m.merge_success_rate.unwrap();
        assert!((mr - 100.0).abs() < 0.01);

        // auto_merge_rate: 3 auto / (3+0) = 100%
        let amr = m.auto_merge_rate.unwrap();
        assert!((amr - 100.0).abs() < 0.01);

        // cycle times: each task has 60s cycle → avg 60
        let avg = m.avg_cycle_time_secs.unwrap();
        assert!((avg - 60.0).abs() < 0.01);
    }

    #[test]
    fn format_dashboard_renders_sections() {
        let m = DashboardMetrics {
            sessions_count: 2,
            total_tasks_completed: 10,
            total_merges: 8,
            total_events: 50,
            discord_events_sent: 4,
            verification_pass_count: 5,
            verification_fail_count: 1,
            verification_pass_rate: Some(83.0),
            notification_isolation_count: 3,
            avg_notification_delivery_latency_secs: Some(12.0),
            merge_queue_depth: 2,
            non_engineer_stalls: vec![telemetry_db::NonEngineerStallMetricRow {
                role: "manager".to_string(),
                lane: "manager".to_string(),
                signal: "dispatch_gap_pressure".to_string(),
                count: 2,
                last_seen_at: 1_744_000_000,
                max_stall_secs: 600,
            }],
            latest_release: Some(crate::release::ReleaseRecord {
                ts: "2026-04-10T12:00:00Z".to_string(),
                package_name: Some("batty".to_string()),
                version: Some("0.10.0".to_string()),
                tag: Some("v0.10.0".to_string()),
                git_ref: Some("abc123".to_string()),
                branch: Some("main".to_string()),
                previous_tag: Some("v0.9.0".to_string()),
                commits_since_previous: Some(2),
                verification_command: Some("cargo test".to_string()),
                verification_summary: Some("cargo test passed".to_string()),
                success: true,
                reason: "created annotated tag `v0.10.0`".to_string(),
                details: None,
                notes_path: Some(".batty/releases/v0.10.0.md".to_string()),
            }),
            avg_cycle_time_secs: Some(300.0),
            min_cycle_time_secs: Some(60),
            max_cycle_time_secs: Some(900),
            completion_rate: Some(90.0),
            failure_rate: Some(10.0),
            merge_success_rate: Some(80.0),
            auto_merge_count: 6,
            manual_merge_count: 2,
            direct_root_merge_count: 5,
            isolated_integration_merge_count: 3,
            direct_root_failure_count: 1,
            isolated_integration_failure_count: 2,
            auto_merge_rate: Some(75.0),
            accepted_decision_count: 6,
            rejected_decision_count: 2,
            decision_accept_rate: Some(75.0),
            rejection_reasons: vec![telemetry_db::AutoMergeReasonRow {
                reason: "needs-human-review".to_string(),
                count: 2,
            }],
            post_merge_verify_pass_count: 5,
            post_merge_verify_fail_count: 1,
            post_merge_verify_skip_count: 2,
            rework_count: 1,
            rework_rate: Some(11.0),
            avg_review_latency_secs: Some(120.0),
            cycle_time_by_priority: vec![telemetry_db::PriorityCycleTimeRow {
                priority: "high".to_string(),
                average_cycle_time_mins: 42.0,
                completed_tasks: 3,
            }],
            engineer_throughput: vec![telemetry_db::EngineerThroughputRow {
                engineer: "eng-1".to_string(),
                completed_tasks: 5,
                average_cycle_time_mins: Some(42.0),
                average_lead_time_mins: Some(60.0),
            }],
            tasks_completed_per_hour: vec![telemetry_db::HourlyThroughputRow {
                hour_start: 1_744_000_000,
                completed_tasks: 2,
            }],
            longest_running_tasks: vec![metrics::InProgressTaskSummary {
                task_id: 473,
                title: "Track cycle time".to_string(),
                engineer: Some("eng-1".to_string()),
                priority: "high".to_string(),
                minutes_in_progress: 95,
            }],
            agent_rows: vec![AgentRow {
                role: "eng-1".to_string(),
                completions: 5,
                failures: 1,
                restarts: 0,
                total_cycle_secs: 1500,
                idle_pct: Some(20.0),
            }],
        };

        let text = format_dashboard(&m);

        assert!(text.contains("Telemetry Dashboard"));
        assert!(text.contains("Session Totals"));
        assert!(text.contains("Sessions:        2"));
        assert!(text.contains("Tasks Completed: 10"));
        assert!(text.contains("Total Merges:    8"));
        assert!(text.contains("Discord Events:  4"));

        assert!(text.contains("Cycle Time"));
        assert!(text.contains("Average: 5m 0s"));
        assert!(text.contains("Min:     1m 0s"));
        assert!(text.contains("Max:     15m 0s"));

        assert!(text.contains("Rates"));
        assert!(text.contains("Completion Rate:    90%"));
        assert!(text.contains("Failure Rate:       10%"));
        assert!(text.contains("Merge Success Rate: 80%"));

        assert!(text.contains("Review Pipeline"));
        assert!(text.contains("Auto-merge Rate: 75%"));
        assert!(text.contains("Merge Modes: direct ok 5 / fail 1  isolated ok 3 / fail 2"));
        assert!(text.contains("Decision Accept Rate: 75%"));
        assert!(text.contains("Post-merge Verify: pass 5  fail 1  skipped 2"));
        assert!(text.contains("needs-human-review"));
        assert!(text.contains("Rework Rate:     11%"));

        assert!(text.contains("Average Cycle Time By Priority"));
        assert!(text.contains("Engineer Throughput Ranking"));
        assert!(text.contains("Longest-Running In-Progress Tasks"));
        assert!(text.contains("Subsystem Health"));
        assert!(text.contains("Verification: pass 5  fail 1"));
        assert!(text.contains("Merge Queue Depth: 2"));
        assert!(text.contains("Notification Isolation: 3"));
        assert!(text.contains("Notification Latency: 12s"));
        assert!(text.contains("Non-Engineer Stall SLOs"));
        assert!(text.contains("dispatch_gap_pressure"));
        assert!(text.contains("Latest Release"));
        assert!(text.contains("v0.10.0 abc123 (success)"));

        assert!(text.contains("Per-Agent Breakdown"));
        assert!(text.contains("eng-1"));
    }

    #[test]
    fn format_dashboard_empty_shows_na() {
        let m = DashboardMetrics::default();
        let text = format_dashboard(&m);

        assert!(text.contains("n/a"));
        assert!(text.contains("Sessions:        0"));
        // No agent table when empty
        assert!(!text.contains("Per-Agent Breakdown"));
    }

    #[test]
    fn format_duration_works() {
        assert_eq!(format_duration(30.0), "30s");
        assert_eq!(format_duration(90.0), "1m 30s");
        assert_eq!(format_duration(3661.0), "1h 1m");
    }
}
