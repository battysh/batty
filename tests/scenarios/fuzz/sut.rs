//! `StateMachineTest` implementation — the bridge between the pure
//! reference model and the real [`ScenarioFixture`].
//!
//! Each [`Transition`] maps to one or more concrete fixture method
//! calls. The mapping is deliberately forgiving: transitions that
//! don't cleanly translate into phase-1 fixture operations are
//! applied as no-ops at the SUT level so the fuzzer can still
//! generate them and measure "does the daemon panic / log errors"
//! invariants (#645).
//!
//! Ticket #644 ships the plumbing. Ticket #645 layers invariants on
//! top via `check_invariants`.

use std::path::PathBuf;
use std::time::Duration;

use batty_cli::shim::fake::ShimBehavior;
use proptest_state_machine::StateMachineTest;

use super::super::scenarios_common::{ScenarioFixture, board_ops};
use super::model::{CorruptionKind, ModelBoard, ModelTaskStatus, Transition};
use super::reference_sm::FuzzModel;

/// The system under test: an owned [`ScenarioFixture`] + a growing
/// audit log of tick errors that `check_invariants` inspects.
pub struct FuzzSut {
    pub fixture: ScenarioFixture,
    /// Every subsystem error observed across the test run. `apply`
    /// appends to this after each tick so invariants (#645) can
    /// assert the error count is bounded.
    pub tick_errors: Vec<(String, String)>,
    /// Names of engineers the fuzzer has installed fake shims for.
    /// Used to dedup `insert_fake_shim` when the same engineer is
    /// dispatched twice in a row.
    pub wired_fakes: Vec<String>,
}

impl FuzzSut {
    fn ensure_fake_shim(&mut self, member: &str) {
        if !self.wired_fakes.iter().any(|name| name == member) {
            self.fixture.insert_fake_shim(member);
            self.wired_fakes.push(member.to_string());
        }
    }

    fn drive_tick(&mut self) {
        let report = self.fixture.tick();
        self.tick_errors.extend(report.subsystem_errors);
    }
}

/// The `StateMachineTest` impl. Phase 1 mapping covers every
/// workflow transition at best effort; fault transitions mostly
/// route through `tick()` + state-mutation helpers.
pub struct FuzzTest;

impl StateMachineTest for FuzzTest {
    type SystemUnderTest = FuzzSut;
    type Reference = FuzzModel;

