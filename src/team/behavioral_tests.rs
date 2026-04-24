//! Behavioral verification tests — cross-module workflow tests without tmux.
//!
//! These tests verify end-to-end workflows by exercising multiple modules
//! together. No tmux dependency — pure state machine / logic tests.

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::team::auto_merge::{
        AutoMergeDecision, DiffSummary, compute_merge_confidence, should_auto_merge,
    };
    use crate::team::completion::{CompletionPacket, parse_completion, validate_completion};
    use crate::team::config::{AutoMergePolicy, RoleType, WorkflowPolicy};
    use crate::team::merge::handle_engineer_completion;
    use crate::team::messaging::send_message_as;
    use crate::team::policy::{
        check_wip_limit, is_review_nudge_due, is_review_stale, should_escalate,
    };
    use crate::team::resolver::{ResolutionStatus, resolve_board, runnable_tasks};
    use crate::team::review::{MergeDisposition, apply_review};
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{
        engineer_member, git_ok, init_git_repo, manager_member, write_board_task_file,
    };
    use crate::team::workflow::{TaskState, WorkflowMeta, can_transition};

    // -----------------------------------------------------------------------
    // 1. Task lifecycle: todo → assigned → in-progress → completed → done
    //    with all events emitted correctly
    // -----------------------------------------------------------------------

    #[test]
    fn full_task_lifecycle_transitions() {
        let mut meta = WorkflowMeta {
            state: TaskState::Backlog,
            ..WorkflowMeta::default()
        };

        // Backlog → Todo (triage)
        meta.transition(TaskState::Todo).unwrap();
        assert_eq!(meta.state, TaskState::Todo);

        // Todo → InProgress (engineer picks it up)
        meta.transition(TaskState::InProgress).unwrap();
        assert_eq!(meta.state, TaskState::InProgress);
        meta.execution_owner = Some("eng-1".to_string());

        // InProgress → Review (engineer submits)
        meta.transition(TaskState::Review).unwrap();
        assert_eq!(meta.state, TaskState::Review);
        meta.review_owner = Some("manager-1".to_string());

        // Review → Done (approved)
        meta.transition(TaskState::Done).unwrap();
        assert_eq!(meta.state, TaskState::Done);

        // Done → Archived (cleanup)
        meta.transition(TaskState::Archived).unwrap();
        assert_eq!(meta.state, TaskState::Archived);
    }

    #[test]
    fn task_lifecycle_review_rework_loop() {
        let mut meta = WorkflowMeta {
            state: TaskState::Todo,
            ..WorkflowMeta::default()
        };

        meta.transition(TaskState::InProgress).unwrap();
        meta.transition(TaskState::Review).unwrap();

        // Review → InProgress (changes requested)
        meta.transition(TaskState::InProgress).unwrap();
        assert_eq!(meta.state, TaskState::InProgress);

        // InProgress → Review (resubmit)
        meta.transition(TaskState::Review).unwrap();

        // Review → Done (approved on second pass)
        meta.transition(TaskState::Done).unwrap();
        assert_eq!(meta.state, TaskState::Done);
    }

    #[test]
    fn task_lifecycle_blocked_then_unblocked() {
        let mut meta = WorkflowMeta {
            state: TaskState::Todo,
            ..WorkflowMeta::default()
        };

        // Block from Todo
        meta.transition(TaskState::Blocked).unwrap();
        assert_eq!(meta.state, TaskState::Blocked);
        meta.blocked_on = Some("waiting for API".to_string());

        // Unblock → back to Todo
        meta.transition(TaskState::Todo).unwrap();
        meta.blocked_on = None;

        // Progress, block from InProgress, unblock
        meta.transition(TaskState::InProgress).unwrap();
        meta.transition(TaskState::Blocked).unwrap();
        meta.blocked_on = Some("infrastructure issue".to_string());
        meta.transition(TaskState::InProgress).unwrap();
        meta.blocked_on = None;

        // Continue to completion
        meta.transition(TaskState::Review).unwrap();
        meta.transition(TaskState::Done).unwrap();
        assert_eq!(meta.state, TaskState::Done);
    }

    #[test]
    fn illegal_lifecycle_skips_are_rejected() {
        assert!(can_transition(TaskState::Backlog, TaskState::Done).is_err());
        assert!(can_transition(TaskState::Backlog, TaskState::InProgress).is_err());
        assert!(can_transition(TaskState::Todo, TaskState::Done).is_err());
        assert!(can_transition(TaskState::InProgress, TaskState::Done).is_err());
        assert!(can_transition(TaskState::Done, TaskState::InProgress).is_err());
        assert!(can_transition(TaskState::Archived, TaskState::Todo).is_err());
    }

    #[test]
    fn review_apply_transitions_state_correctly() {
        // MergeReady → Done
        let mut meta = WorkflowMeta {
            state: TaskState::Review,
            execution_owner: Some("eng-1".to_string()),
            review_owner: Some("mgr".to_string()),
            ..WorkflowMeta::default()
        };
        apply_review(&mut meta, MergeDisposition::MergeReady, "mgr").unwrap();
        assert_eq!(meta.state, TaskState::Done);

        // ReworkRequired → InProgress
        let mut meta2 = WorkflowMeta {
            state: TaskState::Review,
            execution_owner: Some("eng-1".to_string()),
            ..WorkflowMeta::default()
        };
        apply_review(&mut meta2, MergeDisposition::ReworkRequired, "mgr").unwrap();
        assert_eq!(meta2.state, TaskState::InProgress);

        // Discarded → Archived
        let mut meta3 = WorkflowMeta {
            state: TaskState::Review,
            ..WorkflowMeta::default()
        };
        apply_review(&mut meta3, MergeDisposition::Discarded, "mgr").unwrap();
        assert_eq!(meta3.state, TaskState::Archived);

        // Escalated → Blocked
        let mut meta4 = WorkflowMeta {
            state: TaskState::Review,
            ..WorkflowMeta::default()
        };
        apply_review(&mut meta4, MergeDisposition::Escalated, "mgr").unwrap();
        assert_eq!(meta4.state, TaskState::Blocked);
    }

    #[test]
    fn completion_packet_validates_and_integrates() {
        let json = r#"{
            "task_id": 42,
            "branch": "eng-1/42",
            "worktree_path": "/tmp/eng-1",
            "commit": "abc1234",
            "changed_paths": ["src/foo.rs"],
            "tests_run": true,
            "tests_passed": true,
            "artifacts": [],
            "outcome": "ready_for_review"
        }"#;

        let packet = parse_completion(json).unwrap();
        assert_eq!(packet.task_id, 42);
        assert_eq!(packet.branch.as_deref(), Some("eng-1/42"));

        let validation = validate_completion(&packet);
        assert!(validation.is_complete);
        assert!(validation.missing_fields.is_empty());
    }

    #[test]
    fn incomplete_completion_packet_flags_missing_fields() {
        let packet = CompletionPacket {
            task_id: 0,
            branch: None,
            worktree_path: None,
            commit: None,
            changed_paths: vec![],
            tests_run: false,
            tests_passed: false,
            artifacts: vec![],
            outcome: String::new(),
        };

        let validation = validate_completion(&packet);
        assert!(!validation.is_complete);
        assert!(validation.missing_fields.contains(&"task_id".to_string()));
        assert!(validation.missing_fields.contains(&"branch".to_string()));
        assert!(validation.missing_fields.contains(&"commit".to_string()));
        assert!(validation.missing_fields.contains(&"tests_run".to_string()));
    }

    #[test]
    fn review_then_completion_then_done_flow() {
        // Full flow: state machine transition + completion packet + review
        let mut meta = WorkflowMeta {
            state: TaskState::Todo,
            depends_on: vec![],
            ..WorkflowMeta::default()
        };

        // Engineer works on it
        meta.transition(TaskState::InProgress).unwrap();
        meta.execution_owner = Some("eng-1".to_string());

        // Engineer produces a completion packet
        let packet = CompletionPacket {
            task_id: 42,
            branch: Some("eng-1/42".to_string()),
            worktree_path: Some("/tmp/eng-1".to_string()),
            commit: Some("abc1234".to_string()),
            changed_paths: vec!["src/foo.rs".to_string()],
            tests_run: true,
            tests_passed: true,
            artifacts: vec![],
            outcome: "ready_for_review".to_string(),
        };
        let validation = validate_completion(&packet);
        assert!(validation.is_complete, "packet should be valid");

        // Move to Review
        meta.transition(TaskState::Review).unwrap();
        meta.branch = packet.branch;
        meta.commit = packet.commit;

        // Reviewer approves
        apply_review(&mut meta, MergeDisposition::MergeReady, "mgr").unwrap();
        assert_eq!(meta.state, TaskState::Done);
    }

    #[test]
    fn completion_packet_ingestion_closes_the_verification_retry_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-behavioral-completion-loop");
        write_board_task_file(
            &repo,
            42,
            "behavioral-completion-loop",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs"]);
        git_ok(
            &worktree_dir,
            &["commit", "-m", "add failing behavioral test"],
        );

        send_message_as(
            &repo,
            Some("eng-1"),
            "manager",
            r#"Done.

## Completion Packet

```json
{"task_id":42,"branch":"eng-1/task-42","worktree_path":".batty/worktrees/eng-1","commit":"abc1234","changed_paths":["src/lib.rs"],"tests_run":true,"tests_passed":true,"artifacts":[],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap();

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-behavioral-completion-loop.md");
        let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1/task-42"));
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(false));
        assert_eq!(
            metadata.outcome.as_deref(),
            Some("verification_retry_required")
        );
        assert!(
            metadata
                .artifacts
                .iter()
                .any(|artifact| artifact.ends_with("task-042-eng-1-attempt-1.json"))
        );
    }

    // -----------------------------------------------------------------------
    // 2. Auto-merge policy: evaluates correctly for various diff
    //    characteristics (small/large, tests pass/fail, migration files)
    // -----------------------------------------------------------------------

    fn enabled_policy() -> AutoMergePolicy {
        AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        }
    }

    fn make_diff(files: usize, added: usize, removed: usize, modules: &[&str]) -> DiffSummary {
        DiffSummary {
            files_changed: files,
            lines_added: added,
            lines_removed: removed,
            generated_lines_added: 0,
            generated_lines_removed: 0,
            modules_touched: modules.iter().map(|s| s.to_string()).collect(),
            sensitive_files: vec![],
            generated_report_artifacts: vec![],
            has_unsafe: false,
            has_conflicts: false,
            rename_count: 0,
            has_migrations: false,
            has_config_changes: false,
        }
    }

    #[test]
    fn auto_merge_small_clean_diff_with_passing_tests() {
        let summary = make_diff(2, 30, 10, &["team"]);
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, true);
        assert!(matches!(decision, AutoMergeDecision::AutoMerge { .. }));
    }

    #[test]
    fn auto_merge_rejected_when_tests_fail() {
        let summary = make_diff(2, 30, 10, &["team"]);
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, false);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("tests")));
            }
            _ => panic!("should reject when tests fail"),
        }
    }

    #[test]
    fn auto_merge_multiple_risk_factors_compound() {
        let mut summary = make_diff(8, 300, 200, &["team", "cli", "tmux"]);
        summary.has_migrations = true;
        summary.has_config_changes = true;
        summary.has_unsafe = true;
        summary.sensitive_files = vec!["Cargo.toml".to_string()];

        let policy = enabled_policy();
        let confidence = compute_merge_confidence(&summary, &policy);
        assert_eq!(confidence, 0.0, "extreme risk should floor at 0.0");

        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(
                    reasons.len() >= 3,
                    "should have many reasons: {:?}",
                    reasons
                );
                assert!(reasons.iter().any(|r| r.contains("migration")));
                assert!(reasons.iter().any(|r| r.contains("unsafe")));
                // modules gate relaxed (max=10), so 3 modules won't trigger
                // config changes are a soft signal (confidence penalty only)
                // but sensitive paths, confidence, file/line limits still trigger
            }
            _ => panic!("should be ManualReview"),
        }
    }

    #[test]
    fn auto_merge_rename_heavy_diff_gets_confidence_boost() {
        let mut rename_heavy = make_diff(5, 5, 5, &["team"]);
        rename_heavy.rename_count = 4;

        let plain = make_diff(5, 5, 5, &["team"]);

        let policy = enabled_policy();
        let rename_confidence = compute_merge_confidence(&rename_heavy, &policy);
        let plain_confidence = compute_merge_confidence(&plain, &policy);

        assert!(
            rename_confidence > plain_confidence,
            "renames boost confidence: {} vs {}",
            rename_confidence,
            plain_confidence
        );
    }

    #[test]
    fn auto_merge_disabled_policy_always_manual() {
        let summary = make_diff(1, 5, 2, &["team"]);
        let policy = AutoMergePolicy {
            enabled: false,
            ..AutoMergePolicy::default()
        };
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("disabled")));
            }
            _ => panic!("disabled policy should always be ManualReview"),
        }
    }

    #[test]
    fn auto_merge_conflicts_always_block() {
        let mut summary = make_diff(2, 20, 10, &["team"]);
        summary.has_conflicts = true;
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("conflicts")));
            }
            _ => panic!("conflicts should block auto-merge"),
        }
    }

    #[test]
    fn auto_merge_confidence_integrates_with_policy_threshold() {
        let policy = enabled_policy();

        // Clean small diff → high confidence → auto-merge
        let clean = make_diff(2, 20, 10, &["team"]);
        let confidence = compute_merge_confidence(&clean, &policy);
        assert!(
            confidence >= policy.confidence_threshold,
            "clean diff confidence {} should meet threshold {}",
            confidence,
            policy.confidence_threshold
        );
        assert!(matches!(
            should_auto_merge(&clean, &policy, true),
            AutoMergeDecision::AutoMerge { .. }
        ));

        // Unsafe + sensitive paths → manual review regardless of threshold
        let mut risky = make_diff(6, 150, 100, &["team", "cli", "tmux"]);
        risky.has_unsafe = true;
        risky.sensitive_files = vec!["Cargo.toml".to_string()];
        assert!(matches!(
            should_auto_merge(&risky, &policy, true),
            AutoMergeDecision::ManualReview { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // 3. Dependency resolution: unblocks tasks when deps complete
    // -----------------------------------------------------------------------

    fn solo_members() -> Vec<crate::team::hierarchy::MemberInstance> {
        vec![
            manager_member("mgr", None),
            engineer_member("eng-1", Some("mgr"), false),
        ]
    }

    #[test]
    fn dependency_unblocks_when_dep_completes() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task_file(tmp.path(), 1, "foundation", "done", None, &[], None);
        write_board_task_file(tmp.path(), 2, "build-on-it", "todo", None, &[1], None);

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();

        let task2 = resolutions.iter().find(|r| r.task_id == 2).unwrap();
        assert_eq!(task2.status, ResolutionStatus::Runnable);
    }

    #[test]
    fn dependency_stays_blocked_when_dep_incomplete() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task_file(
            tmp.path(),
            1,
            "still-working",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );
        write_board_task_file(tmp.path(), 2, "waiting", "todo", None, &[1], None);

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();

        let task2 = resolutions.iter().find(|r| r.task_id == 2).unwrap();
        assert_eq!(task2.status, ResolutionStatus::Blocked);
        assert!(task2.blocking_reason.as_ref().unwrap().contains("#1"));
    }

    #[test]
    fn diamond_dependency_unblocks_only_when_all_deps_met() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task_file(tmp.path(), 1, "root", "done", None, &[], None);
        write_board_task_file(tmp.path(), 2, "left", "done", None, &[1], None);
        write_board_task_file(
            tmp.path(),
            3,
            "right",
            "in-progress",
            Some("eng-1"),
            &[1],
            None,
        );
        write_board_task_file(tmp.path(), 4, "join", "todo", None, &[2, 3], None);

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();

        let task4 = resolutions.iter().find(|r| r.task_id == 4).unwrap();
        assert_eq!(task4.status, ResolutionStatus::Blocked);
    }

    #[test]
    fn runnable_tasks_filter_works_with_mixed_statuses() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task_file(tmp.path(), 1, "ready", "todo", None, &[], None);
        write_board_task_file(
            tmp.path(),
            2,
            "working",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );
        write_board_task_file(tmp.path(), 3, "blocked", "todo", None, &[99], None);
        write_board_task_file(
            tmp.path(),
            4,
            "in-review",
            "review",
            Some("eng-1"),
            &[],
            None,
        );

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();
        let runnable = runnable_tasks(&resolutions);

        assert!(
            runnable.iter().any(|r| r.task_id == 1),
            "task 1 should be runnable"
        );
        assert!(
            runnable.iter().any(|r| r.task_id == 2),
            "task 2 (in-progress) should be runnable"
        );
        assert!(
            !runnable.iter().any(|r| r.task_id == 3),
            "task 3 should be blocked (dep #99 missing)"
        );
    }

    #[test]
    fn workflow_meta_is_runnable_integrates_with_dep_set() {
        let done_set = HashSet::from([1u32, 2]);

        let runnable = WorkflowMeta {
            state: TaskState::Todo,
            depends_on: vec![1, 2],
            ..WorkflowMeta::default()
        };
        assert!(runnable.is_runnable(&done_set));

        let blocked = WorkflowMeta {
            state: TaskState::Todo,
            depends_on: vec![1, 3],
            ..WorkflowMeta::default()
        };
        assert!(!blocked.is_runnable(&done_set));

        let wrong_state = WorkflowMeta {
            state: TaskState::InProgress,
            depends_on: vec![1],
            ..WorkflowMeta::default()
        };
        assert!(!wrong_state.is_runnable(&done_set));
    }

    #[test]
    fn blocked_on_field_blocks_resolution() {
        let tmp = tempfile::tempdir().unwrap();

        write_board_task_file(
            tmp.path(),
            1,
            "externally-blocked",
            "todo",
            None,
            &[],
            Some("waiting-for-api"),
        );

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();

        let task1 = resolutions.iter().find(|r| r.task_id == 1).unwrap();
        assert_eq!(task1.status, ResolutionStatus::Blocked);
        assert!(
            task1
                .blocking_reason
                .as_ref()
                .unwrap()
                .contains("waiting-for-api")
        );
    }

    // -----------------------------------------------------------------------
    // 4. Dispatch queue: respects WIP limits and stabilization delay together
    // -----------------------------------------------------------------------

    #[test]
    fn wip_limit_blocks_dispatch_when_at_capacity() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(1),
            ..WorkflowPolicy::default()
        };

        assert!(check_wip_limit(&policy, RoleType::Engineer, 0));
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 1));
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 2));
    }

    #[test]
    fn wip_limit_none_means_unlimited() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: None,
            ..WorkflowPolicy::default()
        };

        assert!(check_wip_limit(&policy, RoleType::Engineer, 0));
        assert!(check_wip_limit(&policy, RoleType::Engineer, 100));
    }

    #[test]
    fn wip_limits_differ_by_role_type() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(2),
            wip_limit_per_reviewer: Some(5),
            ..WorkflowPolicy::default()
        };

        // Engineers hit limit at 2
        assert!(check_wip_limit(&policy, RoleType::Engineer, 1));
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 2));

        // Managers (reviewers) hit limit at 5
        assert!(check_wip_limit(&policy, RoleType::Manager, 4));
        assert!(!check_wip_limit(&policy, RoleType::Manager, 5));

        // Architects (also reviewers) hit limit at 5
        assert!(check_wip_limit(&policy, RoleType::Architect, 4));
        assert!(!check_wip_limit(&policy, RoleType::Architect, 5));

        // User type has no WIP limit
        assert!(check_wip_limit(&policy, RoleType::User, 999));
    }

    #[test]
    fn wip_and_dependency_interact_correctly() {
        // When WIP is at capacity, even runnable tasks shouldn't be dispatched.
        // When deps are unmet, even under WIP, tasks shouldn't be dispatched.
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(1),
            ..WorkflowPolicy::default()
        };

        // Under WIP, deps met → runnable
        let meta = WorkflowMeta {
            state: TaskState::Todo,
            depends_on: vec![1],
            ..WorkflowMeta::default()
        };
        let done = HashSet::from([1u32]);
        assert!(meta.is_runnable(&done));
        assert!(check_wip_limit(&policy, RoleType::Engineer, 0));

        // Under WIP, deps unmet → blocked (deps win)
        let unmet_done = HashSet::new();
        assert!(!meta.is_runnable(&unmet_done));

        // Over WIP, deps met → WIP blocks dispatch
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 1));
    }

    // -----------------------------------------------------------------------
    // Cross-cutting: policy + escalation thresholds working together
    // -----------------------------------------------------------------------

    #[test]
    fn policy_escalation_and_review_thresholds_interact_correctly() {
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            escalation_threshold_secs: 3600,
            ..WorkflowPolicy::default()
        };

        // At 1799s: nothing fires
        assert!(!is_review_nudge_due(&policy, 1799));
        assert!(!is_review_stale(&policy, 1799));
        assert!(!should_escalate(&policy, 1799));

        // At 1800s: nudge fires only
        assert!(is_review_nudge_due(&policy, 1800));
        assert!(!is_review_stale(&policy, 1800));
        assert!(!should_escalate(&policy, 1800));

        // At 3600s: nudge + escalation, not stale
        assert!(is_review_nudge_due(&policy, 3600));
        assert!(!is_review_stale(&policy, 3600));
        assert!(should_escalate(&policy, 3600));

        // At 7200s: all three fire
        assert!(is_review_nudge_due(&policy, 7200));
        assert!(is_review_stale(&policy, 7200));
        assert!(should_escalate(&policy, 7200));
    }

    #[test]
    fn dispatch_task_selection_respects_dependency_chain() {
        // Verifies the cross-module interaction: resolver sees deps as met
        // only when the board marks them done, which is the same logic
        // the dispatch queue uses.
        let tmp = tempfile::tempdir().unwrap();

        // Chain: 1 (done) → 2 (done) → 3 (todo, deps=[2]) → 4 (todo, deps=[3])
        write_board_task_file(tmp.path(), 1, "step1", "done", None, &[], None);
        write_board_task_file(tmp.path(), 2, "step2", "done", None, &[1], None);
        write_board_task_file(tmp.path(), 3, "step3", "todo", None, &[2], None);
        write_board_task_file(tmp.path(), 4, "step4", "todo", None, &[3], None);

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();
        let runnable = runnable_tasks(&resolutions);

        // Task 3 is runnable (dep 2 done), task 4 is blocked (dep 3 not done)
        assert!(
            runnable.iter().any(|r| r.task_id == 3),
            "task 3 should be runnable (dep 2 done)"
        );
        assert!(
            !runnable.iter().any(|r| r.task_id == 4),
            "task 4 should be blocked (dep 3 still todo)"
        );
    }

    #[test]
    fn review_state_and_resolution_status_align() {
        // Tasks in review status should get NeedsReview from resolver
        let tmp = tempfile::tempdir().unwrap();

        write_board_task_file(
            tmp.path(),
            1,
            "under-review",
            "review",
            Some("eng-1"),
            &[],
            None,
        );

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let resolutions = resolve_board(&board_dir, &solo_members()).unwrap();

        let task1 = resolutions.iter().find(|r| r.task_id == 1).unwrap();
        assert_eq!(task1.status, ResolutionStatus::NeedsReview);
    }
}
