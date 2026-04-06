use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use batty_cli::team::events::TeamEvent;
use batty_cli::team::inbox::{self, InboxMessage};
use batty_cli::team::openclaw;
use tempfile::TempDir;

struct FakeTmux {
    _dir: TempDir,
    path_env: String,
}

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("openclaw")
        .join(name)
}

fn copy_fixture_project(name: &str) -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let source = fixture_root(name);
    copy_dir_recursive(&source, tmp.path());
    let snapshot = tmp.path().join("_batty");
    if snapshot.exists() {
        fs::rename(snapshot, tmp.path().join(".batty")).unwrap();
    }
    tmp
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn install_fake_tmux(scenario: &str) -> FakeTmux {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("tmux");
    let body = format!(
        r#"#!/bin/sh
scenario="{scenario}"
command="$1"
shift

case "$command" in
  has-session)
    if [ "$scenario" = "stopped" ]; then
      exit 1
    fi
    exit 0
    ;;
  list-panes)
    if [ "$scenario" = "running" ]; then
      printf '%%1\tarchitect\tidle\t0\n%%2\tmanager\tworking\t0\n%%3\teng-1-1\tworking\t0\n'
      exit 0
    fi
    if [ "$scenario" = "degraded" ]; then
      printf '%%1\tarchitect\tidle\t0\n%%2\tmanager\tworking\t0\n%%3\teng-1-1\tcrashed\t1\n'
      exit 0
    fi
    exit 1
    ;;
  *)
    echo "unexpected tmux command: $command" >&2
    exit 1
    ;;
esac
"#
    );
    fs::write(&script, body).unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();

    let path_env = match std::env::var("PATH") {
        Ok(path) if !path.is_empty() => format!("{}:{path}", dir.path().display()),
        _ => dir.path().display().to_string(),
    };
    FakeTmux {
        _dir: dir,
        path_env,
    }
}

fn with_fake_tmux<T>(fake_tmux: Option<&FakeTmux>, run: impl FnOnce() -> T) -> T {
    let original_path = std::env::var_os("PATH");
    if let Some(fake_tmux) = fake_tmux {
        unsafe {
            std::env::set_var("PATH", &fake_tmux.path_env);
        }
    }
    let result = run();
    match original_path {
        Some(path) => unsafe {
            std::env::set_var("PATH", path);
        },
        None => unsafe {
            std::env::remove_var("PATH");
        },
    }
    result
}

fn seed_delivered_message(project_root: &Path, from: &str, to: &str, body: &str) {
    let root = inbox::inboxes_root(project_root);
    let id = inbox::deliver_to_inbox(&root, &InboxMessage::new_send(from, to, body)).unwrap();
    inbox::mark_delivered(&root, to, &id).unwrap();
}

#[test]
fn event_fixture_matches_contract_schema() {
    let content = fs::read_to_string(
        fixture_root("degraded")
            .join("_batty")
            .join("team_config")
            .join("events.jsonl"),
    )
    .unwrap();

    let events = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<TeamEvent>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(events.len(), 4);
    assert_eq!(events[0].event, "daemon_started");
    assert_eq!(events[1].event, "health_changed");
    assert_eq!(events[1].role.as_deref(), Some("eng-1-1"));
    assert_eq!(events[2].event, "task_escalated");
    assert_eq!(events[2].task.as_deref(), Some("449"));
    assert!(events[2].reason.as_deref().unwrap().contains("wording drift"));
    assert_eq!(events[3].event, "task_completed");
    assert_eq!(events[3].role.as_deref(), Some("eng-1-1"));
}

#[test]
fn openclaw_status_contract_supports_running_fixture_snapshot() {
    let project = copy_fixture_project("running");
    let fake_tmux = install_fake_tmux("running");

    let status = with_fake_tmux(Some(&fake_tmux), || {
        openclaw::openclaw_status_summary(project.path()).unwrap()
    });

    assert_eq!(status.project, "fixture-running");
    assert_eq!(status.team, "fixture-team");
    assert!(status.running);
    assert!(!status.paused);
    assert_eq!(status.active_task_count, 1);
    assert_eq!(status.review_queue_count, 1);
    assert!(status.unhealthy_members.is_empty());
    assert_eq!(status.triage_backlog_count, 0);
    assert!(
        status
            .highlights
            .iter()
            .any(|item| item == "Review queue has 1 task(s)")
    );
    assert!(
        status
            .recent_events
            .iter()
            .any(|item| item.contains("task 41 assigned to eng-1-1"))
    );
}

