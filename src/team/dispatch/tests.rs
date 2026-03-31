use super::*;
use crate::shim::protocol::{Command, ShimState, socketpair};
use crate::team::config::{BoardConfig, WorkflowPolicy};
use crate::team::daemon::agent_handle::AgentHandle;
use crate::team::inbox;
use crate::team::standup::MemberState;
use crate::team::task_loop::{engineer_base_branch_name, setup_engineer_worktree};
use crate::team::test_support::{
    TestDaemonBuilder, engineer_member, init_git_repo, manager_member, write_open_task_file,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[test]
fn engineer_task_branch_name_uses_explicit_task_id() {
    assert_eq!(
        engineer_task_branch_name("eng-1-3", "freeform task body", Some(123)),
        "eng-1-3/123"
    );
}

#[test]
fn engineer_task_branch_name_extracts_task_id_from_assignment_text() {
    assert_eq!(
        engineer_task_branch_name("eng-1-3", "Task #456: fix move generation", None),
        "eng-1-3/456"
    );
}

#[test]
fn engineer_task_branch_name_falls_back_to_slugged_branch() {
    let branch = engineer_task_branch_name("eng-1-3", "Fix castling rights sync", None);
    assert!(branch.starts_with("eng-1-3/task-fix-castling-rights-sy"));
}

#[test]
fn summarize_assignment_uses_first_non_empty_line() {
    assert_eq!(
        summarize_assignment("\n\nTask #9: fix move ordering\n\nDetails below"),
        "Task #9: fix move ordering"
    );
}

#[test]
fn shim_assignment_sends_message_to_existing_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(
        tmp.path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )
    .unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .build();
    daemon.config.team_config.use_shim = true;

    let (parent_sock, child_sock) = socketpair().unwrap();
    let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
    let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
    let mut handle = AgentHandle::new(
        "eng-1".to_string(),
        parent_channel,
        12345,
        "codex".to_string(),
        "codex".to_string(),
        tmp.path().to_path_buf(),
    );
    handle.apply_state_change(ShimState::Idle);
    daemon.shim_handles.insert("eng-1".to_string(), handle);

    let launch = daemon
        .assign_task_with_task_id_as("manager", "eng-1", "Task #42: fix it", Some(42))
        .unwrap();

    let cmd: Command = child_channel.recv().unwrap().unwrap();
    match cmd {
        Command::SendMessage { from, body, .. } => {
            assert_eq!(from, "manager");
            assert_eq!(body, "Task #42: fix it");
        }
        other => panic!("expected SendMessage, got {other:?}"),
    }

    // Shim-managed agents: state driven by shim events, not speculative mark_member_working
    assert_ne!(daemon.states.get("eng-1"), Some(&MemberState::Working));
    assert_eq!(launch.branch, None);
    assert_eq!(launch.work_dir, tmp.path());
}

#[test]
fn assignment_guard_rejects_second_active_task_for_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    crate::team::test_support::write_owned_task_file(
        tmp.path(),
        91,
        "active-task",
        "in-progress",
        "eng-1",
    );
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .build();
    daemon.config.team_config.use_shim = true;

    let (parent_sock, _child_sock) = socketpair().unwrap();
    let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
    let mut handle = AgentHandle::new(
        "eng-1".to_string(),
        parent_channel,
        12345,
        "codex".to_string(),
        "codex".to_string(),
        tmp.path().to_path_buf(),
    );
    handle.apply_state_change(ShimState::Idle);
    daemon.shim_handles.insert("eng-1".to_string(), handle);

    let error = daemon
        .assign_task_with_task_id_as("manager", "eng-1", "Task #42: fix it", Some(42))
        .unwrap_err()
        .to_string();

    assert!(error.contains("already owns active board task(s) #91"));
}

