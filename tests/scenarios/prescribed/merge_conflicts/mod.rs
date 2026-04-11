//! Merge conflict scenarios — exercise the rebase retry and
//! cherry-pick fallback paths of the merge queue. Phase 1 scope is
//! subsystem-health (no crash on conflicting state); full merge
//! pipeline end-to-end lands after the happy-path merge cycle is
//! wired into the fixture.

pub mod cherry_pick_fallback;
pub mod rebase_retry;
