# Scenario framework execution plan

This is the execution-side companion to `~/.claude/plans/serene-pondering-snowglobe.md` (the design plan). Every ticket in the board under this plan maps to a concrete step here. The 15-minute nudge points at this file.

## Goal

Build an end-to-end scenario test framework for batty that:
- Uses **in-process fake shims** (no subprocesses, no tmux, no real agents).
- Runs on **real git repos in `tempfile::TempDir`** with real board files, real worktrees, real inboxes.
- Drives the daemon **one tick at a time** so assertions are deterministic and bounded.
- Has a **prescriptive catalog** (every known failure mode from the current session) AND a **randomized state-machine fuzzer** (proptest-state-machine) sharing the same harness.
- Runs **parallel by default** with no `PATH_LOCK`, no global state mutation, no flakes.

## Ordered ticket list

Work the tickets in this order. Later tickets depend on infrastructure landed by earlier ones.

| # | Ticket | File(s) touched | Deliverable |
|---|---|---|---|
| 1 | 636 `tick()` refactor + `TickReport` | `src/team/daemon/poll.rs` | `pub fn tick(&mut self) -> TickReport`; `run()` calls `tick()` in a loop with the existing 5s sleep |
| 2 | 637 `scenario-test` feature flag + `ScenarioHooks` | `Cargo.toml`, `src/team/scenario_api.rs`, `src/team/daemon.rs` | Feature-gated public test surface: `insert_fake_shim`, `backdate_*`, `inspect_*` |
| 3 | 638 `FakeShim` and `ShimBehavior` | `src/shim/fake.rs`, `src/shim/mod.rs`, `src/lib.rs` | `FakeShim` wrapping the child side of a real socketpair, scriptable via `ShimBehavior::{CompleteWith, ErrorOut, ContextExhausted, Silent, NarrationOnly, Script}` |
| 4 | 639 `ScenarioFixture` harness | `tests/scenarios/common/{fixture,fake_shim,board_ops,time_warp,assert,chaos}.rs`, `tests/scenarios/mod.rs` | The top-level harness composing TempDir + git + board + daemon + fakes; `tick_until`, `commit_on_branch`, `corrupt_worktree`, `force_stall_timeout`, `assert_state_consistent` |
| 5 | 640 happy-path scenario | `tests/scenarios/prescribed/happy_path.rs` | End-to-end: dispatch → fake shim commits real code → verification → auto-merge → main advances. Proves the harness can drive a full cycle. |
| 6 | 641 regression scenario catalog (7 scenarios) | `tests/scenarios/prescribed/regressions/*.rs` | One scenario per recent release bug: scope_check_base, preserve_dedup, branch_recovery, disk_emergency, stall_cross_session, frontmatter_idempotent, review_queue_aging |
| 7 | 642 cross-feature prescribed catalog (14 scenarios) | `tests/scenarios/prescribed/*.rs` | worktree_corruption, merge_conflicts, narration_only, scope_fence_violations, state_desync, ack_loops, context_exhausted, silent_death, multi_engineer, disk_pressure, stale_merge_lock, orphan_review_owner, claim_ttl_expiry, task_dependency_cycle |
| 8 | 643 `ReferenceStateMachine` model | `tests/scenarios/fuzz/{model,reference_sm}.rs` | ModelBoard, ModelEngineer, Transition enum; preconditions and deterministic `apply` |
| 9 | 644 `StateMachineTest` SUT mapping | `tests/scenarios/fuzz/sut.rs` | Maps each `Transition` to one or more `ScenarioFixture` method calls; uses the same invariant checks as prescriptive scenarios |
| 10 | 645 fuzz targets + invariants | `tests/scenarios/fuzz/{invariants,fuzz_workflow}.rs` | `fuzz_workflow_happy`, `fuzz_workflow_with_faults`, `fuzz_restart_resilience` targets; 10 invariants enforced after every transition |
| 11 | 646 CI wiring + docs | `.github/workflows/*`, `docs/testing.md` (new) | `cargo test --test scenarios --features scenario-test` in CI; doc explains how to write new prescriptive scenarios and how to read proptest shrinks |

Total: **11 tickets** delivering phases 1–3 of the design plan.

## Definitions of done (per ticket)

- Code compiles with `cargo build --release`.
- `cargo test --lib` is still green (no regression in existing 3,354 tests).
- `cargo test --features scenario-test` passes (harness-specific tests).
- `cargo fmt --check` clean.
- For each ticket that adds a scenario file: the scenario runs in <300ms and passes 10 consecutive runs locally.
- No changes to visibility of daemon internals outside `ScenarioHooks`.
- No use of `PATH_LOCK`, no real subprocess spawn, no real tmux.
- Each ticket lands as its own git commit on main.

## Non-goals (guardrails)

- **No real agents.** `FakeShim` only.
- **No tmux.** Scenarios never touch `src/tmux.rs`.
- **No refactoring existing tests.** Phase 1 adds; it does not rewrite.
- **No `fail-rs` in phase 1.** Fault injection uses `ShimBehavior` + direct filesystem manipulation only. `fail-rs` is a phase 4 add-on.
- **No network deps.** Discord/Telegram/Grafana are skipped in scenarios.

## Reference plan

Full design rationale in `~/.claude/plans/serene-pondering-snowglobe.md`. That file explains **why** each piece exists; this file tracks **what order to build them in**.

## How to pick the next ticket

1. `grep -l "status: todo" .batty/team_config/board/tasks/63{6,7,8,9}*.md .batty/team_config/board/tasks/64{0,1,2,3,4,5,6}*.md | head -1`
2. Read the ticket body.
3. `batty task transition <id> in-progress`.
4. Implement. Run `cargo test --lib` + `cargo test --features scenario-test` before committing.
5. `git commit` with a message referencing the ticket ID.
6. `batty task transition <id> review && batty review <id> approve "..."`.
7. Move to the next ticket.