#[test]
fn assignment_guard_allows_resuming_same_active_task() {
    let tmp = tempfile::tempdir().unwrap();
    crate::team::test_support::write_owned_task_file(
        tmp.path(),
        91,
        "active-task",
        "in-progress",
        "eng-1",
    );
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .build();
    daemon.config.team_config.use_shim = true;

    let (parent_sock, child_sock) = socketpair().unwrap();
    let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
    let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
    let mut handle = AgentHandle::new(
        "eng-1".to_string(),
        parent_channel,
        12345,
        "codex".to_string(),
        "codex".to_string(),
        tmp.path().to_path_buf(),
    );
    handle.apply_state_change(ShimState::Idle);
    daemon.shim_handles.insert("eng-1".to_string(), handle);

    daemon
        .assign_task_with_task_id_as("manager", "eng-1", "Task #91: continue", Some(91))
        .unwrap();

    let cmd: Command = child_channel.recv().unwrap().unwrap();
    match cmd {
        Command::SendMessage { from, body, .. } => {
            assert_eq!(from, "manager");
            assert_eq!(body, "Task #91: continue");
        }
        other => panic!("expected SendMessage, got {other:?}"),
    }
}

#[test]
fn stabilization_delay_prevents_premature_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 30,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon
        .idle_started_at
        .insert("eng-1".to_string(), Instant::now() - Duration::from_secs(5));

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(daemon.dispatch_queue.len(), 1);
    assert_eq!(daemon.dispatch_queue[0].validation_failures, 0);
    assert_eq!(daemon.dispatch_queue[0].task_id, 101);
}

#[test]
fn wip_gate_blocks_double_assignment() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    crate::team::test_support::write_owned_task_file(
        tmp.path(),
        91,
        "active-task",
        "in-progress",
        "eng-1",
    );
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .workflow_policy(WorkflowPolicy {
            wip_limit_per_engineer: Some(1),
            ..WorkflowPolicy::default()
        })
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(daemon.dispatch_queue.len(), 1);
    assert_eq!(daemon.dispatch_queue[0].validation_failures, 1);
    assert!(
        daemon.dispatch_queue[0]
            .last_failure
            .as_deref()
            .unwrap_or_default()
            .contains("Dispatch guard")
    );
}

#[test]
fn dispatch_guard_blocks_claimed_todo_assignment() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    crate::team::test_support::write_owned_task_file(
        tmp.path(),
        91,
        "claimed-todo",
        "todo",
        "eng-1",
    );
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(daemon.dispatch_queue.len(), 1);
    assert_eq!(daemon.dispatch_queue[0].validation_failures, 1);
    assert!(
        daemon.dispatch_queue[0]
            .last_failure
            .as_deref()
            .unwrap_or_default()
            .contains("Dispatch guard blocked assignment")
    );
}

#[test]
fn active_board_item_count_includes_todo_in_progress_and_review() {
    let tmp = tempfile::tempdir().unwrap();
    crate::team::test_support::write_owned_task_file(tmp.path(), 11, "todo-task", "todo", "eng-1");
    crate::team::test_support::write_owned_task_file(
        tmp.path(),
        12,
        "working-task",
        "in-progress",
        "eng-1",
    );
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("013-review-task.md"),
        "---\nid: 13\ntitle: review-task\nstatus: review\npriority: critical\nclaimed_by: manager\nreview_owner: eng-1\nclass: standard\n---\n\nTask description.\n",
    )
    .unwrap();
    let daemon = TestDaemonBuilder::new(tmp.path()).build();
    let board_dir = tmp.path().join(".batty").join("team_config").join("board");

    assert_eq!(
        daemon
            .engineer_active_board_item_count(&board_dir, "eng-1")
            .unwrap(),
        3
    );
}

#[test]
fn worktree_gate_blocks_dirty_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "dispatch-queue");
    write_open_task_file(&repo, 101, "queued-task", "todo");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &engineer_base_branch_name("eng-1"),
        &team_config_dir,
    )
    .unwrap();
    std::fs::write(worktree_dir.join("DIRTY.txt"), "dirty\n").unwrap();
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), true),
    ];
    let mut daemon = TestDaemonBuilder::new(&repo)
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    // Worktree auto-recovery cleans dirty files and resets to base branch,
    // so the entry stays in the queue with failures cleared for retry.
    assert_eq!(daemon.dispatch_queue.len(), 1);
    assert_eq!(
        daemon.dispatch_queue[0].validation_failures, 0,
        "auto-recovery should clear failure count after successful reset"
    );
    assert!(
        daemon.dispatch_queue[0].last_failure.is_none(),
        "auto-recovery should clear failure message after successful reset"
    );
}

