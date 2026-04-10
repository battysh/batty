//! Merge orchestration extracted from the team daemon.
//!
//! This module owns the completion path after an engineer reports a task as
//! done in a worktree-based flow. It validates that the branch contains real
//! work, runs the configured test gate, serializes merges with a lock, and
//! either lands the branch on `main` or escalates conflicts and failures back
//! through the daemon.
//!
//! The daemon calls into this module so the poll loop can stay focused on
//! orchestration while merge-specific retries and board transitions remain in
//! one place.

mod completion;
mod git_ops;
mod lock;
mod operations;

pub(crate) use completion::handle_engineer_completion;
pub(crate) use completion::record_merge_test_timing;
pub(crate) use lock::{MergeLock, MergeMode, MergeOutcome, infer_merge_mode_from_failure};
pub(crate) use operations::merge_engineer_branch;
