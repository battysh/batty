//! Regression for 0.10.5: stall_detected events older than the latest
//! `daemon_started` entry must not leak into the current supervisory
//! stall signal. Before the fix a 2h-old stall from a previous daemon
//! session appeared as a live "stalled after 2h" warning on a freshly
//! restarted member.
//!
//! Test: write a supervisory stall event at ts=T-7200, a daemon_started
//! at ts=T-3600, then call the status query. Assert the stall is
//! filtered (no supervisory stall signal for the member).

use super::super::super::scenarios_common::ScenarioFixture;

#[test]
fn stall_cross_session_older_than_daemon_started_is_filtered() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();

    let now = now_secs();
    // Old supervisory stall: 2h ago, carries the supervisory:: task
    // prefix so status.rs treats it as a supervisory signal.
    let old_stall = format!(
        r#"{{"event":"stall_detected","ts":{ts},"role":"manager","task":"supervisory::manager","uptime_secs":7200,"reason":"supervisory:review_backlog"}}"#,
        ts = now - 7200
    );
    // Fresh daemon restart: 1h ago (between the stall and now).
    let daemon_started = format!(
        r#"{{"event":"daemon_started","ts":{ts},"action_type":"session_lifecycle","session_running":true}}"#,
        ts = now - 3600
    );

    fixture.append_raw_event_line(&old_stall);
    fixture.append_raw_event_line(&daemon_started);

    let has_stall = fixture
        .daemon_mut()
        .scenario_hooks()
        .has_supervisory_stall_signal("manager");
    assert!(
        !has_stall,
        "stall older than latest daemon_started must be filtered out"
    );
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
