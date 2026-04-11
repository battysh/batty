//! Fuzz targets driving the [`FuzzTest`] state machine with
//! `prop_state_machine!` (ticket #645).
//!
//! Three targets ship in phase 1:
//!
//! - `fuzz_workflow_happy` — small sequences over the full
//!   weighted alphabet. Default 32 cases × up to 20 transitions;
//!   release-mode runs complete in a few minutes.
//! - `fuzz_workflow_with_faults` — same alphabet, longer sequences
//!   (up to 40 transitions), default 32 cases.
//! - `fuzz_restart_resilience` — shorter sequences (up to 15
//!   transitions) with an emphasis on DaemonRestart interleaving
//!   driven by the weighted strategy.
//!
//! Why 32 cases instead of the design plan's 512? The phase 1 SUT
//! spawns a real `TeamDaemon` per invocation of `init_test` and
//! ticks it with real git operations — each case costs ~100ms in
//! debug mode, so 512 × 50 transitions would blow the <5min budget
//! on developer laptops. The target is tuned so `cargo test --test
//! scenarios --features scenario-test` stays under ~30 seconds for
//! the full fuzz suite in debug mode. CI can crank `PROPTEST_CASES`
//! to scale it up per ticket #646.
//!
//! ## Replaying a shrunk case
//!
//! When a fuzz target fails, proptest prints a seed. Re-run with:
//!
//! ```bash
//! PROPTEST_CASES=1 PROPTEST_REPLAY=... cargo test --test scenarios \
//!     --features scenario-test fuzz_workflow_happy
//! ```
//!
//! The shrunk transition sequence is minimal and can be pasted into
//! a new file under `prescribed/regressions/` verbatim.

#![cfg(test)]

use proptest::prelude::ProptestConfig;
use proptest_state_machine::prop_state_machine;

use super::sut::FuzzTest;

prop_state_machine! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        max_shrink_iters: 1024,
        .. ProptestConfig::default()
    })]

    #[test]
    fn fuzz_workflow_happy(sequential 1..20 => FuzzTest);

    #[test]
    fn fuzz_workflow_with_faults(sequential 1..40 => FuzzTest);

    #[test]
    fn fuzz_restart_resilience(sequential 1..15 => FuzzTest);
}
