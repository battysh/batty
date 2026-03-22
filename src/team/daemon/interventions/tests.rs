use std::fs;
use std::time::{Duration, Instant};

use super::*;
use super::board_replenishment::board_replenishment_intervention_signature;
use super::dispatch::manager_dispatch_intervention_signature;
use super::owned_tasks::owned_task_intervention_signature;
use super::review::{
    review_backlog_owner_for_task, review_intervention_key, review_task_intervention_signature,
};
use super::utilization::architect_utilization_intervention_signature;
use crate::team::config::WorkflowMode;
use crate::team::daemon::interventions::dispatch::manager_dispatch_intervention_key;
use crate::team::daemon::interventions::utilization::architect_utilization_intervention_key;
use crate::team::harness::{TestHarness, architect_member, engineer_member, manager_member};
use crate::team::inbox::{self, InboxMessage};
use crate::team::test_support::TestDaemonBuilder;

fn triage_harness() -> TestHarness {
    TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead", Some("architect")))
        .with_member(engineer_member("eng-1", Some("lead"), false))
        .with_member(engineer_member("eng-2", Some("lead"), false))
        .with_member_state("lead", MemberState::Idle)
        .with_pane("lead", "%999999")
}

fn delivered_result(from: &str, body: &str) -> InboxMessage {
    let mut message = InboxMessage::new_send(from, "lead", body);
    message.timestamp = super::super::now_unix();
    message
}

fn delivered_result_for(from: &str, to: &str, body: &str) -> InboxMessage {
    let mut message = InboxMessage::new_send(from, to, body);
    message.timestamp = super::super::now_unix();
    message
}

fn delivered_reply(to: &str, body: &str) -> InboxMessage {
    let mut message = InboxMessage::new_send("lead", to, body);
    message.timestamp = super::super::now_unix();
    message
}

fn enter_idle_epoch(daemon: &mut TeamDaemon, member: &str) {
    daemon.update_automation_timers_for_state(member, MemberState::Working);
    daemon.update_automation_timers_for_state(member, MemberState::Idle);
    daemon.idle_started_at.insert(
        member.to_string(),
        Instant::now() - daemon.automation_idle_grace_duration() - Duration::from_secs(1),
    );
}

fn expire_triage_cooldown(daemon: &mut TeamDaemon, member: &str) {
    daemon.intervention_cooldowns.insert(
        format!("triage::{member}"),
        Instant::now()
            - Duration::from_secs(
                daemon
                    .config
                    .team_config
                    .automation
                    .intervention_cooldown_secs
                    + 1,
            ),
    );
}

fn mark_pending_delivered(harness: &TestHarness, member: &str) {
    for message in harness.pending_inbox_messages(member).unwrap() {
        inbox::mark_delivered(&harness.inbox_root(), member, &message.id).unwrap();
    }
}

fn write_prompt_nudge(project_root: &std::path::Path, filename: &str, body: &str) {
    std::fs::write(
        crate::team::team_config_dir(project_root).join(filename),
        format!("# Prompt\n\n## Nudge\n\n{body}\n"),
    )
    .unwrap();
}

fn owned_task_harness() -> TestHarness {
    TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead", Some("architect")))
        .with_member(engineer_member("eng-1", Some("lead"), false))
        .with_member_state("eng-1", MemberState::Idle)
        .with_pane("eng-1", "%999998")
}

fn intervention_team_harness() -> TestHarness {
    TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead", Some("architect")))
        .with_member(engineer_member("eng-1", Some("lead"), false))
        .with_member(engineer_member("eng-2", Some("lead"), false))
}

fn expire_owned_task_cooldown(daemon: &mut TeamDaemon, member: &str) {
    daemon.intervention_cooldowns.insert(
        member.to_string(),
        Instant::now()
            - Duration::from_secs(
                daemon
                    .config
                    .team_config
                    .automation
                    .intervention_cooldown_secs
                    + 1,
            ),
    );
}

fn expire_intervention_key_cooldown(daemon: &mut TeamDaemon, key: &str) {
    daemon.intervention_cooldowns.insert(
        key.to_string(),
        Instant::now()
            - Duration::from_secs(
                daemon
                    .config
                    .team_config
                    .automation
                    .intervention_cooldown_secs
                    + 1,
            ),
    );
}

fn set_exact_cooldown_boundary(daemon: &mut TeamDaemon, key: &str) {
    daemon.intervention_cooldowns.insert(
        key.to_string(),
        Instant::now()
            - Duration::from_secs(
                daemon
                    .config
                    .team_config
                    .automation
                    .intervention_cooldown_secs,
            ),
    );
}

fn run_intervention_cycle(daemon: &mut TeamDaemon) {
    daemon.maybe_intervene_triage_backlog().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_review_backlog().unwrap();
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    daemon.maybe_intervene_architect_utilization().unwrap();
    daemon.maybe_intervene_board_replenishment().unwrap();
}

fn enable_orchestrator_logging(daemon: &mut TeamDaemon) {
    daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
    daemon.config.team_config.orchestrator_pane = true;
}

fn assert_log_contains_in_order(log: &str, fragments: &[&str]) {
    let mut offset = 0;
    for fragment in fragments {
        let next = log[offset..].find(fragment).unwrap_or_else(|| {
            panic!("log never contained `{fragment}` after offset {offset}")
        });
        offset += next + fragment.len();
    }
}