#[test]
fn queue_escalates_after_repeated_validation_failures() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    crate::team::test_support::write_owned_task_file(
        tmp.path(),
        91,
        "active-task",
        "in-progress",
        "eng-1",
    );
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .workflow_policy(WorkflowPolicy {
            wip_limit_per_engineer: Some(1),
            ..WorkflowPolicy::default()
        })
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([
            ("eng-1".to_string(), MemberState::Idle),
            ("manager".to_string(), MemberState::Idle),
        ]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    for _ in 0..DISPATCH_QUEUE_FAILURE_LIMIT {
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon.maybe_auto_dispatch().unwrap();
    }

    assert!(
        daemon.dispatch_queue.is_empty(),
        "queue should be drained after failure limit"
    );
    // With the reassign-or-drop fix, blocked entries are silently dropped
    // (no escalation to manager) since auto-dispatch will re-queue when
    // the engineer frees up.
    let inbox_root = inbox::inboxes_root(tmp.path());
    let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert_eq!(manager_messages.len(), 0);
}

#[test]
fn cwd_correction_handles_symlinks() {
    // On macOS, /tmp is a symlink to /private/tmp.  normalized_assignment_dir
    // must resolve both to the same canonical path so that comparisons succeed
    // even when tmux reports the symlinked variant.
    let tmp = tempfile::tempdir().unwrap();
    let canonical = tmp.path().canonicalize().unwrap();

    // normalized_assignment_dir should resolve to the same canonical path
    // regardless of which representation is passed in.
    let from_canonical = normalized_assignment_dir(&canonical);
    let from_raw = normalized_assignment_dir(tmp.path());
    assert_eq!(from_canonical, from_raw);

    // Simulate the macOS /tmp vs /private/tmp scenario: if the temp dir
    // lives under /private/var (or /private/tmp), stripping the /private
    // prefix would give a different path that still resolves identically.
    #[cfg(target_os = "macos")]
    {
        let path_str = canonical.to_string_lossy();
        if path_str.starts_with("/private/") {
            let without_private = PathBuf::from(&path_str["/private".len()..]);
            let normalized = normalized_assignment_dir(&without_private);
            assert_eq!(normalized, from_canonical);
        }
    }
}

#[test]
fn cwd_correction_normalizes_nonexistent_paths_to_self() {
    // When canonicalize fails (path doesn't exist), the function should
    // fall back to the original path unchanged.
    let bogus = PathBuf::from("/nonexistent/path/that/does/not/exist");
    let normalized = normalized_assignment_dir(&bogus);
    assert_eq!(normalized, bogus);
}

#[test]
fn cwd_correction_retries_on_stale_read() {
    // Verify the retry constants are sensible for the cwd correction loop.
    // The actual retry logic is integration-tested via tmux sessions, but
    // we validate that the path comparison logic used in each retry attempt
    // correctly identifies matching vs non-matching paths.
    let tmp = tempfile::tempdir().unwrap();
    let expected = tmp.path().to_path_buf();
    let normalized_expected = normalized_assignment_dir(&expected);

    // Simulate "stale read" — pane initially reports project root
    let project_root = tmp.path().parent().unwrap_or(tmp.path());
    let stale_path = normalized_assignment_dir(project_root);
    assert_ne!(
        stale_path, normalized_expected,
        "stale path should differ from expected"
    );

    // Simulate "corrected read" — pane eventually reports the worktree dir
    let corrected = normalized_assignment_dir(&expected);
    assert_eq!(
        corrected, normalized_expected,
        "corrected path should match expected"
    );

    // Codex context subdirectory is also accepted as valid
    let codex_dir = expected.join(".batty").join("codex-context").join("eng-1");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let codex_normalized = normalized_assignment_dir(&codex_dir);
    // Codex dir should NOT equal the expected root — it's a separate valid path
    assert_ne!(codex_normalized, normalized_expected);
}

fn write_scheduled_task_file(
    project_root: &Path,
    id: u32,
    title: &str,
    status: &str,
    scheduled_for: &str,
) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{id:03}-{title}.md")),
        format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\nscheduled_for: \"{scheduled_for}\"\nclass: standard\n---\n\nTask description.\n"
        ),
    )
    .unwrap();
}

#[test]
fn dispatch_skips_future_scheduled_task() {
    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let tmp = tempfile::tempdir().unwrap();
    write_scheduled_task_file(tmp.path(), 101, "future-task", "todo", &future);
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert!(
        daemon.dispatch_queue.is_empty(),
        "future-scheduled task should not be dispatched"
    );
}

