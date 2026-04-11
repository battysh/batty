#![allow(private_interfaces)]

pub mod agent;
pub mod cli;
pub mod config;
pub mod console_pane;
pub mod env_file;
pub mod events;
pub mod log;
pub mod paths;
pub mod project_registry;
pub mod prompt;
pub mod release;
pub mod shim;
#[cfg(any(test, feature = "scenario-test"))]
pub use shim::fake::{FakeShim, ShimBehavior};
pub mod task;
pub mod team;
pub mod tmux;
pub mod worktree;
