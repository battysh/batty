---
id: 2
title: Prompt composition and context injection
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:40:10.555836726-05:00
started: 2026-02-21T22:32:55.488038167-05:00
completed: 2026-02-21T22:40:10.555836326-05:00
tags:
    - core
    - launch
claimed_by: brisk-frost
claimed_at: 2026-02-21T22:40:10.555836676-05:00
class: standard
---

Define and implement deterministic executor launch context.

## Requirements

1. Compose launch prompt from:
   - `CLAUDE.md` or active agent instructions
   - `kanban/<phase>/PHASE.md`
   - current board state (task titles/status/dependencies)
   - `.batty/config.toml` policy and execution defaults
2. Persist composed prompt/input snapshot to logs for auditability.
3. Validate required context files and fail with actionable errors.
4. Support agent-specific prompt wrappers via adapter layer.

## Exit signal for this task

- A dry-run mode shows the exact composed launch context before execution.

## Statement of Work

- **What was done:** Implemented deterministic launch-context composition for `batty work`, including required file validation, config/policy snapshot rendering, adapter-level prompt wrappers, persisted launch snapshot logging, and a `--dry-run` mode that prints the exact executor input without launching supervision.
- **Files created:** None.
- **Files modified:** `src/work.rs` (new launch-context composer, validation, dry-run path, snapshot persistence), `src/agent/mod.rs` (adapter wrapper/instruction-candidate hooks), `src/agent/codex.rs` (Codex-specific wrapper + instruction priority), `src/log/mod.rs` (launch-context snapshot event), `src/cli.rs` and `src/main.rs` (`--dry-run` wiring and detached-mode guard), `src/worktree.rs` (dry-run cleanup outcome), `README.md` (dry-run usage example).
- **Key decisions:** Made adapter wrappers explicit in trait defaults so each agent can customize launch framing without branching work pipeline logic; logged both snapshot path and full snapshot content for auditability; treated `PHASE.md` and agent instructions as hard requirements with actionable failure messages.
- **How to verify:** `cargo test work::tests`; `cargo test agent::tests`; `cargo test agent::codex::tests`; `cargo test log::tests::all_event_types_serialize`; `cargo run -- work phase-2.5 --dry-run --foreground`.
- **Open issues:** Full `cargo test` still includes many tmux/PTY integration tests that fail in restricted sandbox environments (`Operation not permitted`), unrelated to this task's logic.