#[test]
fn dispatch_includes_past_scheduled_task() {
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let tmp = tempfile::tempdir().unwrap();
    write_scheduled_task_file(tmp.path(), 101, "past-task", "todo", &past);
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(daemon.dispatch_queue.len(), 1);
    assert_eq!(daemon.dispatch_queue[0].task_id, 101);
}

#[test]
fn dispatch_skips_worktree_prep_for_working_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "skip-prep");
    write_open_task_file(&repo, 201, "new-task", "todo");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &engineer_base_branch_name("eng-1"),
        &team_config_dir,
    )
    .unwrap();
    // Create dirty (uncommitted) file in the worktree
    std::fs::write(
        worktree_dir.join("uncommitted-work.txt"),
        "important work\n",
    )
    .unwrap();

    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), true),
    ];
    let mut daemon = TestDaemonBuilder::new(&repo)
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.active_tasks.insert("eng-1".to_string(), 100);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    // Working engineer should NOT have dispatch entries processed
    assert!(
        daemon.dispatch_queue.is_empty()
            || daemon
                .dispatch_queue
                .iter()
                .all(|e| e.validation_failures == 0),
        "working engineer's dispatch entry should be retained without validation failure"
    );
    // Critical invariant: uncommitted work preserved
    assert!(
        worktree_dir.join("uncommitted-work.txt").exists(),
        "uncommitted work must survive — worktree prep should not have run"
    );
}

#[test]
fn dispatch_preps_worktree_for_idle_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "idle-prep");
    write_open_task_file(&repo, 301, "idle-task", "todo");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &engineer_base_branch_name("eng-1"),
        &team_config_dir,
    )
    .unwrap();

    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), true),
    ];
    let mut daemon = TestDaemonBuilder::new(&repo)
        .members(members)
        .pane_map(HashMap::from([("eng-1".to_string(), "%99".to_string())]))
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    // Task branch should have been created by prepare_engineer_assignment_worktree
    let task_branch = "eng-1/301";
    let branches = crate::team::test_support::git_stdout(&repo, &["branch", "--list"]);
    assert!(
        branches.contains(task_branch),
        "idle engineer should have task branch created by worktree prep; branches: {branches}"
    );
}

// -- End-to-end scheduled tasks tests (task #203) --

#[test]
fn e2e_past_scheduled_for_is_dispatchable() {
    use crate::team::resolver::{ResolutionStatus, resolve_board};

    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let tmp = tempfile::tempdir().unwrap();
    write_scheduled_task_file(tmp.path(), 501, "past-sched", "todo", &past);

    // Step 1: resolve_board sees it as Runnable
    let board_dir = tmp.path().join(".batty").join("team_config").join("board");
    let members_list = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let resolutions = resolve_board(&board_dir, &members_list).unwrap();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(
        resolutions[0].status,
        ResolutionStatus::Runnable,
        "past scheduled_for task should be Runnable"
    );

    // Step 2: next_dispatch_task (via maybe_auto_dispatch) returns the task
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members_list)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();
    assert_eq!(
        daemon.dispatch_queue.len(),
        1,
        "past-scheduled task should be dispatched"
    );
    assert_eq!(daemon.dispatch_queue[0].task_id, 501);
}

#[test]
fn e2e_future_scheduled_for_is_blocked() {
    use crate::team::resolver::{ResolutionStatus, resolve_board};

    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let tmp = tempfile::tempdir().unwrap();
    write_scheduled_task_file(tmp.path(), 502, "future-sched", "todo", &future);

    // Step 1: resolve_board sees it as Blocked with 'scheduled for' reason
    let board_dir = tmp.path().join(".batty").join("team_config").join("board");
    let members_list = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let resolutions = resolve_board(&board_dir, &members_list).unwrap();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(
        resolutions[0].status,
        ResolutionStatus::Blocked,
        "future scheduled_for task should be Blocked"
    );
    assert!(
        resolutions[0]
            .blocking_reason
            .as_ref()
            .unwrap()
            .contains("scheduled for"),
        "blocking reason should mention 'scheduled for'"
    );

    // Step 2: next_dispatch_task (via maybe_auto_dispatch) does NOT return the task
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members_list)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();
    assert!(
        daemon.dispatch_queue.is_empty(),
        "future-scheduled task should NOT be dispatched"
    );
}

