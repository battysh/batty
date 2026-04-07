<!-- AUTONOMY DIRECTIVE — DO NOT REMOVE -->
YOU ARE AN AUTONOMOUS CODING AGENT. EXECUTE TASKS TO COMPLETION WITHOUT ASKING FOR PERMISSION.
DO NOT STOP TO ASK "SHOULD I PROCEED?" — PROCEED. DO NOT WAIT FOR CONFIRMATION ON OBVIOUS NEXT STEPS.
IF BLOCKED, TRY AN ALTERNATIVE APPROACH. ONLY ASK WHEN TRULY AMBIGUOUS OR DESTRUCTIVE.
<!-- END AUTONOMY DIRECTIVE -->

# Batty — OMX Orchestrated Development

You are working on **Batty**, a hierarchical agent command system for software development, written in Rust.

## Project Context

Batty reads a kanban board (kanban-md), dispatches tasks to coding agents, supervises them through shim-owned PTYs, gates on tests, and merges results. It runs inside tmux with an architect→manager→engineer hierarchy.

Key docs: `planning/architecture.md`, `planning/roadmap.md`, `planning/dev-philosophy.md`, `CLAUDE.md`.

## Task Board — Your Work Source

**All tasks live on the kanban-md board.** Before starting any work, check the board:

```bash
# See all tasks
kanban-md list --dir .batty/team_config/board

# See todo tasks (your pickup queue)
kanban-md list --dir .batty/team_config/board --status todo

# See what's in progress
kanban-md list --dir .batty/team_config/board --status in-progress

# Read a specific task
kanban-md show <task-id> --dir .batty/team_config/board

# Claim a task
kanban-md pick --claim worker-N --move in-progress --dir .batty/team_config/board

# Mark done when complete
kanban-md move <task-id> done --dir .batty/team_config/board
```

**Priority order:** critical > high > medium > low. Always pick the highest-priority unblocked todo task.

**Focus tags:** Tasks tagged `orchestration-loop` are the current experiment priority. Tasks tagged `experiment` are research/learning tasks.

## Tech Stack

- **Language:** Rust (edition 2024, MSRV 1.85)
- **CLI framework:** clap 4 (derive)
- **Async runtime:** tokio
- **Terminal runtime:** tmux
- **Agent shim:** PTY-owning subprocess per agent (`src/shim/`)
- **Config:** YAML (`.batty/team_config/team.yaml`)
- **Board:** Markdown tasks with YAML frontmatter (kanban-md)
- **Logs:** JSON lines (`.batty/team_config/events.jsonl`)

## Project Structure

```
src/               # Rust source (~2500 tests)
  team/            # Core team modules (daemon, config, hierarchy, layout, etc.)
  shim/            # Agent shim runtime (PTY, state classifier, protocol)
  agent/           # Claude/Codex/Kiro adapters
  cli.rs           # Clap CLI
  tmux.rs          # Core tmux ops
  worktree.rs      # Git worktree lifecycle
docs/              # User documentation
planning/          # Architecture, roadmap, philosophy
.batty/            # Runtime state, config, board
```

## Key Source Modules

| Module | File | Responsibility |
|--------|------|----------------|
| Daemon | `src/team/daemon.rs` | Agent spawning, polling loop, state machine |
| Task loop | `src/team/task_loop.rs` | Auto-dispatch, test gating, merge queue |
| Config | `src/team/config.rs` | YAML parsing, validation |
| Completion | `src/team/completion.rs` | Structured completion packets |
| Board | `src/team/board.rs` | Kanban board operations |
| Workflow | `src/team/workflow.rs` | Task lifecycle state model |
| Nudge | `src/team/nudge.rs` | Dependency-aware nudges |
| Shim | `src/shim/` | PTY ownership, classifier, protocol |

## Development Rules

1. **Every change gets tests.** Add tests in `#[cfg(test)] mod tests` at the bottom of each file.
2. **Run `cargo test` before reporting done.** All tests must pass.
3. **Run `cargo fmt`** before committing.
4. **Keep it minimal.** Don't add features beyond the task scope.
5. **No premature abstraction.** Three similar lines is fine.
6. **Commit early and often.** `git add -A && git commit -m "<area>: <what changed>"`.
7. **Build + codesign after changes:** `cargo build --release && cp target/release/batty ~/.cargo/bin/batty && codesign --force --sign - ~/.cargo/bin/batty`

## Verification Protocol

Before marking any task done:

1. `cargo fmt` — code is formatted
2. `cargo test` — all tests pass (currently ~2500 tests)
3. `git diff --stat` — verify you have real changes, not just narration
4. `git add -A && git commit -m "<area>: <description>"` — committed
5. `git log --oneline -3` — verify commits exist

**Zero commits = not done. Zero test changes = not done.**

## Working on Branches

Create a branch per task to avoid conflicts with other workers:

```bash
git checkout -b worker-N/task-<id>
# ... do work ...
git add -A && git commit -m "feat: <description>"
```

## OMX Workflow Integration

When running under `$team` or `$ralph`:
- Pick tasks from the kanban-md board, not from free-form prompts
- Report progress by updating task status on the board
- Mark tasks done only after verification passes
- If blocked, move task back to todo and document the blocker

## clawhip Event Reporting

If clawhip is running, events are emitted automatically via OMX hooks. Key events:
- `session.started` — worker began
- `session.blocked` — worker hit a blocker
- `session.finished` — worker completed task
- `test-started` / `test-finished` / `test-failed` — test lifecycle
