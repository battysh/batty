# Batty: Origin and Core Insight

Date: 2026-02-21

## The Problem

Modern development has moved to the terminal. Coding agents (Claude Code, Codex, Aider) run as interactive CLI processes. But there's no good way to:

- **Supervise** an agent while it works — detect when it's stuck, answer its questions, verify its output.
- **Orchestrate** multiple agents working on related tasks with isolation and merge control.
- **Enforce policy** — what the agent can do autonomously vs. what needs human approval.
- **Gate completion** — verify work against objective criteria (tests, builds) before accepting it.

The human either babysits the agent manually or lets it run unsupervised and hopes for the best.

## The Insight

The gap isn't a better terminal or a better AI. It's the **control plane between the human and the agent**. A thin orchestration layer that:

1. Launches agents in isolated environments (git worktrees).
2. Monitors their output in real time (tmux pipe-pane).
3. Handles routine prompts automatically (pattern matching + supervisor agent).
4. Gates completion on objective criteria (test suites, build checks).
5. Provides full audit trail of every automated decision.

The agent does the work. The human sets strategy. Batty controls the execution.

## Evolution

The original idea started broader — a "terminal superset" with rich rendering, plugin ecosystem, and custom UI. Through iteration, we converged on a sharper thesis:

- **tmux is the runtime.** It provides panes, sessions, output capture, input injection, and persistence for free. We build on top of it, not around it.
- **Batty is a workflow model**, not just a tool. Phases as units of work, kanban boards as the command interface, three-layer hierarchy (director/supervisor/executor), progressive autonomy.
- **The human is a product director**, not a code typist. Think → spec → launch → steer → review → repeat.

See `roadmap.md` for the current plan, `architecture.md` for the system design.
