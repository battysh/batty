//! `ScenarioFixture` — the top-level scenario harness.
//!
//! Composes `TempDir + git repo + kanban board + TeamDaemon + N FakeShim`s
//! into a single struct that drives the daemon through tick sequences.
//! Phase 1 ships a minimal API: team composition, `tick*`, board
//! inspection. Later tickets extend it with fake-shim command scripting,
//! worktree corruption helpers, and time-warp primitives.

use std::collections::HashMap;
use std::path::PathBuf;

use batty_cli::shim::fake::FakeShim;
use batty_cli::shim::protocol::Event;
use batty_cli::task;
use batty_cli::team::daemon::TeamDaemon;
use batty_cli::team::daemon::tick_report::TickReport;
use batty_cli::team::harness::{TestHarness, engineer_member, manager_member};
use batty_cli::team::inbox::InboxMessage;
use batty_cli::team::standup::MemberState;

/// Default upper bound for `tick_until` so runaway scenarios fail fast
/// instead of spinning. 200 ≈ 200 poll cycles in model time; real
/// scenarios usually converge in <20.
pub const DEFAULT_TICK_BUDGET: usize = 200;

/// Returned by [`ScenarioFixture::tick_until`] when the predicate does
/// not fire within the tick budget.
#[derive(Debug)]
pub struct TickBudgetExceeded {
    pub budget: usize,
    pub last_cycle: u64,
}

impl std::fmt::Display for TickBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tick_until budget exceeded: drove {} ticks, last cycle = {}",
            self.budget, self.last_cycle
        )
    }
}

impl std::error::Error for TickBudgetExceeded {}

/// Top-level scenario harness. Owns a `TestHarness` (which owns the
/// `TempDir`) and a built `TeamDaemon`. Dropped at end of scope, which
/// cleans up the tempdir.
#[allow(dead_code)]
pub struct ScenarioFixture {
    harness: TestHarness,
    daemon: TeamDaemon,
    engineers: Vec<String>,
    fakes: HashMap<String, FakeShim>,
}

#[allow(dead_code)]
impl ScenarioFixture {
    /// Construct a fresh fixture builder.
    pub fn builder() -> ScenarioFixtureBuilder {
        ScenarioFixtureBuilder::default()
    }

    /// Drive one productive iteration of the daemon. Returns the tick
    /// report; any subsystem errors are captured in `report.subsystem_errors`.
    pub fn tick(&mut self) -> TickReport {
        self.daemon.tick()
    }

    /// Drive `n` consecutive ticks, returning every report in order.
    pub fn tick_n(&mut self, n: usize) -> Vec<TickReport> {
        (0..n).map(|_| self.tick()).collect()
    }

    /// Drive ticks until the predicate returns true, up to the default
    /// tick budget. Returns the matching report, or
    /// [`TickBudgetExceeded`] if no tick satisfied the predicate.
    pub fn tick_until<F>(&mut self, mut pred: F) -> Result<TickReport, TickBudgetExceeded>
    where
        F: FnMut(&TickReport) -> bool,
    {
        self.tick_until_with_budget(DEFAULT_TICK_BUDGET, &mut pred)
    }

    /// Same as `tick_until` but with an explicit tick budget.
    pub fn tick_until_with_budget<F>(
        &mut self,
        budget: usize,
        pred: &mut F,
    ) -> Result<TickReport, TickBudgetExceeded>
    where
        F: FnMut(&TickReport) -> bool,
    {
        let mut last_cycle = 0u64;
        for _ in 0..budget {
            let report = self.tick();
            last_cycle = report.cycle;
            if pred(&report) {
                return Ok(report);
            }
        }
        Err(TickBudgetExceeded { budget, last_cycle })
    }

    /// Absolute path to the project root (the fixture's tempdir).
    pub fn project_root(&self) -> &std::path::Path {
        self.harness.project_root()
    }

    /// Absolute path to the board tasks directory.
    pub fn board_tasks_dir(&self) -> PathBuf {
        self.harness.board_tasks_dir()
    }

    /// Every engineer name in fixture order.
    pub fn engineers(&self) -> &[String] {
        &self.engineers
    }

    /// IDs of every task on the board, sorted ascending.
    pub fn task_ids(&self) -> Vec<u32> {
        let Ok(tasks) = task::load_tasks_from_dir(&self.board_tasks_dir()) else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = tasks.into_iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids
    }

