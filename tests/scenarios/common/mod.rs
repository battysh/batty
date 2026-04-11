//! Common scenario framework harness.
//!
//! Exposed modules and re-exports used by tests in `tests/scenarios.rs`.
//! Keep this file tiny — actual implementation lives in submodules.

pub mod board_ops;
pub mod fixture;

#[allow(unused_imports)]
pub use fixture::{ScenarioFixture, ScenarioFixtureBuilder, TickBudgetExceeded};
