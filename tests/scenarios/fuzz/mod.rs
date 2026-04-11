//! Fuzz harness for the scenario framework.
//!
//! Phase 3 of the scenario framework plan:
//! - `model` — pure data types for the reference model
//! - `reference_sm` — the `ReferenceStateMachine` impl that drives
//!   deterministic shrinking
//! - `sut` — `StateMachineTest` impl that applies generated
//!   transitions to the real [`ScenarioFixture`] (ticket #644)
//! - `invariants` — cross-cutting checks enforced after every
//!   transition (ticket #645)
//!
//! Ticket #643 ships the model + reference SM. Several transition
//! variants and helper methods are declared here for use by later
//! tickets (#644 / #645); `#[allow(dead_code)]` keeps the catalog
//! complete without triggering unused-code warnings in the
//! intermediate state.

#![allow(dead_code)]

pub mod model;
pub mod reference_sm;
pub mod sut;