#[test]
fn maybe_intervene_triage_backlog_queues_expected_message_for_idle_manager() {
    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Triage backlog detected"));
    assert!(
        pending[0]
            .body
            .contains("1 delivered direct-report result packet")
    );
    assert!(pending[0].body.contains("Reports in scope: eng-1, eng-2."));
    assert!(pending[0].body.contains("batty inbox lead"));
    assert!(pending[0].body.contains("batty read lead <ref>"));
    assert!(pending[0].body.contains("batty send eng-1"));
    assert!(pending[0].body.contains("batty assign eng-1"));
    assert!(pending[0].body.contains("batty send architect"));
}

#[test]
fn maybe_intervene_triage_backlog_does_not_fire_without_idle_epoch() {
    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();
    daemon.idle_started_at.insert(
        "lead".to_string(),
        Instant::now() - daemon.automation_idle_grace_duration() - Duration::from_secs(1),
    );

    daemon.maybe_intervene_triage_backlog().unwrap();

    assert!(!daemon.triage_interventions.contains_key("lead"));
    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());
}

#[test]
fn maybe_intervene_triage_backlog_ignores_reports_already_answered_by_manager() {
    let harness = triage_harness()
        .with_inbox_message("lead", delivered_result("eng-1", "done"), true)
        .with_inbox_message("eng-1", delivered_reply("eng-1", "accepted"), false);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    assert!(!daemon.triage_interventions.contains_key("lead"));
    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());
}

#[test]
fn maybe_intervene_triage_backlog_respects_cooldown_after_new_idle_epoch() {
    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();
    mark_pending_delivered(&harness, "lead");

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());
}

#[test]
fn maybe_intervene_triage_backlog_does_not_refire_while_prior_message_is_pending() {
    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();
    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
}

#[test]
fn maybe_intervene_triage_backlog_refires_after_cooldown_expires() {
    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();
    mark_pending_delivered(&harness, "lead");

    enter_idle_epoch(&mut daemon, "lead");
    expire_triage_cooldown(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(daemon.triage_interventions.get("lead"), Some(&2));
}

#[test]
fn maybe_intervene_triage_backlog_updates_count_when_new_report_arrives() {
    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();
    mark_pending_delivered(&harness, "lead");

    let mut second_result = delivered_result("eng-2", "done too");
    second_result.timestamp += 1;
    let second_result_id =
        inbox::deliver_to_inbox(&harness.inbox_root(), &second_result).unwrap();
    inbox::mark_delivered(&harness.inbox_root(), "lead", &second_result_id).unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    expire_triage_cooldown(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]
            .body
            .contains("2 delivered direct-report result packet")
    );
    assert!(pending[0].body.contains("Reports in scope: eng-1, eng-2."));
}

#[test]
fn maybe_intervene_owned_tasks_queues_task_message_for_idle_engineer() {
    let harness =
        owned_task_harness().with_board_task(191, "owned-task", "in-progress", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = harness.pending_inbox_messages("eng-1").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "lead");
    assert!(
        pending[0]
            .body
            .contains("Owned active task backlog detected")
    );
    assert!(pending[0].body.contains("Task #191"));
    assert!(pending[0].body.contains("batty send lead"));

    let state = daemon.owned_task_interventions.get("eng-1").unwrap();
    assert_eq!(state.idle_epoch, 1);
    assert_eq!(state.signature, "191:in-progress");
    assert!(!state.escalation_sent);
}

#[test]
fn maybe_intervene_owned_tasks_updates_idle_epoch_across_state_transitions() {
    let harness =
        owned_task_harness().with_board_task(191, "owned-task", "in-progress", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = harness.pending_inbox_messages("eng-1").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .map(|state| state.idle_epoch),
        Some(2)
    );
}

#[test]
fn maybe_intervene_owned_tasks_escalates_to_manager_after_threshold() {
    let harness =
        owned_task_harness().with_board_task(191, "owned-task", "in-progress", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();
    daemon
        .config
        .team_config
        .workflow_policy
        .escalation_threshold_secs = 120;

    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon
        .owned_task_interventions
        .get_mut("eng-1")
        .unwrap()
        .detected_at = Instant::now() - Duration::from_secs(121);

    daemon.maybe_intervene_owned_tasks().unwrap();

    let lead_pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(lead_pending.len(), 1);
    assert_eq!(lead_pending[0].from, "daemon");
    assert!(lead_pending[0].body.contains("Stuck task escalation"));
    assert!(lead_pending[0].body.contains("eng-1"));
    assert!(lead_pending[0].body.contains("Task #191"));
    assert!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .is_some_and(|state| state.escalation_sent)
    );
}

#[test]
fn maybe_intervene_owned_tasks_only_escalates_once_per_signature() {
    let harness =
        owned_task_harness().with_board_task(191, "owned-task", "in-progress", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();
    daemon
        .config
        .team_config
        .workflow_policy
        .escalation_threshold_secs = 120;

    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon
        .owned_task_interventions
        .get_mut("eng-1")
        .unwrap()
        .detected_at = Instant::now() - Duration::from_secs(121);

    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();

    let lead_pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(lead_pending.len(), 1);
    assert!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .is_some_and(|state| state.escalation_sent)
    );
}

