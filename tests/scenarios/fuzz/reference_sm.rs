//! `ReferenceStateMachine` impl + pure `apply` function for the
//! scenario framework fuzzer.
//!
//! The reference state machine is the *oracle* the fuzzer compares
//! against: when a generated transition is applied to the real
//! fixture via the SUT (#644), the model is also stepped forward
//! via [`apply`], and invariants (#645) are asserted to hold between
//! model and SUT. Because `apply` is pure, `proptest-state-machine`
//! can shrink failing sequences deterministically.

use proptest::prelude::*;
use proptest::strategy::{BoxedStrategy, Just};
use proptest_state_machine::ReferenceStateMachine;

use super::model::{ModelBoard, ModelEngineerState, ModelTaskStatus, Transition};

/// Pure state transition function. Takes the previous state and a
/// transition, returns the next state. No I/O, no time, no
/// randomness — this is the oracle every fuzz invariant compares
/// against.
pub fn apply(mut state: ModelBoard, transition: &Transition) -> ModelBoard {
    match transition {
        Transition::DispatchTask { task_id, engineer } => {
            if let (Some(task), Some(eng)) = (
                state.tasks.get_mut(task_id),
                state.engineers.get_mut(engineer),
            ) {
                if task.status == ModelTaskStatus::Todo && eng.state == ModelEngineerState::Idle {
                    task.status = ModelTaskStatus::InProgress;
                    task.claimed_by = Some(engineer.clone());
                    eng.state = ModelEngineerState::Working;
                    eng.active_task = Some(*task_id);
                    eng.worktree_branch = Some(format!("{engineer}/{task_id}"));
                }
            }
        }
        Transition::EngineerCommits { engineer, lines } => {
            if let Some(eng) = state.engineer_mut(engineer) {
                if eng.state == ModelEngineerState::Working {
                    eng.dirty_lines = eng.dirty_lines.saturating_add(*lines);
                    if let Some(task_id) = eng.active_task {
                        if let Some(task) = state.tasks.get_mut(&task_id) {
                            task.branch_commits = task.branch_commits.saturating_add(1);
                        }
                    }
                }
            }
        }
        Transition::ReportCompletion { engineer } => {
            if let Some(eng) = state.engineers.get_mut(engineer) {
                if eng.state == ModelEngineerState::Working {
                    // Completion drops the dirty-line counter (work
                    // now lives as a committed branch) and moves the
                    // engineer back to Idle, but keeps the task in
                    // InProgress until verification runs.
                    eng.dirty_lines = 0;
                    eng.state = ModelEngineerState::Idle;
                }
            }
        }
        Transition::RunVerification { task_id } => {
            if let Some(task) = state.task_mut(*task_id) {
                if task.status == ModelTaskStatus::InProgress && task.branch_commits > 0 {
                    task.status = ModelTaskStatus::Review;
                }
            }
        }
        Transition::SubmitForMerge { task_id } => {
            if let Some(task) = state.task_mut(*task_id) {
                if task.status == ModelTaskStatus::Review {
                    task.merge_attempts = task.merge_attempts.saturating_add(1);
                }
            }
        }
        Transition::MergeQueueTick => {
            // Pick the first review task and land it, advancing the
            // main tip and freeing its engineer.
            let next = state
                .tasks
                .iter()
                .find(|(_, t)| t.status == ModelTaskStatus::Review)
                .map(|(id, _)| *id);
            if let Some(task_id) = next {
                let engineer = state.tasks.get(&task_id).and_then(|t| t.claimed_by.clone());
                if let Some(task) = state.tasks.get_mut(&task_id) {
                    task.status = ModelTaskStatus::Done;
                }
                state.main_tip = state.main_tip.saturating_add(1);
                if let Some(engineer) = engineer {
                    if let Some(eng) = state.engineers.get_mut(&engineer) {
                        eng.active_task = None;
                        eng.worktree_branch = None;
                        eng.state = ModelEngineerState::Idle;
                    }
                }
            }
        }
        Transition::ReclaimExpiredClaim { task_id } => {
            if let Some(task) = state.tasks.get_mut(task_id) {
                if task.status == ModelTaskStatus::InProgress {
                    let prev_engineer = task.claimed_by.take();
                    task.status = ModelTaskStatus::Todo;
                    task.branch_commits = 0;
                    if let Some(engineer) = prev_engineer {
                        if let Some(eng) = state.engineers.get_mut(&engineer) {
                            eng.active_task = None;
                            eng.state = ModelEngineerState::Idle;
                        }
                    }
                }
            }
        }
        Transition::DaemonRestart => {
            // Daemon restart clears in-flight claim state but keeps
            // board tasks. Engineers go back to Idle; any
            // InProgress task with no matching engineer snaps back
            // to Todo.
            for eng in state.engineers.values_mut() {
                eng.state = ModelEngineerState::Idle;
                eng.active_task = None;
                eng.worktree_branch = None;
                eng.dirty_lines = 0;
            }
            state.merge_lock_held_by = None;
        }
        Transition::ShimGoSilent { engineer } | Transition::ContextExhaust { engineer } => {
            if let Some(eng) = state.engineer_mut(engineer) {
                eng.state = ModelEngineerState::Dead;
            }
        }
        Transition::ShimEmitError { engineer, .. } => {
            if let Some(eng) = state.engineer_mut(engineer) {
                if eng.state == ModelEngineerState::Working {
                    eng.state = ModelEngineerState::Idle;
                }
            }
        }
        Transition::DirtyWorktree { engineer, lines } => {
            if let Some(eng) = state.engineer_mut(engineer) {
                eng.dirty_lines = eng.dirty_lines.saturating_add(*lines);
            }
        }
        Transition::CorruptWorktree { .. }
        | Transition::BranchDrift { .. }
        | Transition::BadFrontmatter { .. }
        | Transition::DiskPressure { .. }
        | Transition::ScopeFenceViolation { .. }
        | Transition::NarrationOnlyCompletion { .. }
        | Transition::FireStandup
        | Transition::FireNudge { .. }
        | Transition::AdvanceTime { .. } => {
            // These are observable only in the real SUT (filesystem,
            // inbox, time-warp). The model treats them as no-ops so
            // the fuzzer still generates them; SUT-side invariants
            // (ticket #645) assert the real daemon handled them.
        }
        Transition::StaleMergeLock => {
            // Model: a stale lock blocks new merges until cleared by
            // a later MergeQueueTick or DaemonRestart.
            if state.merge_lock_held_by.is_none() {
                state.merge_lock_held_by = Some("stale".to_string());
            }
        }
    }
    state
}

