//! Regression catalog — one scenario per bug fix shipped in a recent
//! release. Each scenario pins a fix so reverting it causes the test
//! to fail. See ticket #641.

pub mod branch_recovery;
pub mod disk_emergency;
pub mod frontmatter_idempotent;
pub mod preserve_dedup;
pub mod review_queue_aging;
pub mod scope_check_base;
pub mod stall_cross_session;
