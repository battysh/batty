# Batty Tech Stack

## Principle

Compose existing tools. Build only the orchestration layer.

## Stack

```
┌─────────────────────────────────────────┐
│  tmux                                   │
│  Terminal runtime. Panes, pipe-pane,    │
│  send-keys, status bar, persistence.   │
├─────────────────────────────────────────┤
│  Batty (Rust)                           │
│  clap (CLI) · tokio (async)            │
│  Event extraction · Prompt detection    │
│  Policy engine · Test gates · Logging   │
├─────────────────────────────────────────┤
│  kanban-md (Go CLI, external)           │
│  Markdown task boards with YAML         │
│  frontmatter. Command center.           │
└─────────────────────────────────────────┘
```

## Why These Choices

**Rust** — Fast, safe, great for state machines and regex. Proven in terminal tools (Alacritty, Zellij, Warp).

**tmux** — Gives us output capture, input injection, panes, status bar, session persistence, resize handling — all battle-tested. No custom terminal code.

**kanban-md** — Markdown-based task management. Composable CLI tool. Zero dev effort for us.

**TOML** — Config format. Rust ecosystem standard, human-friendly.

**JSON lines** — Execution log format. Machine-readable, appendable, queryable.

## Key Rust Crates

| Crate | Purpose |
|---|---|
| `clap` | CLI argument parsing |
| `tokio` | Async runtime |
| `portable-pty` | PTY management fallback for non-tmux environments |
| `term_size` | Terminal dimension detection for layout/status rendering |
| `serde` | Data model serialization/deserialization |
| `serde_yaml` | YAML frontmatter parsing for kanban task files |
| `serde_json` | JSON output for logs and machine-readable config output |
| `toml` | Config file parsing |
| `regex` | Event extraction, prompt pattern matching |
| `anyhow` | Application-level error propagation with context |
| `thiserror` | Structured error definitions for internal error types |
| `ctrlc` | Signal handling for graceful shutdown/interrupt behavior |
| `tracing` | Structured instrumentation and runtime logs |
| `tracing-subscriber` | Log filtering/formatting and env-filter support |
| `tempfile` (dev-dependency) | Temp directories/files for unit and integration tests |

## What We Don't Build

- No terminal emulator (tmux handles it)
- No custom UI framework (tmux panes + status bar)
- No task management system (kanban-md handles it)
- No AI model (BYO agents)
- No plugin runtime (not needed yet)
