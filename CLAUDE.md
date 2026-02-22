# Batty — Agent Instructions

## What Is This Project

Batty is a hierarchical agent command system for software development. It reads a kanban board, dispatches tasks to coding agents, supervises their work, gates on tests, and merges results.

See `planning/architecture.md` for the full architecture and `planning/dev-philosophy.md` for development principles.

## Tech Stack

- **Language:** Rust
- **CLI framework:** clap
- **Terminal runtime:** tmux (output capture, input injection, status bar, panes, session persistence)
- **PTY management:** portable-pty (Phase 1 fallback for non-tmux environments)
- **Async runtime:** tokio
- **Config format:** TOML (.batty/config.toml)
- **Task management:** kanban-md (external CLI tool, Markdown files with YAML frontmatter)
- **Execution logs:** JSON lines

## Project Structure

```
src/              # Rust source
kanban/           # Kanban boards (one per phase)
  phase-1/        # DONE: core agent runner
  phase-2/        # tmux-based intelligent supervisor
  phase-2.5/      # Adjustments and ideas (parking lot)
  phase-3/        # Director & review gate
  phase-4/        # Parallel execution
  phase-5/        # Polish + ship
planning/        # Architecture, roadmap, philosophy docs
.batty/           # Batty config (to be created)
```

## How To Work On a Phase

### Execution Model: Phase as Unit of Work

The unit of supervised work is a **whole phase**, not an individual task. You work through the phase board from start to finish in a single session. Tasks are your checklist, not your branching strategy.

This means:
- **No per-task branches.** Work on `main` (or a single phase branch if needed).
- **Commit at natural checkpoints** — after completing a task or a logical group of tasks. Don't wait until the entire phase is done if there's meaningful progress to save.
- **Manage the board as you go.** Claim tasks, move them through statuses, write statements of work.
- **The session is the unit.** One agent, one phase, start to finish.

### Workflow

The phase to work on will be specified in the prompt (e.g., `kanban/phase-2/`). All kanban-md commands must use `--dir kanban/<phase>/` to target the correct board.

1. Check the board: `kanban-md board --compact --dir kanban/<phase>`
2. Generate agent name: `kanban-md agent-name` (remember it for the session)
3. Review all tasks to understand the full phase scope
4. Pick the next unblocked task: `kanban-md pick --claim <agent-name> --status backlog --move in-progress --dir kanban/<phase>`
5. Read the task: `kanban-md show <ID> --dir kanban/<phase>`
6. Implement and test the work
7. Write a statement of work on the task (see Statement of Work below)
8. Mark done: `kanban-md move <ID> done --dir kanban/<phase>`
9. Commit with a detailed message (see Commit Messages below)
10. Pick next task and continue until the phase is complete

### Commit Messages

Commit at natural checkpoints — after completing a task or a coherent group of changes. Write detailed commit messages that serve as a record of what changed and why:

```
phase-<N>/<task-IDs>: <short summary>

What: <what was implemented/changed>
Why: <why this approach was chosen>
How: <key implementation details — files created, patterns used, decisions made>

Tasks completed: <list of task IDs and titles>
Files: <list of key files added or modified>
```

Keep the first line under 72 characters. The body should give enough context that someone reading `git log` understands the full scope of the change without reading the diff.

### Statement of Work

After completing each task, update the task file with a statement of work. This is the project's progress documentation — future agents and humans read it to understand what was done.

Use `kanban-md edit <ID> -a "note" -t --dir kanban/<phase>` to append a timestamped note, or edit the task file directly to add a `## Statement of Work` section:

```markdown
## Statement of Work

- **What was done:** Brief description of the deliverable
- **Files created:** List of new files with one-line purpose each
- **Files modified:** List of changed files with what changed
- **Key decisions:** Any design choices or trade-offs made
- **How to verify:** Command to run or thing to check that proves it works
- **Open issues:** Anything deferred, known limitations, or follow-up needed
```

This is not optional. Every completed task must have a statement of work before being marked done.

### Rules

- Always claim before starting work.
- Work directly on `main` — no per-task feature branches.
- Commit after each completed task or logical group of tasks. Don't accumulate too much uncommitted work.
- Run `cargo test` before every commit — all tests must pass.
- Run kanban-md commands from the project root with `--dir kanban/<phase>`.
- Leave progress notes: `kanban-md edit <ID> -a "note" -t --claim <agent> --dir kanban/<phase>`
- If blocked, hand off: `kanban-md handoff <ID> --claim <agent> --note "reason" -t --release --dir kanban/<phase>`

## Development Principles

- **Compose, don't monolith.** Use existing CLI tools where possible.
- **Markdown as backend.** All state in human-readable, git-versioned files.
- **Minimal code.** Don't over-engineer. Build the smallest thing that works.
- **No premature abstraction.** Three similar lines > one clever abstraction.
- **Test what matters.** Focus on the PTY supervision and prompt detection — that's the hard part.
- **Extensive unit tests.** Every module must have unit tests. Test happy paths, edge cases, and error conditions. Use `#[cfg(test)]` modules in each source file. Run `cargo test` before committing — all tests must pass. If a task adds code, it adds tests. No exceptions.

## Key Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
portable-pty = "0.8"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
toml = "0.8"
regex = "1"
```
