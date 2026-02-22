# Batty: Competitive Landscape

Date: 2026-02-21

## Where Batty Sits

Batty is not a terminal emulator. It's an agent orchestration system that runs on tmux. The competitive landscape includes agent-native terminals, multiplexers, and agent workflow tools.

## Similar Projects

### Agent-Native Terminals

**Wave Terminal** (17k stars) — AI-native terminal with workspace/block UX. Integrated AI experience. Gap: no deterministic agent governance, DoD gating, or policy engine.

**Superset Terminal** (2k stars) — Multi-agent terminal for coordinating parallel coding agents with worktree flows. TypeScript. Closest to Batty's vision but weaker on deterministic orchestration and supervision.

**Warp** — Rust + GPU rendering, productized modern terminal UX. Different category: terminal product, not agent control plane.

### Multiplexers (Our Runtime Layer)

**tmux** (42k stars) — Our chosen runtime. Provides sessions, panes, pipe-pane, send-keys, status bar, control mode, hooks. Battle-tested, universally available.

**Zellij** (29k stars) — Modern multiplexer with WASM plugin model. CLI actions. Good plugin architecture but we chose tmux for ubiquity and stability.

**WezTerm** (24k stars) — Programmable multiplexer with Lua API. Strong pane orchestration. More terminal-emulator-focused than what we need.

### Terminal Emulators

**Ghostty** (44k stars) — High-performance native terminal (Zig). No plugin direction. Useful as performance benchmark.

**kitty** (31k stars) — Remote control mode and command API. Confirms external process control over terminal state is practical.

## Gap Analysis

No project combines:
- Agent supervision with policy-gated autonomy
- tmux-based execution with real interactive sessions
- Hierarchical command structure (director/supervisor/executor)
- Objective completion gates (test suites, not vibes)
- Full audit trail of automated decisions
- Phase-based workflow with kanban boards as command interface

## Architecture Decision

We chose to build on tmux rather than build a terminal emulator because:
- tmux gives us panes, output capture, input injection, session persistence, and hooks for free
- Building a terminal emulator is months of work that doesn't validate the product thesis
- Our differentiation is the workflow model, not the terminal
- Power users already use tmux — we meet them where they are

## Go/No-Go

Go. The concept addresses a real gap. Success depends on the quality of the agent control plane and safe automation model, not on renderer novelty.