#[test]
fn maybe_intervene_owned_tasks_signature_change_resets_state() {
    let harness =
        owned_task_harness().with_board_task(191, "first-task", "in-progress", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();
    mark_pending_delivered(&harness, "eng-1");

    let tasks_dir = harness.board_tasks_dir();
    let second_task_path = tasks_dir.join("192-second-task.md");
    std::fs::write(
        &second_task_path,
        "---\nid: 192\ntitle: second-task\nstatus: in-progress\npriority: high\nclass: standard\nclaimed_by: eng-1\n---\n\nTask description.\n",
    )
    .unwrap();
    expire_owned_task_cooldown(&mut daemon, "eng-1");

    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = harness.pending_inbox_messages("eng-1").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("Task #191"));
    assert!(pending[0].body.contains("#192 (in-progress) second-task"));

    let state = daemon.owned_task_interventions.get("eng-1").unwrap();
    assert_eq!(state.signature, "191:in-progress|192:in-progress");
    assert!(!state.escalation_sent);
}

#[test]
fn maybe_intervene_owned_tasks_skips_working_engineer() {
    let harness = TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead", Some("architect")))
        .with_member(engineer_member("eng-1", Some("lead"), false))
        .with_member_state("eng-1", MemberState::Working)
        .with_pane("eng-1", "%999998")
        .with_board_task(191, "owned-task", "in-progress", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    daemon.maybe_intervene_owned_tasks().unwrap();

    assert!(harness.pending_inbox_messages("eng-1").unwrap().is_empty());
    assert!(!daemon.owned_task_interventions.contains_key("eng-1"));
}

#[test]
fn maybe_intervene_review_backlog_queues_for_idle_manager() {
    let harness = intervention_team_harness()
        .with_member_state("lead", MemberState::Idle)
        .with_pane("lead", "%999997")
        .with_board_task(191, "review-task", "review", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_review_backlog().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Review backlog detected"));
    assert!(pending[0].body.contains("#191 by eng-1"));
    assert!(pending[0].body.contains("batty merge eng-1"));
    assert!(daemon.owned_task_interventions.contains_key("review::lead"));
}

#[test]
fn maybe_intervene_review_backlog_dedupes_same_signature() {
    let harness = intervention_team_harness()
        .with_member_state("lead", MemberState::Idle)
        .with_pane("lead", "%999997")
        .with_board_task(191, "review-task", "review", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_review_backlog().unwrap();
    mark_pending_delivered(&harness, "lead");

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_review_backlog().unwrap();

    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("review::lead")
            .map(|state| state.signature.as_str()),
        Some("191:review:eng-1")
    );
}

#[test]
fn maybe_intervene_review_backlog_respects_cooldown_until_signature_refire() {
    let harness = intervention_team_harness()
        .with_member_state("lead", MemberState::Idle)
        .with_pane("lead", "%999997")
        .with_board_task(191, "review-task", "review", Some("eng-1"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_review_backlog().unwrap();
    mark_pending_delivered(&harness, "lead");

    std::fs::write(
        harness.board_tasks_dir().join("192-review-task.md"),
        "---\nid: 192\ntitle: review-task-2\nstatus: review\npriority: high\nclass: standard\nclaimed_by: eng-2\n---\n\nTask description.\n",
    )
    .unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_review_backlog().unwrap();
    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());

    expire_intervention_key_cooldown(&mut daemon, "review::lead");
    daemon.maybe_intervene_review_backlog().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("#191 by eng-1"));
    assert!(pending[0].body.contains("#192 by eng-2"));
}

#[test]
fn maybe_intervene_manager_dispatch_gap_queues_for_idle_lead() {
    let harness = intervention_team_harness()
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("lead", "%999997")
        .with_board_task(191, "active-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "open-task", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Dispatch recovery needed"));
    assert!(pending[0].body.contains("eng-1 on #191"));
    assert!(pending[0].body.contains("eng-2"));
    assert!(pending[0].body.contains("batty assign eng-2"));
    assert!(
        daemon
            .owned_task_interventions
            .contains_key("dispatch::lead")
    );
}

#[test]
fn maybe_intervene_manager_dispatch_gap_dedupes_same_signature() {
    let harness = intervention_team_harness()
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("lead", "%999997")
        .with_board_task(191, "active-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "open-task", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    mark_pending_delivered(&harness, "lead");

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();

    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());
}

#[test]
fn maybe_intervene_manager_dispatch_gap_respects_cooldown_until_signature_refire() {
    let harness = intervention_team_harness()
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("lead", "%999997")
        .with_board_task(191, "active-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "open-task", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    mark_pending_delivered(&harness, "lead");

    std::fs::write(
        harness.board_tasks_dir().join("193-open-task.md"),
        "---\nid: 193\ntitle: open-task-2\nstatus: backlog\npriority: high\nclass: standard\n---\n\nTask description.\n",
    )
    .unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    assert!(harness.pending_inbox_messages("lead").unwrap().is_empty());

    expire_intervention_key_cooldown(&mut daemon, "dispatch::lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();

    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("#193 (backlog) open-task-2"));
}

#[test]
fn maybe_intervene_architect_utilization_queues_for_underloaded_architect() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996")
        .with_board_task(191, "active-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "open-task", "backlog", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_architect_utilization().unwrap();

    let pending = harness.pending_inbox_messages("architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "daemon");
    assert!(pending[0].body.contains("Utilization recovery needed"));
    assert!(pending[0].body.contains("eng-1 on #191"));
    assert!(pending[0].body.contains("eng-2"));
    assert!(pending[0].body.contains("Task #192"));
    assert!(
        daemon
            .owned_task_interventions
            .contains_key("utilization::architect")
    );
}

#[test]
fn maybe_intervene_architect_utilization_dedupes_same_signature() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996")
        .with_board_task(191, "active-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "open-task", "backlog", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_architect_utilization().unwrap();
    mark_pending_delivered(&harness, "architect");

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_architect_utilization().unwrap();

    assert!(
        harness
            .pending_inbox_messages("architect")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn maybe_intervene_architect_utilization_respects_cooldown_until_signature_refire() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996")
        .with_board_task(191, "active-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "open-task", "backlog", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_architect_utilization().unwrap();
    mark_pending_delivered(&harness, "architect");

    std::fs::write(
        harness.board_tasks_dir().join("193-open-task.md"),
        "---\nid: 193\ntitle: open-task-2\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
    )
    .unwrap();

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_architect_utilization().unwrap();
    assert!(
        harness
            .pending_inbox_messages("architect")
            .unwrap()
            .is_empty()
    );

    expire_intervention_key_cooldown(&mut daemon, "utilization::architect");
    daemon.maybe_intervene_architect_utilization().unwrap();

    let pending = harness.pending_inbox_messages("architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("#193 (todo) open-task-2"));
}

#[test]
fn maybe_intervene_board_replenishment_fires_when_todo_below_threshold_and_idle_engineers_exist()
{
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Working)
        .with_member_state("eng-1", MemberState::Working)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996")
        .with_board_task(191, "already-running", "in-progress", Some("eng-1"))
        .with_board_task(192, "next-up", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();
    daemon.config.team_config.automation.replenishment_threshold = Some(2);

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_board_replenishment().unwrap();

    let pending = harness.pending_inbox_messages("architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "daemon");
    assert!(pending[0].body.contains("Board replenishment needed"));
    assert!(pending[0].body.contains("todo=1, in-progress=1, done=0"));
    assert!(
        pending[0]
            .body
            .contains("Idle engineers without work: 1 (eng-2)")
    );
    assert!(pending[0].body.contains("Runnable todo threshold: 2"));
    assert!(
        daemon
            .owned_task_interventions
            .contains_key("replenishment::architect")
    );
}

#[test]
fn maybe_intervene_board_replenishment_respects_cooldown() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Working)
        .with_member_state("eng-1", MemberState::Working)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996")
        .with_board_task(191, "already-running", "in-progress", Some("eng-1"))
        .with_board_task(192, "next-up", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();
    daemon.config.team_config.automation.replenishment_threshold = Some(2);

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_board_replenishment().unwrap();
    mark_pending_delivered(&harness, "architect");

    daemon
        .owned_task_interventions
        .remove("replenishment::architect");
    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_board_replenishment().unwrap();

    assert!(
        harness
            .pending_inbox_messages("architect")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn maybe_intervene_board_replenishment_includes_optional_context_file() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Working)
        .with_member_state("eng-1", MemberState::Working)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996");
    fs::create_dir_all(harness.board_tasks_dir()).unwrap();
    fs::write(
        harness
            .project_root()
            .join(".batty")
            .join("team_config")
            .join("replenishment_context.md"),
        "Prioritize launch-blocking tasks first.\nAvoid docs-only filler.",
    )
    .unwrap();
    let mut daemon = TestDaemonBuilder::new(harness.project_root())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
            engineer_member("eng-2", Some("lead"), false),
        ])
        .states(std::collections::HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("lead".to_string(), MemberState::Working),
            ("eng-1".to_string(), MemberState::Working),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .pane_map(std::collections::HashMap::from([(
            "architect".to_string(),
            "%999996".to_string(),
        )]))
        .build();
    daemon.config.team_config.automation.replenishment_threshold = Some(1);

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_board_replenishment().unwrap();

    let pending = harness.pending_inbox_messages("architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("Replenishment context:"));
    assert!(
        pending[0]
            .body
            .contains("Prioritize launch-blocking tasks first.")
    );
    assert!(pending[0].body.contains("Avoid docs-only filler."));
}

#[test]
fn maybe_intervene_board_replenishment_does_not_fire_when_all_engineers_busy() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Working)
        .with_member_state("eng-1", MemberState::Working)
        .with_member_state("eng-2", MemberState::Working)
        .with_pane("architect", "%999996")
        .with_board_task(191, "already-running", "in-progress", Some("eng-1"))
        .with_board_task(192, "next-up", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();
    daemon.config.team_config.automation.replenishment_threshold = Some(2);

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_board_replenishment().unwrap();

    assert!(
        harness
            .pending_inbox_messages("architect")
            .unwrap()
            .is_empty()
    );
    assert!(
        !daemon
            .owned_task_interventions
            .contains_key("replenishment::architect")
    );
}

#[test]
fn maybe_intervene_board_replenishment_does_not_fire_when_todo_is_sufficient() {
    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead", MemberState::Working)
        .with_member_state("eng-1", MemberState::Working)
        .with_member_state("eng-2", MemberState::Idle)
        .with_pane("architect", "%999996")
        .with_board_task(191, "next-up", "todo", None)
        .with_board_task(192, "follow-up", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();
    daemon.config.team_config.automation.replenishment_threshold = Some(2);

    enter_idle_epoch(&mut daemon, "architect");
    daemon.maybe_intervene_board_replenishment().unwrap();

    assert!(
        harness
            .pending_inbox_messages("architect")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn multi_intervention_cycle_fires_independent_recoveries_in_strict_order() {
    let harness = TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead-triage", Some("architect")))
        .with_member(manager_member("lead-review", Some("architect")))
        .with_member(manager_member("lead-dispatch", Some("architect")))
        .with_member(engineer_member("eng-triage", Some("lead-triage"), false))
        .with_member(engineer_member("eng-owned", Some("lead-triage"), false))
        .with_member(engineer_member("eng-review", Some("lead-review"), false))
        .with_member(engineer_member("eng-active", Some("lead-dispatch"), false))
        .with_member(engineer_member("eng-free", Some("lead-dispatch"), false))
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead-triage", MemberState::Idle)
        .with_member_state("lead-review", MemberState::Idle)
        .with_member_state("lead-dispatch", MemberState::Idle)
        .with_member_state("eng-owned", MemberState::Idle)
        .with_member_state("eng-triage", MemberState::Idle)
        .with_member_state("eng-review", MemberState::Idle)
        .with_member_state("eng-active", MemberState::Idle)
        .with_member_state("eng-free", MemberState::Idle)
        .with_pane("architect", "%999997")
        .with_pane("lead-triage", "%999998")
        .with_pane("lead-review", "%999999")
        .with_pane("lead-dispatch", "%999996")
        .with_pane("eng-owned", "%999995")
        .with_inbox_message(
            "lead-triage",
            delivered_result_for("eng-triage", "lead-triage", "done"),
            true,
        )
        .with_board_task(191, "owned-task", "in-progress", Some("eng-owned"))
        .with_board_task(192, "review-task", "review", Some("eng-review"))
        .with_board_task(193, "dispatch-task", "in-progress", Some("eng-active"))
        .with_board_task(194, "open-task", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();
    enable_orchestrator_logging(&mut daemon);

    enter_idle_epoch(&mut daemon, "architect");
    enter_idle_epoch(&mut daemon, "lead-triage");
    enter_idle_epoch(&mut daemon, "lead-review");
    enter_idle_epoch(&mut daemon, "lead-dispatch");
    enter_idle_epoch(&mut daemon, "eng-owned");
    run_intervention_cycle(&mut daemon);

    assert_eq!(
        harness.pending_inbox_messages("lead-triage").unwrap().len(),
        1
    );
    assert_eq!(
        harness.pending_inbox_messages("eng-owned").unwrap().len(),
        1
    );
    assert_eq!(
        harness.pending_inbox_messages("lead-review").unwrap().len(),
        1
    );
    assert_eq!(
        harness
            .pending_inbox_messages("lead-dispatch")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        harness.pending_inbox_messages("architect").unwrap().len(),
        1
    );

    let log = fs::read_to_string(
        harness
            .project_root()
            .join(".batty")
            .join("orchestrator.log"),
    )
    .unwrap();
    assert_log_contains_in_order(
        &log,
        &[
            "recovery: triage intervention for lead-triage",
            "recovery: owned-task intervention for eng-owned",
            "recovery: review intervention for lead-review",
            "recovery: dispatch-gap intervention for lead-dispatch",
            "recovery: utilization intervention for architect",
        ],
    );
}

#[test]
fn multi_intervention_cycle_pending_inbox_only_suppresses_targeted_member() {
    let harness = TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead-triage", Some("architect")))
        .with_member(manager_member("lead-review", Some("architect")))
        .with_member(manager_member("lead-dispatch", Some("architect")))
        .with_member(engineer_member("eng-triage", Some("lead-triage"), false))
        .with_member(engineer_member("eng-owned", Some("lead-triage"), false))
        .with_member(engineer_member("eng-review", Some("lead-review"), false))
        .with_member(engineer_member("eng-active", Some("lead-dispatch"), false))
        .with_member(engineer_member("eng-free", Some("lead-dispatch"), false))
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("lead-triage", MemberState::Idle)
        .with_member_state("lead-review", MemberState::Idle)
        .with_member_state("lead-dispatch", MemberState::Idle)
        .with_member_state("eng-owned", MemberState::Idle)
        .with_member_state("eng-triage", MemberState::Idle)
        .with_member_state("eng-review", MemberState::Idle)
        .with_member_state("eng-active", MemberState::Idle)
        .with_member_state("eng-free", MemberState::Idle)
        .with_pane("architect", "%999997")
        .with_pane("lead-triage", "%999998")
        .with_pane("lead-review", "%999999")
        .with_pane("lead-dispatch", "%999996")
        .with_pane("eng-owned", "%999995")
        .with_inbox_message(
            "lead-triage",
            InboxMessage::new_send("architect", "lead-triage", "Handle this first."),
            false,
        )
        .with_inbox_message(
            "lead-triage",
            delivered_result_for("eng-triage", "lead-triage", "done"),
            true,
        )
        .with_board_task(191, "owned-task", "in-progress", Some("eng-owned"))
        .with_board_task(192, "review-task", "review", Some("eng-review"))
        .with_board_task(193, "dispatch-task", "in-progress", Some("eng-active"))
        .with_board_task(194, "open-task", "todo", None);
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "architect");
    enter_idle_epoch(&mut daemon, "lead-triage");
    enter_idle_epoch(&mut daemon, "lead-review");
    enter_idle_epoch(&mut daemon, "lead-dispatch");
    enter_idle_epoch(&mut daemon, "eng-owned");
    run_intervention_cycle(&mut daemon);

    let triage_pending = harness.pending_inbox_messages("lead-triage").unwrap();
    assert_eq!(triage_pending.len(), 1);
    assert_eq!(triage_pending[0].from, "architect");
    assert!(!daemon.triage_interventions.contains_key("lead-triage"));
    assert_eq!(
        harness.pending_inbox_messages("eng-owned").unwrap().len(),
        1
    );
    assert_eq!(
        harness.pending_inbox_messages("lead-review").unwrap().len(),
        1
    );
    assert_eq!(
        harness
            .pending_inbox_messages("lead-dispatch")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        harness.pending_inbox_messages("architect").unwrap().len(),
        1
    );
}

#[test]
fn multi_intervention_cycle_exact_owned_cooldown_boundary_still_allows_parallel_refire() {
    let harness = TestHarness::new()
        .with_member(architect_member("architect"))
        .with_member(manager_member("lead", Some("architect")))
        .with_member(manager_member("lead-review", Some("architect")))
        .with_member(engineer_member("eng-triage", Some("lead"), false))
        .with_member(engineer_member("eng-1", Some("lead"), false))
        .with_member(engineer_member("eng-review", Some("lead-review"), false))
        .with_member_state("lead", MemberState::Idle)
        .with_member_state("lead-review", MemberState::Idle)
        .with_member_state("eng-triage", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle)
        .with_member_state("eng-review", MemberState::Idle)
        .with_pane("lead", "%999999")
        .with_pane("lead-review", "%999998")
        .with_pane("eng-1", "%999997")
        .with_inbox_message("lead", delivered_result("eng-triage", "done"), true)
        .with_board_task(191, "owned-task", "in-progress", Some("eng-1"))
        .with_board_task(192, "review-task", "review", Some("eng-review"));
    let mut daemon = harness.build_daemon().unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    enter_idle_epoch(&mut daemon, "lead-review");
    enter_idle_epoch(&mut daemon, "eng-1");
    daemon.maybe_intervene_triage_backlog().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_review_backlog().unwrap();
    mark_pending_delivered(&harness, "lead");
    mark_pending_delivered(&harness, "lead-review");
    mark_pending_delivered(&harness, "eng-1");

    std::fs::write(
        harness.board_tasks_dir().join("193-second-task.md"),
        "---\nid: 193\ntitle: second-task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\nTask description.\n",
    )
    .unwrap();

    enter_idle_epoch(&mut daemon, "lead");
    enter_idle_epoch(&mut daemon, "lead-review");
    enter_idle_epoch(&mut daemon, "eng-1");
    expire_triage_cooldown(&mut daemon, "lead");
    set_exact_cooldown_boundary(&mut daemon, "eng-1");
    daemon.maybe_intervene_triage_backlog().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_review_backlog().unwrap();

    assert_eq!(daemon.triage_interventions.get("lead"), Some(&2));
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .map(|state| state.idle_epoch),
        Some(2)
    );
    assert_eq!(harness.pending_inbox_messages("lead").unwrap().len(), 1);
    assert_eq!(harness.pending_inbox_messages("eng-1").unwrap().len(), 1);
    assert!(
        harness
            .pending_inbox_messages("lead-review")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn task_needs_owned_intervention_excludes_terminal_and_review_states() {
    assert!(task_needs_owned_intervention("backlog"));
    assert!(task_needs_owned_intervention("todo"));
    assert!(task_needs_owned_intervention("in-progress"));
    assert!(!task_needs_owned_intervention("review"));
    assert!(!task_needs_owned_intervention("done"));
    assert!(!task_needs_owned_intervention("archived"));
}

#[test]
fn intervention_key_helpers_use_expected_prefixes() {
    assert_eq!(manager_dispatch_intervention_key("lead"), "dispatch::lead");
    assert_eq!(review_intervention_key("lead"), "review::lead");
    assert_eq!(
        architect_utilization_intervention_key("architect"),
        "utilization::architect"
    );
}

#[test]
fn owned_task_intervention_signature_sorts_tasks() {
    let harness = triage_harness()
        .with_board_task(20, "task-20", "todo", Some("lead"))
        .with_board_task(10, "task-10", "in-progress", Some("lead"));
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let refs = vec![&tasks[0], &tasks[1]];

    let signature = owned_task_intervention_signature(&refs);

    assert_eq!(signature, "10:in-progress|20:todo");
}

#[test]
fn review_task_intervention_signature_sorts_and_includes_owner() {
    let harness = triage_harness()
        .with_board_task(22, "task-22", "review", Some("eng-2"))
        .with_board_task(11, "task-11", "review", Some("eng-1"));
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let refs = vec![&tasks[0], &tasks[1]];

    let signature = review_task_intervention_signature(&refs);

    assert_eq!(signature, "11:review:eng-1|22:review:eng-2");
}

#[test]
fn manager_dispatch_intervention_signature_sorts_all_components() {
    // ReportDispatchSnapshot is private to dispatch module, so we test via the
    // integration tests in daemon.rs or by exercising the public intervention
    // methods. This test is kept as a placeholder.
    // The signature function is tested indirectly through the integration tests.
}

#[test]
fn architect_utilization_intervention_signature_sorts_all_inputs() {
    let harness = triage_harness()
        .with_board_task(60, "task-60", "todo", None)
        .with_board_task(61, "task-61", "backlog", None);
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let task_refs = vec![&tasks[0], &tasks[1]];

    let signature = architect_utilization_intervention_signature(
        &["eng-2".to_string(), "eng-1".to_string()],
        &[
            ("eng-3".to_string(), vec![80, 81]),
            ("eng-4".to_string(), vec![70]),
        ],
        &["eng-5".to_string(), "eng-6".to_string()],
        &task_refs,
    );

    assert_eq!(
        signature,
        "idle-active:eng-3:80,81|idle-active:eng-4:70|idle-free:eng-5|idle-free:eng-6|open:60:todo|open:61:backlog|working:eng-1|working:eng-2"
    );
}

#[test]
fn review_backlog_owner_for_task_prefers_reporting_manager() {
    let task = crate::task::Task {
        id: 42,
        title: "Review task".to_string(),
        status: "review".to_string(),
        priority: "high".to_string(),
        claimed_by: Some("eng-1".to_string()),
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
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
        description: "Task body".to_string(),
        batty_config: None,
        source_path: std::path::PathBuf::from("task-42.md"),
    };
    let members = vec![
        architect_member("architect"),
        manager_member("lead", Some("architect")),
        engineer_member("eng-1", Some("lead"), false),
    ];

    let owner = review_backlog_owner_for_task(&task, &members);

    assert_eq!(owner, Some("lead".to_string()));
}

#[test]
fn review_backlog_owner_for_task_falls_back_to_claimed_by_when_member_missing() {
    let task = crate::task::Task {
        id: 43,
        title: "Review task".to_string(),
        status: "review".to_string(),
        priority: "high".to_string(),
        claimed_by: Some("eng-9".to_string()),
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
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
        description: "Task body".to_string(),
        batty_config: None,
        source_path: std::path::PathBuf::from("task-43.md"),
    };

    let owner = review_backlog_owner_for_task(&task, &[manager_member("lead", None)]);

    assert_eq!(owner, Some("eng-9".to_string()));
}

#[test]
fn review_backlog_owner_for_task_ignores_non_review_tasks() {
    let task = crate::task::Task {
        id: 44,
        title: "Work task".to_string(),
        status: "in-progress".to_string(),
        priority: "high".to_string(),
        claimed_by: Some("eng-1".to_string()),
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
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
        description: "Task body".to_string(),
        batty_config: None,
        source_path: std::path::PathBuf::from("task-44.md"),
    };

    let owner = review_backlog_owner_for_task(&task, &[engineer_member("eng-1", None, false)]);

    assert_eq!(owner, None);
}

#[test]
fn build_owned_task_intervention_message_includes_parent_escalation() {
    let harness = triage_harness().with_board_task(70, "task-70", "in-progress", Some("lead"));
    write_prompt_nudge(harness.project_root(), "manager.md", "Manager nudge text.");
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "lead")
        .unwrap()
        .clone();

    let message = daemon.build_owned_task_intervention_message(
        &member,
        &[&tasks[0]],
        &["eng-1".to_string()],
    );

    assert!(message.contains("Owned active task backlog detected"));
    assert!(message.starts_with("Manager nudge text.\n\n"));
    assert!(message.contains("kanban-md list --dir"));
    assert!(message.contains("batty assign eng-1"));
    assert!(message.contains("batty send architect"));
    assert!(message.contains("kanban-md move --dir"));
}

#[test]
fn build_review_intervention_message_includes_merge_and_rework_paths() {
    let harness = triage_harness().with_board_task(71, "task-71", "review", Some("eng-1"));
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "lead")
        .unwrap()
        .clone();

    let message = daemon.build_review_intervention_message(&member, &[&tasks[0]]);

    assert!(message.contains("Review backlog detected"));
    assert!(message.contains("batty merge eng-1"));
    assert!(message.contains("kanban-md move --dir"));
    assert!(message.contains("batty assign eng-1"));
    assert!(message.contains("batty send architect"));
}

#[test]
fn build_review_intervention_message_prepends_review_policy() {
    let harness = triage_harness().with_board_task(171, "task-171", "review", Some("eng-1"));
    fs::write(
        harness
            .project_root()
            .join(".batty")
            .join("team_config")
            .join("review_policy.md"),
        "Review must confirm tests and scope.",
    )
    .unwrap();
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "lead")
        .unwrap()
        .clone();

    let message = daemon.build_review_intervention_message(&member, &[&tasks[0]]);

    assert!(
        message.starts_with("Review policy context:\nReview must confirm tests and scope.")
    );
}

#[test]
fn build_stuck_task_escalation_message_uses_assign_for_engineer() {
    let harness = triage_harness().with_board_task(72, "task-72", "in-progress", Some("eng-1"));
    write_prompt_nudge(
        harness.project_root(),
        "engineer.md",
        "Engineer nudge text.",
    );
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "eng-1")
        .unwrap()
        .clone();

    let message = daemon.build_stuck_task_escalation_message(&member, &[&tasks[0]], 125);

    assert!(message.contains("Stuck task escalation"));
    assert!(message.contains("2m"));
    assert!(message.contains("batty assign eng-1"));
    assert!(message.contains("batty send lead"));
}

#[test]
fn build_stuck_task_escalation_message_prepends_escalation_policy() {
    let harness =
        triage_harness().with_board_task(172, "task-172", "in-progress", Some("eng-1"));
    fs::write(
        harness
            .project_root()
            .join(".batty")
            .join("team_config")
            .join("escalation_policy.md"),
        "Escalate only with exact blocker text.",
    )
    .unwrap();
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "eng-1")
        .unwrap()
        .clone();

    let message = daemon.build_stuck_task_escalation_message(&member, &[&tasks[0]], 125);

    assert!(
        message
            .starts_with("Escalation policy context:\nEscalate only with exact blocker text.")
    );
}

#[test]
fn build_manager_dispatch_gap_message_includes_active_and_unassigned_paths() {
    let harness = triage_harness()
        .with_board_task(80, "task-80", "todo", None)
        .with_board_task(81, "task-81", "backlog", None);
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "lead")
        .unwrap();

    // Can't create ReportDispatchSnapshot directly since it's private to dispatch.
    // Test this via the integration test: maybe_intervene_manager_dispatch_gap_queues_for_idle_lead.
}

#[test]
fn build_architect_utilization_message_includes_recovery_commands() {
    let harness = triage_harness().with_board_task(90, "task-90", "todo", None);
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let architect = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "architect")
        .unwrap();

    let message = daemon.build_architect_utilization_message(
        architect,
        &["eng-2".to_string()],
        &[("eng-1".to_string(), vec![11])],
        &["eng-2".to_string()],
        &[&tasks[0]],
    );

    assert!(message.contains("Utilization recovery needed"));
    assert!(message.contains("batty send lead"));
    assert!(message.contains("Task #90"));
    assert!(message.contains("batty send"));
}

#[test]
fn build_architect_utilization_message_reloads_replenishment_context() {
    let harness = triage_harness().with_board_task(190, "task-190", "todo", None);
    let directive_path = harness
        .project_root()
        .join(".batty")
        .join("team_config")
        .join("replenishment_context.md");
    fs::write(&directive_path, "First directive").unwrap();
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let architect = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "architect")
        .unwrap();

    let first = daemon.build_architect_utilization_message(
        architect,
        &["eng-2".to_string()],
        &[("eng-1".to_string(), vec![11])],
        &["eng-2".to_string()],
        &[&tasks[0]],
    );

    fs::write(&directive_path, "Updated directive").unwrap();

    let second = daemon.build_architect_utilization_message(
        architect,
        &["eng-2".to_string()],
        &[("eng-1".to_string(), vec![11])],
        &["eng-2".to_string()],
        &[&tasks[0]],
    );

    assert!(first.contains("First directive"));
    assert!(second.contains("Updated directive"));
    assert!(!second.contains("First directive"));
}

#[test]
fn build_review_intervention_message_truncates_long_policy() {
    let harness = triage_harness().with_board_task(173, "task-173", "review", Some("eng-1"));
    fs::write(
        harness
            .project_root()
            .join(".batty")
            .join("team_config")
            .join("review_policy.md"),
        "x".repeat(DIRECTIVE_MAX_CHARS + 50),
    )
    .unwrap();
    let daemon = harness.build_daemon().unwrap();
    let tasks = crate::task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();
    let member = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "lead")
        .unwrap()
        .clone();

    let message = daemon.build_review_intervention_message(&member, &[&tasks[0]]);

    assert!(message.contains("[truncated to 2000 chars from review_policy.md]"));
}