/// The `ReferenceStateMachine` impl. Generates initial states with
/// 1-3 engineers and 5-15 todo tasks; transitions are generated
/// state-independently (the fuzzer will filter via `preconditions`).
pub struct FuzzModel;

impl ReferenceStateMachine for FuzzModel {
    type State = ModelBoard;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        (1usize..=3, 5u32..=15)
            .prop_map(|(engineer_count, task_count)| {
                let mut board = ModelBoard::new();
                for i in 1..=engineer_count {
                    board = board.with_engineer(&format!("eng-{i}"));
                }
                for id in 1..=task_count {
                    board = board.with_task(id);
                }
                board
            })
            .boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let engineers: Vec<String> = state.engineers.keys().cloned().collect();
        let tasks: Vec<u32> = state.tasks.keys().copied().collect();
        if engineers.is_empty() || tasks.is_empty() {
            return Just(Transition::FireStandup).boxed();
        }
        let engineer_strat = proptest::sample::select(engineers);
        let task_strat = proptest::sample::select(tasks);

        prop_oneof![
            // Workflow-heavy weights (fuzzer prefers forward progress).
            5 => (task_strat.clone(), engineer_strat.clone())
                .prop_map(|(task_id, engineer)| Transition::DispatchTask { task_id, engineer }),
            4 => (engineer_strat.clone(), 1u32..200)
                .prop_map(|(engineer, lines)| Transition::EngineerCommits { engineer, lines }),
            4 => engineer_strat
                .clone()
                .prop_map(|engineer| Transition::ReportCompletion { engineer }),
            3 => task_strat
                .clone()
                .prop_map(|task_id| Transition::RunVerification { task_id }),
            3 => task_strat
                .clone()
                .prop_map(|task_id| Transition::SubmitForMerge { task_id }),
            3 => Just(Transition::MergeQueueTick),
            1 => task_strat
                .clone()
                .prop_map(|task_id| Transition::ReclaimExpiredClaim { task_id }),
            1 => Just(Transition::FireStandup),
            1 => engineer_strat
                .clone()
                .prop_map(|engineer| Transition::FireNudge { engineer }),
            1 => Just(Transition::DaemonRestart),

            // Fault alphabet (weighted lower — ~20% of transitions).
            1 => engineer_strat
                .clone()
                .prop_map(|engineer| Transition::ShimGoSilent { engineer }),
            1 => engineer_strat
                .clone()
                .prop_map(|engineer| Transition::ContextExhaust { engineer }),
            1 => (engineer_strat.clone(), 1u32..50)
                .prop_map(|(engineer, lines)| Transition::DirtyWorktree { engineer, lines }),
            1 => engineer_strat
                .clone()
                .prop_map(|engineer| Transition::NarrationOnlyCompletion { engineer }),
            1 => Just(Transition::StaleMergeLock),
            1 => (1u64..300).prop_map(|seconds| Transition::AdvanceTime { seconds }),
        ]
        .boxed()
    }

    fn apply(state: Self::State, transition: &Self::Transition) -> Self::State {
        apply(state, transition)
    }

    fn preconditions(state: &Self::State, transition: &Self::Transition) -> bool {
        match transition {
            Transition::DispatchTask { task_id, engineer } => {
                state
                    .tasks
                    .get(task_id)
                    .is_some_and(|t| t.status == ModelTaskStatus::Todo)
                    && state
                        .engineers
                        .get(engineer)
                        .is_some_and(|e| e.state == ModelEngineerState::Idle)
            }
            Transition::EngineerCommits { engineer, .. }
            | Transition::ReportCompletion { engineer } => state
                .engineers
                .get(engineer)
                .is_some_and(|e| e.state == ModelEngineerState::Working),
            Transition::RunVerification { task_id } => state
                .tasks
                .get(task_id)
                .is_some_and(|t| t.status == ModelTaskStatus::InProgress),
            Transition::SubmitForMerge { task_id } => state
                .tasks
                .get(task_id)
                .is_some_and(|t| t.status == ModelTaskStatus::Review),
            Transition::ReclaimExpiredClaim { task_id } => state
                .tasks
                .get(task_id)
                .is_some_and(|t| t.status == ModelTaskStatus::InProgress),
            // No preconditions for fault / no-op transitions.
            _ => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the pure apply function
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::model::{
        BadFrontmatterShape, CorruptionKind, ModelBoard, ModelEngineerState, ModelTaskStatus,
        Transition,
    };
    use super::apply;

    fn seed() -> ModelBoard {
        ModelBoard::new()
            .with_engineer("eng-1")
            .with_engineer("eng-2")
            .with_task(1)
            .with_task(2)
            .with_task(3)
    }

    #[test]
    fn apply_dispatch_task_moves_idle_engineer_to_working() {
        let state = seed();
        let next = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        assert_eq!(next.engineers["eng-1"].state, ModelEngineerState::Working);
        assert_eq!(next.engineers["eng-1"].active_task, Some(1));
        assert_eq!(next.tasks[&1].status, ModelTaskStatus::InProgress);
        assert_eq!(next.tasks[&1].claimed_by.as_deref(), Some("eng-1"));
    }

    #[test]
    fn apply_dispatch_skips_already_working_engineer() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        // Second dispatch for the same engineer must not clobber task 1.
        let after = apply(
            state.clone(),
            &Transition::DispatchTask {
                task_id: 2,
                engineer: "eng-1".into(),
            },
        );
        assert_eq!(after.tasks[&2].status, ModelTaskStatus::Todo);
        assert_eq!(after.engineers["eng-1"].active_task, Some(1));
    }

    #[test]
    fn apply_engineer_commits_grows_branch_commits() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        state = apply(
            state,
            &Transition::EngineerCommits {
                engineer: "eng-1".into(),
                lines: 10,
            },
        );
        assert_eq!(state.engineers["eng-1"].dirty_lines, 10);
        assert_eq!(state.tasks[&1].branch_commits, 1);
    }

    #[test]
    fn apply_report_completion_clears_dirty_lines() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        state = apply(
            state,
            &Transition::EngineerCommits {
                engineer: "eng-1".into(),
                lines: 42,
            },
        );
        state = apply(
            state,
            &Transition::ReportCompletion {
                engineer: "eng-1".into(),
            },
        );
        assert_eq!(state.engineers["eng-1"].dirty_lines, 0);
        assert_eq!(state.engineers["eng-1"].state, ModelEngineerState::Idle);
        // Task is still InProgress — verification runs next.
        assert_eq!(state.tasks[&1].status, ModelTaskStatus::InProgress);
    }

    #[test]
    fn apply_run_verification_moves_to_review_only_if_commits_exist() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        // Verification with no commits is a no-op.
        let no_commits = apply(state.clone(), &Transition::RunVerification { task_id: 1 });
        assert_eq!(no_commits.tasks[&1].status, ModelTaskStatus::InProgress);
        // After a commit, verification advances to Review.
        state = apply(
            state,
            &Transition::EngineerCommits {
                engineer: "eng-1".into(),
                lines: 5,
            },
        );
        state = apply(state, &Transition::RunVerification { task_id: 1 });
        assert_eq!(state.tasks[&1].status, ModelTaskStatus::Review);
    }

    #[test]
    fn apply_merge_queue_tick_lands_review_and_advances_main_tip() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        state = apply(
            state,
            &Transition::EngineerCommits {
                engineer: "eng-1".into(),
                lines: 1,
            },
        );
        state = apply(state, &Transition::RunVerification { task_id: 1 });
        let before_tip = state.main_tip;
        state = apply(state, &Transition::MergeQueueTick);
        assert_eq!(state.tasks[&1].status, ModelTaskStatus::Done);
        assert_eq!(state.main_tip, before_tip + 1);
        // Engineer is free again.
        assert_eq!(state.engineers["eng-1"].state, ModelEngineerState::Idle);
        assert_eq!(state.engineers["eng-1"].active_task, None);
    }

    #[test]
    fn apply_reclaim_expired_claim_resets_task_and_engineer() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        state = apply(state, &Transition::ReclaimExpiredClaim { task_id: 1 });
        assert_eq!(state.tasks[&1].status, ModelTaskStatus::Todo);
        assert_eq!(state.tasks[&1].claimed_by, None);
        assert_eq!(state.engineers["eng-1"].state, ModelEngineerState::Idle);
        assert_eq!(state.engineers["eng-1"].active_task, None);
    }

    #[test]
    fn apply_daemon_restart_returns_all_engineers_to_idle() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 2,
                engineer: "eng-2".into(),
            },
        );
        state = apply(state, &Transition::DaemonRestart);
        for engineer in state.engineers.values() {
            assert_eq!(engineer.state, ModelEngineerState::Idle);
            assert_eq!(engineer.active_task, None);
        }
    }

    #[test]
    fn apply_shim_go_silent_marks_dead() {
        let mut state = seed();
        state = apply(
            state,
            &Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        state = apply(
            state,
            &Transition::ShimGoSilent {
                engineer: "eng-1".into(),
            },
        );
        assert_eq!(state.engineers["eng-1"].state, ModelEngineerState::Dead);
    }

    #[test]
    fn apply_stale_merge_lock_blocks_lock_until_cleared() {
        let mut state = seed();
        state = apply(state, &Transition::StaleMergeLock);
        assert_eq!(state.merge_lock_held_by.as_deref(), Some("stale"));
        // DaemonRestart clears it.
        state = apply(state, &Transition::DaemonRestart);
        assert_eq!(state.merge_lock_held_by, None);
    }

    #[test]
    fn apply_is_deterministic_for_same_state_and_transition() {
        let seed_state = seed();
        let transition = Transition::DispatchTask {
            task_id: 1,
            engineer: "eng-1".into(),
        };
        let a = apply(seed_state.clone(), &transition);
        let b = apply(seed_state, &transition);
        assert_eq!(a, b);
    }

    #[test]
    fn apply_noop_transitions_leave_state_unchanged() {
        let before = seed();
        let transitions = [
            Transition::CorruptWorktree {
                engineer: "eng-1".into(),
                kind: CorruptionKind::MissingDir,
            },
            Transition::BadFrontmatter {
                task_id: 1,
                shape: BadFrontmatterShape::LegacyStringBlock,
            },
            Transition::DiskPressure { free_gb: 5 },
            Transition::FireStandup,
            Transition::AdvanceTime { seconds: 60 },
        ];
        for t in &transitions {
            assert_eq!(
                apply(before.clone(), t),
                before,
                "noop transition {t:?} should leave state unchanged"
            );
        }
    }
}
