# Phase 1: Core Agent Runner

**Status:** Done
**Board:** `kanban/phase-1/`

## Summary

Built the Rust CLI foundation: `batty work <phase>` reads a kanban board, spawns an agent in a PTY, supervises the session with prompt detection, policy engine, test gates, and execution logging.

98 tests passing. All 11 tasks complete.

## What Was Built

- Rust project with clap CLI (`src/main.rs`, `src/cli.rs`)
- kanban-md task file reader (`src/task/`)
- Agent adapter trait + Claude Code adapter (`src/agent/`)
- PTY supervision with bidirectional I/O (`src/supervisor/`)
- Prompt detection via regex patterns (`src/prompt/`, `src/detector.rs`)
- Simple policy engine with observe/suggest/act/auto tiers (`src/policy/`)
- Test-gated completion / definition of done (`src/dod/`)
- Structured execution log in JSON lines (`src/log/`, `src/events.rs`)
- `batty work <id>` command to run a single task (`src/work.rs`)

## Key Patterns

- Each module has `#[cfg(test)]` unit tests
- Agent adapters implement `AgentAdapter` trait
- Policy tiers: `observe`, `suggest`, `act-with-approval`, `fully-auto`
- Prompt detection: regex-based pattern matching on PTY output
- Config: TOML in `.batty/config.toml`