#[test]
fn openclaw_status_contract_supports_stopped_fixture_snapshot() {
    let project = copy_fixture_project("stopped");

    let status = openclaw::openclaw_status_summary(project.path()).unwrap();

    assert_eq!(status.project, "fixture-stopped");
    assert!(!status.running);
    assert!(status.paused);
    assert_eq!(status.active_task_count, 1);
    assert_eq!(status.review_queue_count, 0);
    assert!(
        status
            .highlights
            .iter()
            .any(|item| item == "Batty daemon is not running")
    );
    assert!(status.highlights.iter().any(|item| item == "Batty is paused"));
}

#[test]
fn openclaw_status_survives_internal_reason_wording_changes() {
    let project = copy_fixture_project("degraded");
    let fake_tmux = install_fake_tmux("degraded");
    seed_delivered_message(
        project.path(),
        "eng-1-1",
        "manager",
        "Need triage after the latest failed check.",
    );

    let status = with_fake_tmux(Some(&fake_tmux), || {
        openclaw::openclaw_status_summary(project.path()).unwrap()
    });

    assert!(status.running);
    assert_eq!(status.unhealthy_members, vec!["eng-1-1"]);
    assert_eq!(status.triage_backlog_count, 1);
    assert!(
        status
            .highlights
            .iter()
            .any(|item| item.contains("Unhealthy members: eng-1-1"))
    );
    assert!(
        status
            .highlights
            .iter()
            .any(|item| item.contains("Triage backlog: 1 message(s)"))
    );
    assert!(
        status
            .recent_events
            .iter()
            .any(|item| item.starts_with("task 449 escalated"))
    );
}

#[test]
fn openclaw_follow_up_harness_dispatches_reminders_and_escalations() {
    let project = copy_fixture_project("degraded");
    let fake_tmux = install_fake_tmux("degraded");
    seed_delivered_message(
        project.path(),
        "eng-1-1",
        "manager",
        "Follow-up needed on the degraded run.",
    );

    let summary = with_fake_tmux(Some(&fake_tmux), || {
        openclaw::openclaw_follow_up_summary(project.path()).unwrap()
    });

    assert_eq!(summary.dispatched.len(), 3);
    assert_eq!(summary.dispatched[0].name, "review-queue-reminder");
    assert_eq!(summary.dispatched[0].role, "manager");
    assert_eq!(summary.dispatched[0].reason, "Review queue follow-up");
    assert_eq!(summary.dispatched[1].name, "degraded-team-escalation");
    assert_eq!(summary.dispatched[1].role, "architect");
    assert_eq!(summary.dispatched[2].name, "triage-backlog-reminder");
    assert_eq!(summary.dispatched[2].role, "manager");

    let root = inbox::inboxes_root(project.path());
    let manager_messages = inbox::pending_messages(&root, "manager").unwrap();
    let architect_messages = inbox::pending_messages(&root, "architect").unwrap();

    assert_eq!(manager_messages.len(), 2);
    assert!(
        manager_messages
            .iter()
            .any(|message| message.body.contains("Review queue still has pending work"))
    );
    assert!(
        manager_messages
            .iter()
            .any(|message| message.body.contains("Triage backlog is building"))
    );
    assert_eq!(architect_messages.len(), 1);
    assert!(
        architect_messages[0]
            .body
            .contains("Unhealthy members are present")
    );
}

#[test]
fn openclaw_follow_up_harness_persists_last_sent_at_for_contract_stability() {
    let project = copy_fixture_project("degraded");
    let fake_tmux = install_fake_tmux("degraded");
    seed_delivered_message(
        project.path(),
        "eng-1-1",
        "manager",
        "Follow-up needed on the degraded run.",
    );

    let first = with_fake_tmux(Some(&fake_tmux), || {
        openclaw::openclaw_follow_up_summary(project.path()).unwrap()
    });
    let second = with_fake_tmux(Some(&fake_tmux), || {
        openclaw::openclaw_follow_up_summary(project.path()).unwrap()
    });

    assert_eq!(first.dispatched.len(), 3);
    assert!(second.dispatched.is_empty());

    let config_path = project.path().join(".batty").join("openclaw.yaml");
    let config = fs::read_to_string(config_path).unwrap();
    assert!(config.contains("last_sent_at:"));
}