#[test]
fn e2e_no_scheduled_for_always_runnable() {
    use crate::team::resolver::{ResolutionStatus, resolve_board};

    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 503, "no-schedule", "todo");

    // Step 1: resolve_board sees it as Runnable
    let board_dir = tmp.path().join(".batty").join("team_config").join("board");
    let members_list = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let resolutions = resolve_board(&board_dir, &members_list).unwrap();
    assert_eq!(resolutions.len(), 1);
    assert_eq!(
        resolutions[0].status,
        ResolutionStatus::Runnable,
        "task without scheduled_for should be Runnable"
    );
    assert!(
        resolutions[0].blocking_reason.is_none(),
        "no blocking reason expected"
    );

    // Step 2: next_dispatch_task (via maybe_auto_dispatch) returns the task
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members_list)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();
    assert_eq!(
        daemon.dispatch_queue.len(),
        1,
        "task without scheduled_for should be dispatched"
    );
    assert_eq!(daemon.dispatch_queue[0].task_id, 503);
}

#[test]
fn dedup_window_prevents_duplicate_enqueue() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_dedup_window_secs: 60,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    // Simulate a recent dispatch for this (task_id, engineer) pair.
    daemon
        .recent_dispatches
        .insert((101, "eng-1".to_string()), Instant::now());

    daemon.maybe_auto_dispatch().unwrap();

    assert!(
        daemon.dispatch_queue.is_empty(),
        "task should be skipped due to dedup window"
    );
}

#[test]
fn dedup_window_allows_different_task() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "first-task", "todo");
    write_open_task_file(tmp.path(), 102, "second-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_dedup_window_secs: 60,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    // Recent dispatch for task 102, but not 101.
    daemon
        .recent_dispatches
        .insert((102, "eng-1".to_string()), Instant::now());

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(daemon.dispatch_queue.len(), 1);
    assert_eq!(
        daemon.dispatch_queue[0].task_id, 101,
        "a different task should still be dispatched"
    );
}

#[test]
fn dedup_window_expires_and_allows_reassignment() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_dedup_window_secs: 60,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    // Simulate a dispatch that happened 120 seconds ago — outside the 60s window.
    daemon.recent_dispatches.insert(
        (101, "eng-1".to_string()),
        Instant::now() - Duration::from_secs(120),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(
        daemon.dispatch_queue.len(),
        1,
        "expired dedup entry should allow reassignment"
    );
    assert_eq!(daemon.dispatch_queue[0].task_id, 101);
}

#[test]
fn dedup_window_zero_disables_dedup() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_dedup_window_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    // With window=0, even a "just now" entry should be expired immediately.
    daemon
        .recent_dispatches
        .insert((101, "eng-1".to_string()), Instant::now());

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(
        daemon.dispatch_queue.len(),
        1,
        "dedup_window_secs=0 should effectively disable dedup"
    );
    assert_eq!(daemon.dispatch_queue[0].task_id, 101);
}

#[test]
fn manual_cooldown_blocks_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_manual_cooldown_secs: 30,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );
    daemon
        .manual_assign_cooldowns
        .insert("eng-1".to_string(), Instant::now());

    daemon.maybe_auto_dispatch().unwrap();

    assert!(
        daemon.dispatch_queue.is_empty(),
        "engineer within manual cooldown should not be enqueued"
    );
}

#[test]
fn manual_cooldown_expires_and_allows_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_manual_cooldown_secs: 30,
            ..BoardConfig::default()
        })
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );
    daemon.manual_assign_cooldowns.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(
        daemon.dispatch_queue.len(),
        1,
        "expired manual cooldown should allow dispatch"
    );
    assert_eq!(daemon.dispatch_queue[0].task_id, 101);
}

