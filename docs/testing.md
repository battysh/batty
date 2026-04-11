# Testing

Batty has three test surfaces. Pick the one that matches what you're
trying to protect:

| Surface                                                    | When to use                                                                                          | Runs in CI?              |
| ---------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- | ------------------------ |
| **`cargo test --lib`**                                     | Unit tests for any module. Fast, no tmux, no subprocess.                                             | Yes, on every PR         |
| **`cargo test --features integration`**                    | Tmux-dependent integration tests.                                                                    | No (needs a tmux server) |
| **`cargo test --test scenarios --features scenario-test`** | End-to-end scenarios driving the real `TeamDaemon` with in-process fake shims on a per-test tempdir. | Yes, on every PR         |

This document covers the **scenario framework** — the end-to-end
harness introduced in phase 1–3 under tickets #636–#646. For tmux
integration tests see the existing `src/team/harness.rs` docs; for
unit testing conventions see `CLAUDE.md`.

## What the scenario framework gives you

A scenario test is a single `#[test]` in `tests/scenarios/prescribed/`
that:

1. Builds a `ScenarioFixture` (tempdir + git repo + kanban board +
   TeamDaemon + optional fake shims).
1. Drives the daemon through a deterministic sequence of ticks and
   asserts against `TickReport` + fixture state between calls.
1. Runs in under 300 ms and cleans up on drop — no tmux, no
   subprocess, no shared state.

