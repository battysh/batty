# Batty — Agent Instructions

## What Is This Project

Batty is a hierarchical agent command system for software development. It runs a small team of agents inside tmux, routes messages between roles, tracks work on a shared Markdown board, and keeps the whole run visible in the terminal.

See `planning/architecture.md` for the system design and `planning/dev-philosophy.md` for development principles.

## Tech Stack

- **Language:** Rust
- **CLI framework:** clap
- **Terminal runtime:** tmux (pane layout, display-only surfaces when shim mode is active, session persistence)
- **Agent shim:** PTY-owning subprocess per agent, screen classification, structured socketpair protocol (`src/shim/`)
- **PTY support:** portable-pty (used by shim runtime for agent PTY management)
- **Async runtime:** tokio
- **Config format:** YAML (`.batty/team_config/team.yaml`)
- **Board format:** Markdown tasks with YAML frontmatter
- **Execution logs:** JSON lines

## Project Structure

```text
src/               # Rust source
  shim/            # Agent shim runtime (PTY, state classifier, protocol, chat)
docs/              # User and reference documentation
assets/            # Static assets (images, demos)
scripts/           # Utility scripts
planning/          # Architecture, roadmap, philosophy docs
.agents/           # Codex agent rules/skills
.claude/           # Claude agent rules/skills
.batty/
  team_config/
    team.yaml      # Team topology, routing, layout, timing
    *.md           # Role prompts used to launch architect/manager/engineer agents
    board/         # Shared task board and task files
    events.jsonl   # Team event log
```

## Development Principles

- **Compose, don't monolith.** Use existing CLI tools where possible.
- **Markdown as backend.** Keep state human-readable and git-versioned.
- **Minimal code.** Build the smallest thing that works.
- **No premature abstraction.** Prefer obvious code over clever indirection.
- **Test what matters.** Focus on tmux supervision, message routing, board state, and prompt handling.
- **Extensive unit tests.** Every module gets `#[cfg(test)]` coverage for happy paths, edge cases, and failures. Run `cargo test` before committing. If a task adds code, it adds tests.

## Test Categories

Tests are split into **unit** and **integration**:

- **Unit tests** (`cargo test`): ~2,509 tests that run without tmux. Safe for CI without a tmux server.
- **Integration tests** (`cargo test --features integration`): 56 tmux-dependent tests gated behind the `integration` Cargo feature. These require a running tmux server.

## Monitoring

- **Telemetry:** SQLite database at `.batty/telemetry.db` — events, agent metrics, task metrics, session summaries
- **Grafana:** `batty grafana setup` installs Grafana + SQLite plugin, provisions datasource and imports the bundled 34-panel dashboard
- **Dashboard panels:** stat counters, health gauges, hourly time series (with gap-filling for outages), pie charts, tables

Integration tests use `#[cfg_attr(not(feature = "integration"), ignore)]`. Without the feature flag, they are automatically skipped.

## Key Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
clap_complete = "4"
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
ureq = { version = "2", features = ["json"] }
```

## Building & Installing

After building, always re-sign the binary before running from `~/.cargo/bin`:

```bash
cargo build --release
cp target/release/batty ~/.cargo/bin/batty
codesign --force --sign - ~/.cargo/bin/batty
```

macOS AppleSystemPolicy (ASP) kills unsigned or stale-signed binaries. Copying over an existing binary invalidates the cached ad-hoc signature, causing ASP to SIGKILL it. `codesign --force --sign -` re-signs with a fresh ad-hoc signature.

## CLI Commands

- `batty init`: bootstrap Batty assets for a repo
- `batty start`: start the team runtime
- `batty stop`: stop the active team runtime
- `batty attach`: attach to a running tmux session
- `batty status`: show team/runtime status
- `batty send`: send a message to another role
- `batty assign`: assign work to an engineer
- `batty inbox`: show queued messages for a role
- `batty read`: read a message or inbox entry
- `batty ack`: acknowledge a message
- `batty board`: open the shared board
- `batty merge`: merge completed work back to the main branch
- `batty validate`: validate team configuration and runtime prerequisites
- `batty config`: show resolved configuration
- `batty telegram`: manage Telegram integration and setup
- `batty completions`: generate shell completions
