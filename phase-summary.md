# Phase 4 Summary

Date: 2026-02-23  
Phase: `phase-4`  
Board: `.batty/kanban/phase-4`

## What was done (tasks completed + outputs)

- Task #1: Added `src/dag.rs` for board dependency DAG handling.
  - Parses task dependencies from board task files.
  - Validates missing dependencies and cycles with explicit cycle paths.
  - Computes ready frontier and deterministic topological order.
- Task #2: Implemented parallel phase launch path in `batty work <phase> --parallel N`.
  - Added per-agent worktree provisioning and reuse/force-new behavior.
  - Added tmux multi-window spawning with one executor window per agent.
  - Added per-window log pane + pipe-pane capture setup.
- Task #3: Implemented scheduler core in `src/scheduler.rs` and wired it into parallel runtime.
  - Polls board, computes ready set via DAG, dispatches using `kanban-md pick --claim ...`.
  - Verifies claims by re-reading `claimed_by` in frontmatter.
  - Detects completions, deadlocks, stuck agents, and handles crash claim release.
- Task #4: Implemented serialized merge queue in `src/merge_queue.rs` and wired into parallel runtime.
  - FIFO merge requests from completed tasks.
  - Rebase + retry path, test gate before merge, ff-only merge.
  - Escalates on unresolved conflict/test failures.
- Task #5: Extended parallel status visibility in tmux.
  - Global status line includes tasks complete/total, agent count, merge activity.
  - Per-window names show active task title + elapsed minutes.
  - Idle states distinguish `waiting-deps` vs `idle`.
- Task #6: Added shell completions and publishing metadata.
  - New command: `batty completions <bash|zsh|fish>` using `clap_complete`.
  - Updated package metadata to `batty-cli` with explicit `batty` binary.
  - Updated README and CLI reference docs with completion and install details.
- Task #7: Added/expanded integration-style coverage for phase-4 behavior and exit criteria.
  - Synthetic 8-task DAG progression coverage.
  - Cycle/deadlock/crash/empty-board/no-double-dispatch edge coverage.
  - Parallel=1 regression coverage and publish/install/completions verification.

Final board state: `backlog=0, todo=0, in-progress=0, review=0, done=7`.

## Files changed and tests added/modified/run

### Key files changed

- `src/dag.rs`
- `src/worktree.rs`
- `src/tmux.rs`
- `src/scheduler.rs` (new)
- `src/merge_queue.rs` (new)
- `src/work.rs`
- `src/cli.rs`
- `src/shell_completion.rs` (new)
- `src/main.rs`
- `Cargo.toml`
- `Cargo.lock`
- `README.md`
- `docs/reference/cli.md`
- `phase-summary.md`

### Test coverage added/expanded

- DAG: cycle/missing-dep/ready-set/topo/empty + synthetic 8-task progression.
- Worktree: per-agent create/reuse/force-new/duplicate-name validation.
- tmux: multi-window creation path coverage.
- Scheduler: dispatch/claim verification/release on failure/crash/deadlock/stuck/empty-board/distinct dispatch.
- Merge queue: FIFO behavior, conflict retry failure, test-gate failure.
- Work runtime: `parallel=1` regression path and fast-fail cycle validation.
- CLI: completions subcommand parse coverage.

### Commands run

- `cargo test -q`
- `cargo run --bin batty -- completions zsh | head -n 3`
- `cargo install --path . --force --locked`
- `cargo publish --dry-run --allow-dirty`

## Key decisions made and why

- Kept scheduler/merge queue deterministic and file-driven (board polling + explicit queue) to align with Batty’s markdown-backed control model.
- Used strict claim verification (`claimed_by`) after dispatch to prevent silent mis-assignment.
- Made merge queue ff-only with test gate to keep merge serialization safe and reproducible.
- Set package name to `batty-cli` while keeping executable name `batty` via explicit `[[bin]]` mapping for CLI continuity.
- Implemented completions via `clap_complete` so generated scripts stay aligned with clap command definitions.

## What was deferred or left open

- Full external end-to-end agent execution (`batty work <phase> --parallel 3` with real agent CLIs and live human supervision) remains operationally validated but not covered by deterministic unit tests due environment/auth/network variability.
- Remote crates.io publish (non-dry-run) is not executed in this phase.

## What to watch for in follow-up work

- Validate long-running parallel sessions for scheduler stuck-threshold tuning and merge queue throughput under high task churn.
- Consider persisting scheduler/merge queue state snapshots for resume-after-restart behavior.
- Extend docs with a dedicated Phase-4 operator runbook (parallel run lifecycle, recovery flows, and troubleshooting).