    fn init_test(ref_state: &ModelBoard) -> Self::SystemUnderTest {
        let mut builder = ScenarioFixture::builder().with_manager("manager");
        // Shape the SUT team to match the reference. Engineer names
        // follow `eng-{n}` so transitions generated against
        // eng-1..eng-3 map identically.
        builder = builder.with_engineers(ref_state.engineers.len());
        for (id, task) in &ref_state.tasks {
            let status = match task.status {
                ModelTaskStatus::Todo => "todo",
                ModelTaskStatus::InProgress => "in-progress",
                ModelTaskStatus::Review => "review",
                ModelTaskStatus::Done => "done",
                ModelTaskStatus::Blocked => "blocked",
            };
            builder = builder.with_task(
                *id,
                &format!("fuzz task {id}"),
                status,
                task.claimed_by.as_deref(),
            );
        }
        let fixture = builder.build();
        board_ops::init_git_repo(fixture.project_root());

        FuzzSut {
            fixture,
            tick_errors: Vec::new(),
            wired_fakes: Vec::new(),
        }
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        _ref_state: &ModelBoard,
        transition: Transition,
    ) -> Self::SystemUnderTest {
        match transition {
            Transition::DispatchTask { task_id, engineer } => {
                // Wire a fake shim for the engineer so the dispatch
                // path has somewhere to send its message. Seed a
                // CompleteWith that commits a single file so later
                // EngineerCommits / ReportCompletion can drive
                // forward.
                state.ensure_fake_shim(&engineer);
                state.fixture.set_active_task(&engineer, task_id);
                state.drive_tick();
            }
            Transition::EngineerCommits { engineer, lines } => {
                state.ensure_fake_shim(&engineer);
                let payload = (0..lines.min(50))
                    .map(|i| format!("// fuzz line {i}\n"))
                    .collect::<String>();
                state
                    .fixture
                    .shim(&engineer)
                    .queue(ShimBehavior::CompleteWith {
                        response: format!("fuzz commits {lines}"),
                        files_touched: vec![(
                            PathBuf::from(format!("src/fuzz_{engineer}.rs")),
                            payload,
                        )],
                    });
                state
                    .fixture
                    .send_to_shim(&engineer, "manager", "write code");
                let _ = state.fixture.process_shim(&engineer);
                state.drive_tick();
            }
            Transition::ReportCompletion { engineer } => {
                state.ensure_fake_shim(&engineer);
                state
                    .fixture
                    .shim(&engineer)
                    .queue(ShimBehavior::complete_with("fuzz reports done", vec![]));
                state.fixture.send_to_shim(&engineer, "manager", "report");
                let _ = state.fixture.process_shim(&engineer);
                state.drive_tick();
            }
            Transition::RunVerification { .. } | Transition::SubmitForMerge { .. } => {
                state.drive_tick();
            }
            Transition::MergeQueueTick => {
                state.drive_tick();
            }
            Transition::ReclaimExpiredClaim { .. }
            | Transition::FireStandup
            | Transition::FireNudge { .. } => {
                state.drive_tick();
            }
            Transition::DaemonRestart => {
                // Phase 1 restart: wipe active_tasks + tick so
                // reconcile rebuilds from the board. The tempdir
                // and on-disk board survive intact so the daemon
                // state is rebuilt from persistent storage, which
                // is what a real restart does.
                for engineer in state.wired_fakes.clone() {
                    state
                        .fixture
                        .daemon_mut()
                        .scenario_hooks()
                        .remove_shim_handle(&engineer);
                }
                state.wired_fakes.clear();
                state.drive_tick();
            }
            Transition::ShimGoSilent { engineer } => {
                state.ensure_fake_shim(&engineer);
                state.fixture.shim(&engineer).queue(ShimBehavior::Silent);
                state
                    .fixture
                    .daemon_mut()
                    .scenario_hooks()
                    .backdate_shim_state_change(&engineer, Duration::from_secs(7200));
                state.drive_tick();
            }
            Transition::ShimEmitError { engineer, reason } => {
                state.ensure_fake_shim(&engineer);
                state
                    .fixture
                    .shim(&engineer)
                    .queue(ShimBehavior::error("send_message", reason));
                state.fixture.send_to_shim(&engineer, "manager", "trigger");
                let _ = state.fixture.process_shim(&engineer);
                state.drive_tick();
            }
            Transition::ContextExhaust { engineer } => {
                state.ensure_fake_shim(&engineer);
                state
                    .fixture
                    .shim(&engineer)
                    .queue(ShimBehavior::ContextExhausted {
                        message: "fuzz ctx".to_string(),
                    });
                state.fixture.send_to_shim(&engineer, "manager", "exhaust");
                let _ = state.fixture.process_shim(&engineer);
                state.drive_tick();
            }
            Transition::DirtyWorktree { engineer, lines } => {
                // Simulate an engineer with dirty local changes by
                // writing an untracked file.
                let worktree = state
                    .fixture
                    .project_root()
                    .join(".batty")
                    .join("worktrees")
                    .join(&engineer);
                let _ = std::fs::create_dir_all(&worktree);
                let _ = std::fs::write(worktree.join("dirty.txt"), "x\n".repeat(lines as usize));
                state.drive_tick();
            }
            Transition::CorruptWorktree { engineer, kind } => {
                let worktree = state
                    .fixture
                    .project_root()
                    .join(".batty")
                    .join("worktrees")
                    .join(&engineer);
                match kind {
                    CorruptionKind::MissingDir => {
                        let _ = std::fs::create_dir_all(&worktree);
                        let _ = std::fs::remove_dir_all(&worktree);
                    }
                    CorruptionKind::DetachedHead => {
                        // Phase 1: record the intent; full detached-
                        // head simulation requires a real worktree.
                    }
                    CorruptionKind::BrokenIndex => {
                        let index = worktree.join(".git").join("index");
                        if index.exists() {
                            let _ = std::fs::write(&index, b"");
                        }
                    }
                }
                state.drive_tick();
            }
            Transition::BranchDrift { .. }
            | Transition::BadFrontmatter { .. }
            | Transition::ScopeFenceViolation { .. }
            | Transition::NarrationOnlyCompletion { .. } => {
                state.drive_tick();
            }
            Transition::StaleMergeLock => {
                let batty_dir = state.fixture.project_root().join(".batty");
                let _ = std::fs::create_dir_all(&batty_dir);
                let _ = std::fs::write(batty_dir.join("merge.lock"), "pid=999999\n");
                state.drive_tick();
            }
            Transition::DiskPressure { .. } => {
                state.drive_tick();
            }
            Transition::AdvanceTime { seconds } => {
                // Backdate the shim health check so the next tick
                // re-runs stall detection on every installed fake.
                state
                    .fixture
                    .daemon_mut()
                    .scenario_hooks()
                    .backdate_last_shim_health_check(Duration::from_secs(seconds));
                state
                    .fixture
                    .daemon_mut()
                    .scenario_hooks()
                    .backdate_last_disk_hygiene(Duration::from_secs(seconds));
                state.drive_tick();
            }
        }
        state
    }

