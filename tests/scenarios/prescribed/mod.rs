//! Prescribed scenario catalog.
//!
//! Every file here is a named, deterministic scenario: a real
//! `TeamDaemon` driven against in-process fake shims on a per-test
//! tempdir. Phase 1 ships the happy path; the regression catalog and
//! cross-feature scenarios land in later tickets (#641, #642).

pub mod happy_path;
pub mod regressions;