Phase 1 ships 22 prescriptive scenarios (one happy path + 7
regressions + 14 cross-feature) plus a randomized `proptest`-driven
fuzz harness with 10 invariants (ticket #645). Every recent release
bug has a dedicated regression scenario, and the fuzz targets
generate randomized workflow sequences that shrink to minimal
reproducers when an invariant fails.

## Running the suite

```bash
# Full scenario suite + fuzz smoke (~60s)
cargo test --test scenarios --features scenario-test

# Just the prescriptive catalog
cargo test --test scenarios --features scenario-test prescribed::

# Just the regression catalog
cargo test --test scenarios --features scenario-test regressions::

# Just the fuzz targets (each case spawns a real TeamDaemon)
cargo test --test scenarios --features scenario-test fuzz_workflow

# Fuzz with more cases (nightly-style)
PROPTEST_CASES=512 cargo test --test scenarios --features scenario-test \
    --release fuzz_workflow_happy
```

CI runs the default suite on every PR and a nightly fuzz job with
`PROPTEST_CASES=2048` scheduled at 02:15 UTC. Both live in
`.github/workflows/ci.yml`.

## Writing a new prescriptive scenario

Every prescriptive scenario lives in `tests/scenarios/prescribed/`.
Keep each file under 80 lines (regressions) or 150 lines (cross-
feature). Start from the following template:

```rust
//! My scenario: <what this test proves>.
//!
//! Failure mode being protected: <what would break if the fix
//! regressed>.

use super::super::scenarios_common::ScenarioFixture;

#[test]
fn my_scenario_does_the_thing() {
    // 1. Build the fixture with the team shape this scenario needs.
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .with_task(1, "my task", "todo", None)
        .build();

    // 2. Set up the initial fault state (optional).
    //    Examples: write_raw_task_file, append_raw_event_line,
    //    insert_fake_shim, set_active_task, etc.

    // 3. Drive the daemon.
    let report = fixture.tick();

    // 4. Assert against `report` and/or fixture state.
    assert!(report.subsystem_errors.is_empty());

    // 5. Always end with the consistency check.
    fixture.assert_state_consistent();
}
```

Then add a `pub mod my_scenario;` line to the appropriate
`mod.rs` (either `prescribed/mod.rs` or a subdirectory's
`mod.rs`). Run `cargo test --test scenarios --features scenario-test my_scenario` to make sure it passes, then commit.

If your scenario needs internal daemon state that the public
`ScenarioFixture` API doesn't expose, add a method on
`ScenarioHooks` in `src/team/daemon/scenario_api.rs`. That's the one
documented seam for reaching into the daemon from integration tests;
do NOT widen visibility on daemon fields.

## Fake shims

An in-process fake shim replaces a real agent subprocess with
scripted behavior. Typical usage:

```rust
use batty_cli::shim::fake::ShimBehavior;
use std::path::PathBuf;

fixture.insert_fake_shim("eng-1");

fixture.shim("eng-1").queue(ShimBehavior::CompleteWith {
    response: "implemented the thing".to_string(),
    files_touched: vec![(
        PathBuf::from("src/lib.rs"),
        "// new code\n".to_string(),
    )],
});

// Simulate a dispatch from the manager and drain the fake's response.
fixture.send_to_shim("eng-1", "manager", "implement src/lib.rs");
let _events = fixture.process_shim("eng-1");

// Tick the daemon so it observes the completion.
let report = fixture.tick();
```

`ShimBehavior` variants:

- `CompleteWith` — happy-path completion; commits the listed files.
- `NarrationOnly` — Completion with zero files.
- `NarrationFirstThenClean` — first call is narration, second is
  clean.
- `ErrorOut` — emits `Event::Error`.
- `ContextExhausted` — emits `Event::ContextExhausted`.
- `Silent` — swallows the command with no response.
- `Script(Vec<Event>)` — verbatim event sequence.
- `Once(Box<ShimBehavior>)` — apply inner once, then revert.

## Fuzz targets

The fuzz harness lives under `tests/scenarios/fuzz/`:

- `model.rs` — pure `ModelBoard` + `Transition` data types.
- `reference_sm.rs` — `ReferenceStateMachine` impl + pure `apply`
  oracle.
- `sut.rs` — `StateMachineTest` impl mapping each transition to
  concrete `ScenarioFixture` operations.
- `invariants.rs` — ten cross-subsystem invariants checked after
  every transition.
- `fuzz_workflow.rs` — three `prop_state_machine!` targets.

### Reading a fuzz failure

When a fuzz case fails, proptest prints a seed and the full
transition sequence that triggered the failure. Example output:

```
thread 'fuzz::fuzz_workflow::fuzz_workflow_happy' panicked at ...
assertion failed: claim_exclusivity: two engineers claim task #5
...
Minimal failing input: (
    initial_state: ModelBoard { ... },
    transitions: [
        DispatchTask { task_id: 5, engineer: "eng-1" },
        DispatchTask { task_id: 5, engineer: "eng-2" },
    ]
)
```

The `transitions` list is already shrunk to the minimal reproducer.
Copy it verbatim into a new file under
`tests/scenarios/prescribed/regressions/` following the template
above. The shrunk sequence becomes a permanent regression guard that
runs on every PR.

### Re-running a specific fuzz case

Proptest writes failing seeds to
`tests/scenarios/fuzz/proptest-regressions/`. To re-run a specific
seed:

```bash
PROPTEST_CASES=1 \
  PROPTEST_REPLAY=<seed-from-output> \
  cargo test --test scenarios --features scenario-test fuzz_workflow_happy
```

Or bump `PROPTEST_CASES` to exhaustively re-probe the same shape:

```bash
PROPTEST_CASES=1024 cargo test --test scenarios --features \
  scenario-test fuzz_workflow_happy
```

## Debugging a flaky scenario

Run a single scenario in a tight loop:

```bash
for i in $(seq 1 20); do
  cargo test --test scenarios --features scenario-test my_scenario \
    || { echo "FAILED on run $i"; break; }
done
```

If the failure is intermittent, the most likely cause is:

1. **Wall-clock time dependency** — some timer in the daemon fires
   on real-time bounds. Use `ScenarioHooks::backdate_*` to set
   timestamps explicitly instead of waiting.
1. **Parallel test ordering** — two tests mutating the same global.
   All scenario tests should be hermetic (their own `TempDir`); if
   you see this, check for `std::env` or `PATH` mutation.
1. **Socketpair drain ordering** — `process_shim` must be called
   between `send_to_shim` and the next `tick` so the fake can
   respond before the daemon polls.

## Non-goals

The scenario framework deliberately does NOT:

- Spawn real `claude` / `codex` / `kiro` subprocesses. Those live in
  `src/shim/tests_sdk.rs`, `tests_codex.rs`, `tests_kiro.rs`.
- Touch tmux. That's the `integration` feature.
- Mutate `std::env` or `PATH`. Every test must be hermetic.
- Use `fail-rs` failpoints. Phase 4 (future) adds source-level fault
  injection; phase 1 relies on `ShimBehavior` and direct filesystem
  manipulation.

## Related

- Design plan: `~/.claude/plans/serene-pondering-snowglobe.md`
- Execution order: `planning/scenario-framework-execution.md`
- Tickets #636–#646 on the batty board cover every phase-1 deliverable.