#[test]
fn manual_cooldown_only_affects_assigned_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
        engineer_member("eng-2", Some("manager"), false),
    ];
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_manual_cooldown_secs: 30,
            ..BoardConfig::default()
        })
        .states(HashMap::from([
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );
    daemon.idle_started_at.insert(
        "eng-2".to_string(),
        Instant::now() - Duration::from_secs(60),
    );
    daemon
        .manual_assign_cooldowns
        .insert("eng-1".to_string(), Instant::now());

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(
        daemon.dispatch_queue.len(),
        1,
        "only the non-cooldown engineer should be enqueued"
    );
    assert_eq!(daemon.dispatch_queue[0].engineer, "eng-2");
}

// -- dispatch_priority_rank tests --

#[test]
fn priority_rank_critical() {
    assert_eq!(super::dispatch_priority_rank("critical"), 0);
}

#[test]
fn priority_rank_high() {
    assert_eq!(super::dispatch_priority_rank("high"), 1);
}

#[test]
fn priority_rank_medium() {
    assert_eq!(super::dispatch_priority_rank("medium"), 2);
}

#[test]
fn priority_rank_low() {
    assert_eq!(super::dispatch_priority_rank("low"), 3);
}

#[test]
fn priority_rank_unknown_defaults_highest_number() {
    assert_eq!(super::dispatch_priority_rank(""), 4);
    assert_eq!(super::dispatch_priority_rank("urgent"), 4);
}

// -- parse_assignment_task_id tests --

#[test]
fn parse_task_id_standard_format() {
    assert_eq!(
        super::parse_assignment_task_id("Task #42: do stuff"),
        Some(42)
    );
}

#[test]
fn parse_task_id_no_match() {
    assert_eq!(super::parse_assignment_task_id("just a message"), None);
}

#[test]
fn parse_task_id_case_insensitive() {
    assert_eq!(
        super::parse_assignment_task_id("TASK #99: uppercase"),
        Some(99)
    );
    assert_eq!(
        super::parse_assignment_task_id("task #77: lowercase"),
        Some(77)
    );
}

#[test]
fn parse_task_id_multiple_returns_first() {
    assert_eq!(
        super::parse_assignment_task_id("Task #10: depends on Task #5"),
        Some(10)
    );
}

#[test]
fn parse_task_id_empty_digits() {
    assert_eq!(super::parse_assignment_task_id("Task #: no digits"), None);
}

// -- slugify_task_branch tests --

#[test]
fn slugify_basic_text() {
    assert_eq!(
        super::slugify_task_branch("Fix castling rights"),
        "fix-castling-rights"
    );
}

#[test]
fn slugify_special_characters() {
    assert_eq!(
        super::slugify_task_branch("Add feature (v2) — fix!"),
        "add-feature-v2-fix"
    );
}

#[test]
fn slugify_empty_returns_task() {
    assert_eq!(super::slugify_task_branch(""), "task");
}

#[test]
fn slugify_only_special_chars() {
    assert_eq!(super::slugify_task_branch("--- !!!"), "task");
}

#[test]
fn slugify_preserves_numbers() {
    assert_eq!(super::slugify_task_branch("Task 42 fix"), "task-42-fix");
}

// -- summarize_assignment edge cases --

#[test]
fn summarize_empty_body() {
    assert_eq!(super::summarize_assignment(""), "task");
}

#[test]
fn summarize_only_whitespace() {
    assert_eq!(super::summarize_assignment("   \n  \n  "), "task");
}

#[test]
fn summarize_truncates_long_line() {
    let long = "a".repeat(200);
    let result = super::summarize_assignment(&long);
    assert_eq!(result.len(), 120);
    assert!(result.ends_with("..."));
}

#[test]
fn summarize_preserves_short_line() {
    assert_eq!(super::summarize_assignment("Short task"), "Short task");
}

// -- DispatchQueueEntry serde roundtrip --

#[test]
fn dispatch_queue_entry_serde_roundtrip() {
    use super::DispatchQueueEntry;

    let entry = DispatchQueueEntry {
        engineer: "eng-1".to_string(),
        task_id: 42,
        task_title: "Test task".to_string(),
        queued_at: 1234567890,
        validation_failures: 2,
        last_failure: Some("worktree dirty".to_string()),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let restored: DispatchQueueEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, entry);
}

#[test]
fn dispatch_queue_entry_serde_no_failure() {
    use super::DispatchQueueEntry;

    let entry = DispatchQueueEntry {
        engineer: "eng-2".to_string(),
        task_id: 1,
        task_title: "Clean task".to_string(),
        queued_at: 0,
        validation_failures: 0,
        last_failure: None,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let restored: DispatchQueueEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, entry);
}
