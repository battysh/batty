//! Worktree corruption scenarios — prove the daemon handles
//! filesystem-level damage without panicking and (where the phase 1
//! infrastructure allows) recovers the engineer to a valid state.

pub mod broken_index;
pub mod detached_head;
pub mod missing_dir;
