# Batty — Agent Instructions

## What Is This Project

Batty is a hierarchical agent command system for software development. It reads a kanban board, dispatches tasks to coding agents, supervises their work, gates on tests, and merges results.

See `.planning/architecture.md` for the full architecture and `.planning/dev-philosophy.md` for development principles.

## Tech Stack

- **Language:** Rust
- **CLI framework:** clap
- **PTY management:** portable-pty
- **Async runtime:** tokio
- **Config format:** TOML (.batty/config.toml)
- **Task management:** kanban-md (external CLI tool, Markdown files with YAML frontmatter)
- **Execution logs:** JSON lines

## Project Structure

```
src/              # Rust source (to be created)
kanban/           # Kanban boards (one per phase)
  phase-1/        # Current sprint: agent runner (batty work)
  phase-2/        # Next: board runner (batty work all)
  phase-3/        # Policy hardening
  phase-4/        # Tauri terminal
  phase-5/        # Pane orchestration
  phase-6/        # Parallel execution
  phase-7/        # Polish
.planning/        # Architecture, roadmap, philosophy docs
.batty/           # Batty config (to be created)
```

## How To Work On Tasks

### The current sprint board is at `kanban/phase-1/`

All kanban-md commands must use `--dir kanban/phase-1` to target the right board.

### Workflow

1. Check the board: `kanban-md board --compact --dir kanban/phase-1`
2. Generate agent name: `kanban-md agent-name` (remember it for the session)
3. Pick a task: `kanban-md pick --claim <agent-name> --status backlog --move in-progress --dir kanban/phase-1`
4. Read the task: `kanban-md show <ID> --dir kanban/phase-1`
5. Create a branch: `git checkout -b task/<ID>-<kebab-description>`
6. Implement and test the work
7. Commit with a detailed message (see Commit Messages below)
8. Merge to main: `git checkout main && git merge task/<ID>-<kebab-description>`
9. Write a statement of work on the task (see Statement of Work below)
10. Mark done: `kanban-md move <ID> done --dir kanban/phase-1`
11. Pick next task and repeat

When multiple agents work in parallel (future), use git worktrees instead of branches.

### Commit Messages

Write detailed commit messages that serve as a record of what changed and why:

```
task/<ID>: <short summary>

What: <what was implemented/changed>
Why: <why this approach was chosen>
How: <key implementation details — files created, patterns used, decisions made>

Files: <list of key files added or modified>
```

Keep the first line under 72 characters. The body should give enough context that someone reading `git log` understands the full scope of the change without reading the diff.

### Statement of Work

After completing a task, update the task file with a statement of work. This is the project's progress documentation — future agents and humans read it to understand what was done.

Use `kanban-md edit <ID> -a "note" -t --dir kanban/phase-1` to append a timestamped note, or edit the task file directly to add a `## Statement of Work` section:

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
- One task at a time.
- Work on a feature branch, commit there, merge to main when done.
- Run kanban-md commands from the project root with `--dir kanban/phase-1`.
- Leave progress notes: `kanban-md edit <ID> -a "note" -t --claim <agent> --dir kanban/phase-1`
- If blocked, hand off: `kanban-md handoff <ID> --claim <agent> --note "reason" -t --release --dir kanban/phase-1`

## Development Principles

- **Compose, don't monolith.** Use existing CLI tools where possible.
- **Markdown as backend.** All state in human-readable, git-versioned files.
- **Minimal code.** Don't over-engineer. Build the smallest thing that works.
- **No premature abstraction.** Three similar lines > one clever abstraction.
- **Test what matters.** Focus on the PTY supervision and prompt detection — that's the hard part.

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
