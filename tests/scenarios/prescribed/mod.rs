//! Prescribed scenario catalog.
//!
//! Every file here is a named, deterministic scenario: a real
//! `TeamDaemon` driven against in-process fake shims on a per-test
//! tempdir. Phase 1 ships the happy path + 7 regressions (tickets
//! #640 and #641); phase 2 adds the 14 cross-feature scenarios in
//! ticket #642.

pub mod ack_loops;
pub mod context_exhausted;
pub mod disk_pressure_size_tier;
pub mod happy_path;
pub mod merge_conflicts;
pub mod multi_engineer;
pub mod narration_only;
pub mod regressions;
pub mod scope_fence_violations;
pub mod silent_death;
pub mod stale_merge_lock;
pub mod state_desync;
pub mod worktree_corruption;
