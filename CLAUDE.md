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
src/               # Rust source
docs/              # User and reference documentation
assets/            # Static assets (images, demos)
scripts/           # Utility scripts
planning/          # Architecture, roadmap, philosophy docs
.agents/           # Codex agent rules/skills
.claude/           # Claude agent rules/skills
.batty/            # Batty runtime config, kanban boards, logs, worktrees
  kanban/          # Phase boards + docs-update board
    phase-1/       # DONE: Core Agent Runner
    phase-2/       # DONE: tmux-based Intelligent Supervisor
    phase-2.4/     # DONE: Supervision Harness Validation
    phase-2.5/     # DONE: Runtime Hardening + Dogfood
    phase-2.6/     # DONE: Backlog Rollover from 2.5
    phase-2.7/     # DONE: Minor Improvements
    docs-update/   # NEXT: Documentation Sync
    phase-3/       # DONE: 3A Sequencer + Human Review Gate
    phase-3b/      # DONE: 3B AI Director Review
    phase-4/       # DONE: Parallel DAG Scheduler, Merge Queue, Ship
```

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
term_size = "0.3"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
toml = "0.8"
regex = "1"
anyhow = "1"
thiserror = "2"
ctrlc = "3"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

## CLI Commands

- `batty work <phase>`: run a phase with supervision
- `batty attach <phase>`: attach to a running tmux session
- `batty resume <phase|session>`: resume supervision for an existing run
- `batty board <phase>`: open phase board in kanban-md TUI
- `batty list` (alias: `batty board-list`): list all boards with status and task counts
- `batty config [--json]`: show resolved configuration
- `batty install [--target both|claude|codex] [--dir PATH]`: install project assets
- `batty remove [--target both|claude|codex] [--dir PATH]`: remove installed project assets
- `batty merge <phase> <run>`: merge a worktree run back into main
- `batty work all`: run all phases in sequence
- `batty work <phase> --parallel N`: run with N parallel agents