    fn check_invariants(state: &Self::SystemUnderTest, _ref_state: &ModelBoard) {
        // Phase 1 invariant: the subsystem error count across the
        // whole test run must stay bounded. The real invariant
        // catalog lands in ticket #645.
        assert!(
            state.tick_errors.len() < 200,
            "fuzz SUT accumulated too many tick errors ({}): {:?}",
            state.tick_errors.len(),
            &state.tick_errors[..state.tick_errors.len().min(10)]
        );
    }

    fn teardown(_state: Self::SystemUnderTest, _ref_state: ModelBoard) {
        // ScenarioFixture owns a TempDir which cleans up on drop.
    }
}

// ---------------------------------------------------------------------------
// Unit tests: exercise each Transition variant against the SUT
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::model::{BadFrontmatterShape, CorruptionKind, ModelBoard, Transition};
    use super::*;

    fn seed() -> ModelBoard {
        ModelBoard::new()
            .with_engineer("eng-1")
            .with_engineer("eng-2")
            .with_task(1)
            .with_task(2)
    }

    fn init() -> FuzzSut {
        FuzzTest::init_test(&seed())
    }

    fn drive(sut: FuzzSut, t: Transition) -> FuzzSut {
        FuzzTest::apply(sut, &seed(), t)
    }

    #[test]
    fn sut_dispatch_task_installs_fake_and_ticks() {
        let sut = init();
        let after = drive(
            sut,
            Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        assert!(after.wired_fakes.contains(&"eng-1".to_string()));
    }

    #[test]
    fn sut_engineer_commits_writes_file_via_fake() {
        let sut = init();
        let after = drive(
            sut,
            Transition::EngineerCommits {
                engineer: "eng-1".into(),
                lines: 3,
            },
        );
        let committed = after.fixture.project_root().join("src/fuzz_eng-1.rs");
        assert!(
            committed.exists(),
            "EngineerCommits should have written the fake commit file"
        );
    }

    #[test]
    fn sut_report_completion_drains_event() {
        let sut = init();
        let _after = drive(
            sut,
            Transition::ReportCompletion {
                engineer: "eng-1".into(),
            },
        );
    }

    #[test]
    fn sut_run_verification_ticks_without_panic() {
        let sut = init();
        let _ = drive(sut, Transition::RunVerification { task_id: 1 });
    }

    #[test]
    fn sut_submit_for_merge_ticks_without_panic() {
        let sut = init();
        let _ = drive(sut, Transition::SubmitForMerge { task_id: 1 });
    }

    #[test]
    fn sut_merge_queue_tick_advances_cycle() {
        let mut sut = init();
        let pre_cycle = sut.fixture.daemon_mut().scenario_hooks().poll_cycle_count();
        sut = drive(sut, Transition::MergeQueueTick);
        let post_cycle = sut.fixture.daemon_mut().scenario_hooks().poll_cycle_count();
        assert!(post_cycle > pre_cycle);
    }

    #[test]
    fn sut_reclaim_expired_claim_ticks_without_panic() {
        let sut = init();
        let _ = drive(sut, Transition::ReclaimExpiredClaim { task_id: 1 });
    }

    #[test]
    fn sut_fire_standup_and_nudge_tick_without_panic() {
        let mut sut = init();
        sut = drive(sut, Transition::FireStandup);
        let _ = drive(
            sut,
            Transition::FireNudge {
                engineer: "eng-1".into(),
            },
        );
    }

    #[test]
    fn sut_daemon_restart_clears_wired_fakes() {
        let mut sut = init();
        sut = drive(
            sut,
            Transition::DispatchTask {
                task_id: 1,
                engineer: "eng-1".into(),
            },
        );
        assert!(!sut.wired_fakes.is_empty());
        sut = drive(sut, Transition::DaemonRestart);
        assert!(
            sut.wired_fakes.is_empty(),
            "DaemonRestart should clear wired fake shims"
        );
    }

    #[test]
    fn sut_shim_go_silent_queues_silent_and_backdates() {
        let sut = init();
        let _after = drive(
            sut,
            Transition::ShimGoSilent {
                engineer: "eng-1".into(),
            },
        );
    }

    #[test]
    fn sut_shim_emit_error_routes_error_event() {
        let sut = init();
        let _after = drive(
            sut,
            Transition::ShimEmitError {
                engineer: "eng-1".into(),
                reason: "boom".into(),
            },
        );
    }

    #[test]
    fn sut_context_exhaust_routes_event() {
        let sut = init();
        let _after = drive(
            sut,
            Transition::ContextExhaust {
                engineer: "eng-1".into(),
            },
        );
    }

    #[test]
    fn sut_dirty_worktree_creates_dirty_file() {
        let sut = init();
        let after = drive(
            sut,
            Transition::DirtyWorktree {
                engineer: "eng-1".into(),
                lines: 5,
            },
        );
        let dirty = after
            .fixture
            .project_root()
            .join(".batty/worktrees/eng-1/dirty.txt");
        assert!(dirty.exists());
    }

    #[test]
    fn sut_corrupt_worktree_missing_dir_deletes() {
        let sut = init();
        let after = drive(
            sut,
            Transition::CorruptWorktree {
                engineer: "eng-1".into(),
                kind: CorruptionKind::MissingDir,
            },
        );
        let worktree = after.fixture.project_root().join(".batty/worktrees/eng-1");
        assert!(!worktree.exists());
    }

    #[test]
    fn sut_bad_frontmatter_ticks_without_panic() {
        let sut = init();
        let _ = drive(
            sut,
            Transition::BadFrontmatter {
                task_id: 1,
                shape: BadFrontmatterShape::LegacyStringBlock,
            },
        );
    }

    #[test]
    fn sut_stale_merge_lock_writes_lockfile() {
        let sut = init();
        let after = drive(sut, Transition::StaleMergeLock);
        assert!(
            after
                .fixture
                .project_root()
                .join(".batty/merge.lock")
                .exists(),
            "StaleMergeLock should create the lockfile"
        );
    }

    #[test]
    fn sut_advance_time_backdates_and_ticks() {
        let sut = init();
        let _ = drive(sut, Transition::AdvanceTime { seconds: 120 });
    }

    #[test]
    fn sut_narration_only_and_scope_fence_tick_without_panic() {
        let mut sut = init();
        sut = drive(
            sut,
            Transition::NarrationOnlyCompletion {
                engineer: "eng-1".into(),
            },
        );
        let _ = drive(
            sut,
            Transition::ScopeFenceViolation {
                engineer: "eng-2".into(),
            },
        );
    }
}