    /// Mutable access to the underlying daemon for escape hatches. Tests
    /// that need more than the curated fixture API can reach in here,
    /// but new scenarios should prefer adding a method on
    /// `ScenarioFixture` first.
    pub fn daemon_mut(&mut self) -> &mut TeamDaemon {
        &mut self.daemon
    }

    // -----------------------------------------------------------------
    // Fake shim wiring
    // -----------------------------------------------------------------

    /// Install a fresh [`FakeShim`] for `member`, registering the parent
    /// side of the socketpair as a daemon shim handle and marking it as
    /// Ready (Idle with a recorded Pong). The fake is owned by the
    /// fixture and accessible via [`Self::shim`].
    pub fn insert_fake_shim(&mut self, member: &str) {
        let (fake, parent_channel) =
            FakeShim::new_pair(member).expect("FakeShim::new_pair succeeds on test sockets");
        self.daemon.scenario_hooks().insert_fake_shim(
            member,
            parent_channel,
            0,
            "claude",
            "claude",
            self.harness.project_root().to_path_buf(),
        );
        // Fresh handles start in `Starting`. Scenarios nearly always
        // want the handle ready for dispatch immediately, so flip it.
        self.daemon.scenario_hooks().mark_shim_ready(member);
        self.fakes.insert(member.to_string(), fake);
    }

    /// Mutable access to a previously-installed fake shim.
    pub fn shim(&mut self, member: &str) -> &mut FakeShim {
        self.fakes
            .get_mut(member)
            .unwrap_or_else(|| panic!("no fake shim registered for '{member}'"))
    }

    /// Send a message to `member` through their registered shim handle.
    /// Scenarios use this to inject a synthetic dispatch without going
    /// through the full auto-dispatch pipeline.
    pub fn send_to_shim(&mut self, member: &str, from: &str, body: &str) {
        self.daemon
            .scenario_hooks()
            .send_to_shim(member, from, body)
            .expect("send_to_shim");
    }

    /// Let `member`'s fake shim drain pending commands and emit its
    /// scripted response events. Returns the emitted event sequence.
    pub fn process_shim(&mut self, member: &str) -> Vec<Event> {
        let worktree = self.harness.project_root().to_path_buf();
        self.fakes
            .get_mut(member)
            .unwrap_or_else(|| panic!("no fake shim registered for '{member}'"))
            .process_inbound(&worktree)
            .expect("process_inbound")
    }

    /// Pre-seed the daemon's `active_tasks` map so completion handlers
    /// downstream of fake shims can route into the merge pipeline.
    pub fn set_active_task(&mut self, member: &str, task_id: u32) {
        self.daemon
            .scenario_hooks()
            .set_active_task(member, task_id);
    }

    /// Override the daemon's in-memory [`MemberState`] for `member`.
    pub fn set_member_state(&mut self, member: &str, state: MemberState) {
        self.daemon.scenario_hooks().set_member_state(member, state);
    }

    // -----------------------------------------------------------------
    // Board / inbox inspection
    // -----------------------------------------------------------------

    /// Pending (undelivered) inbox messages for `member`. Scenarios use
    /// this to assert that an intervention or completion routed a
    /// message to the expected recipient.
    pub fn pending_inbox_for(&self, member: &str) -> Vec<InboxMessage> {
        self.harness
            .pending_inbox_messages(member)
            .unwrap_or_default()
    }

