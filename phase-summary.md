# Bug-Fix Phase Summary

## What Was Done

Completed all 8 backlog tasks on `.batty/kanban/bug-fix` and moved them to `done`.

1. Unset `CLAUDECODE` for additional tmux windows so parallel slots can launch nested Claude sessions safely.
2. Verified Claude adapter interactive prompt handling was already fixed (positional argument, no `--prompt`).
3. Added `batty install` config scaffolding for `.batty/config.toml` (create-if-missing, never overwrite).
4. Replaced stale `batty work all --parallel` error message that referenced Phase 4 planning.
5. Fixed parallel launch context claim identity conflicts by making per-slot claim identity authoritative in composed context.
6. Documented milestone-tag completion requirement in getting-started and troubleshooting docs.
7. Applied documentation audit updates (phase status, command naming consistency, stale test-count references, execution-loop status).
8. Fixed completion DoD reporting/execution behavior when `defaults.dod` is unset.

## Files Changed

### Runtime/Code
- `src/tmux.rs`
- `src/install.rs`
- `src/main.rs`
- `src/work.rs`
- `src/completion.rs`

### Documentation
- `docs/getting-started.md`
- `docs/troubleshooting.md`
- `docs/reference/modules.md`
- `planning/roadmap.md`
- `planning/execution-loop.md`
- `README.md`
- `AGENTS.md`
- `CLAUDE.md`
- `.batty/kanban/phase-4/PHASE.md`

### Board/Task Records
- `.batty/kanban/bug-fix/PHASE.md`
- `.batty/kanban/bug-fix/tasks/001-unset-claudecode-env.md`
- `.batty/kanban/bug-fix/tasks/002-fix-claude-prompt-flag.md`
- `.batty/kanban/bug-fix/tasks/003-install-scaffold-config.md`
- `.batty/kanban/bug-fix/tasks/004-fix-stale-parallel-error-msg.md`
- `.batty/kanban/bug-fix/tasks/005-fix-parallel-claim-conflict.md`
- `.batty/kanban/bug-fix/tasks/006-document-milestone-tag.md`
- `.batty/kanban/bug-fix/tasks/007-fix-docs-issues.md`
- `.batty/kanban/bug-fix/tasks/008-default-dod-cargo-test.md`
- `.batty/kanban/bug-fix/activity.jsonl`

## Tests Added or Modified

Added tests:
- `tmux::tests::create_window_unsets_claudecode_from_session_environment` (`src/tmux.rs`)
- `install::tests::install_does_not_overwrite_existing_config` (`src/install.rs`)
- `completion::tests::completion_passes_without_dod_when_unset` (`src/completion.rs`)

Updated test expectations:
- install tests that assert created/unchanged assets now include `.batty/config.toml` scaffold behavior.

## Tests Run

- `cargo test create_window_unsets_claudecode_from_session_environment -- --nocapture`
- `cargo test agent::claude -- --nocapture`
- `cargo test install::tests -- --nocapture`
- `cargo test run_phase_parallel -- --nocapture`
- `cargo run --bin batty -- work all --parallel 2 --dry-run`
- `cargo test completion::tests -- --nocapture`
- `cargo test status_bar_init_and_update -- --nocapture`
- `cargo test` (final): passed (`369 passed, 0 failed, 4 ignored`) on `src/main.rs` tests plus `24 passed` for `src/bin/docsgen.rs`

## Key Decisions and Why

- Used tmux command wrapping (`env -u CLAUDECODE`) in both session and window creation paths to make nested agent launch reliable across single and parallel execution.
- Kept parallel claim identity deterministic per slot by composing launch context with slot identity directly (`parallel-slot` source) instead of base single-agent identity.
- Implemented config scaffold as create-if-missing only to preserve user-owned config and ensure install idempotency.
- Treated unset DoD as explicit no-gate in completion logic and reported `dod_command` as `(none)` to avoid misleading `cargo test` defaults for non-Rust projects.
- Standardized docs on `batty list` while preserving `board-list` as alias mention for compatibility.

## Deferred or Left Open

- Direct verification of `batty work bug-fix --parallel 2 --dry-run` in this workspace was blocked by local git ref/worktree branch naming constraints (`refs/heads/batty/bug-fix/<agent>` creation failure). Parallel claim fix was validated via code path updates and parallel unit tests.
- Task #2 required validation only; no code change was needed because the adapter fix already existed in this branch.

## Follow-Up Watch Items

- `orchestrator::tests::status_bar_init_and_update` can fail if a stale tmux test session already exists; cleanup or stronger test isolation may reduce flakiness.
- Parallel worktree branch naming/path creation should be hardened against repository ref layout conflicts observed during dry-run verification.
- Keep docs test-count references synchronized with actual `cargo test -- --list` counts to avoid recurrent drift.
