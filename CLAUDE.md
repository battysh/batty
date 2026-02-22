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
planning/        # Architecture, roadmap, philosophy docs
.batty/           # Batty runtime config, kanban boards, logs, worktrees
  kanban/         # Kanban boards (one per phase)
    phase-1/      # DONE: core agent runner
    phase-2/      # tmux-based intelligent supervisor
    phase-2.5/    # Adjustments and ideas (parking lot)
    phase-3/      # Director & review gate
    phase-4/      # Parallel execution
    phase-5/      # Polish + ship
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
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
toml = "0.8"
regex = "1"
```
