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
| `portable-pty` | PTY management (Phase 1 fallback, non-tmux) |
| `serde` + `serde_yaml` | YAML frontmatter parsing |
| `toml` | Config file parsing |
| `regex` | Event extraction, prompt pattern matching |
| `notify` | File watching (pipe-pane output log) |

## What We Don't Build

- No terminal emulator (tmux handles it)
- No custom UI framework (tmux panes + status bar)
- No task management system (kanban-md handles it)
- No AI model (BYO agents)
- No plugin runtime (not needed yet)