#[test]
fn nudge_disabled_marker_suppresses_triage_intervention() {
    use crate::team::nudge_disabled_marker_path;

    let harness =
        triage_harness().with_inbox_message("lead", delivered_result("eng-1", "done"), true);
    let mut daemon = harness.build_daemon().unwrap();
    enter_idle_epoch(&mut daemon, "lead");

    // Create the nudge disabled marker
    let marker = nudge_disabled_marker_path(&daemon.config.project_root, "triage");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "").unwrap();

    daemon.maybe_intervene_triage_backlog().unwrap();

    // No triage intervention should have fired
    assert_eq!(daemon.triage_interventions.get("lead"), None);
    let pending = harness.pending_inbox_messages("lead").unwrap();
    assert!(pending.is_empty());
}

#[test]
fn nudge_disabled_marker_suppresses_board_replenishment() {
    use crate::team::nudge_disabled_marker_path;

    let harness = intervention_team_harness()
        .with_member_state("architect", MemberState::Idle)
        .with_member_state("eng-1", MemberState::Idle);
    let mut daemon = harness.build_daemon().unwrap();
    daemon.config.team_config.automation.replenishment_threshold = Some(10);

    // Create the nudge disabled marker
    let marker = nudge_disabled_marker_path(&daemon.config.project_root, "replenish");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "").unwrap();

    daemon.maybe_intervene_board_replenishment().unwrap();

    let pending = harness.pending_inbox_messages("architect").unwrap();
    assert!(pending.is_empty());
}
