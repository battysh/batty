//! Pure data model for the scenario framework fuzzer.
//!
//! Every type in this module is `Clone + Debug` so `proptest` can
//! shrink generated transition sequences. The model has no I/O, no
//! time, no filesystem — [`apply`](super::reference_sm::apply) is a
//! pure function.

use std::collections::BTreeMap;

/// Abstract state of a single task on the model board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTaskStatus {
    Todo,
    InProgress,
    Review,
    Done,
    Blocked,
}

/// Abstract state of a single engineer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelEngineerState {
    Idle,
    Working,
    Dead,
}

/// Worktree corruption shapes used by fault transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionKind {
    MissingDir,
    DetachedHead,
    BrokenIndex,
}

/// Frontmatter malformation shapes used by fault transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadFrontmatterShape {
    LegacyStringBlock,
    MissingBlockReason,
    HiddenInProgress,
}

/// Model representation of a task on the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTask {
    pub status: ModelTaskStatus,
    pub claimed_by: Option<String>,
    pub branch_commits: u32,
    pub merge_attempts: u32,
}

impl ModelTask {
    pub fn new_todo() -> Self {
        Self {
            status: ModelTaskStatus::Todo,
            claimed_by: None,
            branch_commits: 0,
            merge_attempts: 0,
        }
    }
}

/// Model representation of an engineer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEngineer {
    pub state: ModelEngineerState,
    pub active_task: Option<u32>,
    pub worktree_branch: Option<String>,
    pub dirty_lines: u32,
}

impl ModelEngineer {
    pub fn new_idle() -> Self {
        Self {
            state: ModelEngineerState::Idle,
            active_task: None,
            worktree_branch: None,
            dirty_lines: 0,
        }
    }
}

/// The top-level model state. Every reference transition takes
/// ownership of the previous state and returns a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelBoard {
    pub tasks: BTreeMap<u32, ModelTask>,
    pub engineers: BTreeMap<String, ModelEngineer>,
    pub main_tip: u32,
    pub merge_lock_held_by: Option<String>,
}

impl ModelBoard {
    pub fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            engineers: BTreeMap::new(),
            main_tip: 0,
            merge_lock_held_by: None,
        }
    }

    pub fn with_engineer(mut self, name: &str) -> Self {
        self.engineers
            .insert(name.to_string(), ModelEngineer::new_idle());
        self
    }

    pub fn with_task(mut self, id: u32) -> Self {
        self.tasks.insert(id, ModelTask::new_todo());
        self
    }

    pub fn idle_engineers(&self) -> Vec<String> {
        self.engineers
            .iter()
            .filter(|(_, e)| e.state == ModelEngineerState::Idle)
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn todo_task_ids(&self) -> Vec<u32> {
        self.tasks
            .iter()
            .filter(|(_, t)| t.status == ModelTaskStatus::Todo)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn engineer_mut(&mut self, name: &str) -> Option<&mut ModelEngineer> {
        self.engineers.get_mut(name)
    }

    pub fn task_mut(&mut self, id: u32) -> Option<&mut ModelTask> {
        self.tasks.get_mut(&id)
    }
}

impl Default for ModelBoard {
    fn default() -> Self {
        Self::new()
    }
}

/// The full alphabet of transitions the fuzzer can generate. Split
/// into "workflow" (happy-path) and "fault" (chaos) subsets; the SUT
/// (ticket #644) maps each variant to one or more `ScenarioFixture`
/// operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    // -------- Workflow alphabet --------
    DispatchTask {
        task_id: u32,
        engineer: String,
    },
    EngineerCommits {
        engineer: String,
        lines: u32,
    },
    ReportCompletion {
        engineer: String,
    },
    RunVerification {
        task_id: u32,
    },
    SubmitForMerge {
        task_id: u32,
    },
    MergeQueueTick,
    ReclaimExpiredClaim {
        task_id: u32,
    },
    FireStandup,
    FireNudge {
        engineer: String,
    },
    DaemonRestart,

    // -------- Fault alphabet --------
    ShimGoSilent {
        engineer: String,
    },
    ShimEmitError {
        engineer: String,
        reason: String,
    },
    ContextExhaust {
        engineer: String,
    },
    DirtyWorktree {
        engineer: String,
        lines: u32,
    },
    CorruptWorktree {
        engineer: String,
        kind: CorruptionKind,
    },
    BranchDrift {
        engineer: String,
        task_id: u32,
    },
    BadFrontmatter {
        task_id: u32,
        shape: BadFrontmatterShape,
    },
    StaleMergeLock,
    DiskPressure {
        free_gb: u64,
    },
    NarrationOnlyCompletion {
        engineer: String,
    },
    ScopeFenceViolation {
        engineer: String,
    },
    AdvanceTime {
        seconds: u64,
    },
}