    /// Write a raw task file directly into the tempdir's board/tasks
    /// directory. Scenarios use this to seed shapes the builder's
    /// `with_task` helper can't express (legacy frontmatter, review
    /// timestamps, etc.).
    pub fn write_raw_task_file(&self, filename: &str, contents: &str) -> PathBuf {
        let tasks_dir = self.board_tasks_dir();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join(filename);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Append a synthetic `TeamEvent`-shaped JSON line to the team
    /// events.jsonl. Scenarios use this to simulate events emitted in
    /// prior daemon sessions (e.g. stale stall_detected events).
    pub fn append_raw_event_line(&self, json_line: &str) {
        let events_path = self.daemon_events_path();
        if let Some(parent) = events_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .unwrap();
        writeln!(file, "{json_line}").unwrap();
    }

    /// Absolute path to `.batty/team_config/events.jsonl` for this
    /// fixture's project root.
    pub fn daemon_events_path(&self) -> PathBuf {
        self.harness
            .project_root()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl")
    }

    /// Cross-subsystem invariant check. Phase 1 version validates the
    /// minimal invariants the fixture can express: every tracked
    /// active_task corresponds to a task on the board, and no board
    /// task has inconsistent claimed_by/status combinations. Scenarios
    /// call this at the end to pin the invariants that the cross-
    /// feature catalog enforces.
    pub fn assert_state_consistent(&mut self) {
        let tasks = task::load_tasks_from_dir(&self.board_tasks_dir())
            .expect("board tasks should be loadable");

        // Every task that is claimed must have a status compatible
        // with being claimed (in-progress, review, blocked, or done).
        for task in &tasks {
            if let Some(claimed_by) = task.claimed_by.as_deref() {
                assert!(
                    matches!(
                        task.status.as_str(),
                        "in-progress" | "review" | "blocked" | "done" | "archived"
                    ),
                    "task #{} claimed by {claimed_by} has invalid status {:?}",
                    task.id,
                    task.status
                );
            }
        }

        // Every tracked active task must exist on the board (either
        // in-progress, review, blocked, or done — never vanished).
        let engineers = self.engineers.clone();
        for engineer in &engineers {
            if let Some(task_id) = self.daemon.scenario_hooks().active_task_for(engineer) {
                let present = tasks.iter().any(|t| t.id == task_id);
                assert!(
                    present,
                    "engineer {engineer} has active_task {task_id} but it's not on the board"
                );
            }
        }
    }
}

/// Builder for [`ScenarioFixture`]. Defaults: no engineers, no manager,
/// no architect, no tasks. Scenarios call the `with_*` methods to shape
/// the team before `build`.
#[derive(Default)]
pub struct ScenarioFixtureBuilder {
    engineer_count: usize,
    manager_name: Option<String>,
    tasks: Vec<TaskSeed>,
}

struct TaskSeed {
    id: u32,
    title: String,
    status: String,
    claimed_by: Option<String>,
}

impl ScenarioFixtureBuilder {
    /// Add `n` engineers named `eng-1` through `eng-n`, each reporting
    /// to the configured manager (if any).
    pub fn with_engineers(mut self, n: usize) -> Self {
        self.engineer_count = n;
        self
    }

    /// Add a manager with the given name. Every engineer added
    /// afterward will report to this manager.
    pub fn with_manager(mut self, name: impl Into<String>) -> Self {
        self.manager_name = Some(name.into());
        self
    }

    /// Seed a task on the board before the daemon starts.
    pub fn with_task(
        mut self,
        id: u32,
        title: impl Into<String>,
        status: impl Into<String>,
        claimed_by: Option<&str>,
    ) -> Self {
        self.tasks.push(TaskSeed {
            id,
            title: title.into(),
            status: status.into(),
            claimed_by: claimed_by.map(str::to_string),
        });
        self
    }

    /// Finalize the fixture. Panics on any construction error — this is
    /// a test helper, not production code.
    pub fn build(self) -> ScenarioFixture {
        // Seed a TestHarness, then layer on the scenario-specific bits.
        let mut harness = TestHarness::new();

        // Bootstrap the tasks dir unconditionally. Several subsystems
        // (owned-tasks intervention, auto-unblock, cron recycle) treat
        // a missing directory as an error and leak into
        // TickReport::subsystem_errors if we don't create it up-front.
        std::fs::create_dir_all(harness.board_tasks_dir()).unwrap();

        // Manager first so engineers can point at it via reports_to.
        if let Some(ref manager_name) = self.manager_name {
            harness = harness.with_member(manager_member(manager_name, None));
        }

        let mut engineers = Vec::new();
        for idx in 1..=self.engineer_count {
            let name = format!("eng-{idx}");
            harness = harness.with_member(engineer_member(
                &name,
                self.manager_name.as_deref(),
                false, // use_worktrees is scenario-specific; default off
            ));
            engineers.push(name);
        }

        // Seed tasks before building the daemon so the initial reconcile
        // sees them.
        for TaskSeed {
            id,
            title,
            status,
            claimed_by,
        } in self.tasks.iter()
        {
            harness = harness.with_board_task(*id, title, status, claimed_by.as_deref());
        }

        let daemon = harness.build_daemon().expect("build_daemon");

        ScenarioFixture {
            harness,
            daemon,
            engineers,
            fakes: HashMap::new(),
        }
    }
}
