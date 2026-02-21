# Batty

**Use the best agents. Batty controls execution.**

Batty is a hierarchical agent command system for software development. It reads your project board, dispatches tasks to coding agents, supervises their work, gates on tests, and merges results — while you watch, intervene, or let it run.

## How It Works

```
You (human) ──→ see everything, intervene anytime
    │
    ▼
Director (batty work all) ──→ reads board, picks tasks, dispatches
    │
    ├── Supervisor #1 ──→ spawns agent, watches PTY, auto-answers, runs tests
    │       └── Claude Code ──→ writes code in worktree /task-3
    │
    ├── Supervisor #2 ──→ spawns agent, watches PTY, auto-answers, runs tests
    │       └── Codex CLI ──→ writes code in worktree /task-7
    │
    └── Supervisor #3 ──→ ...
            └── ...
    │
    ▼
kanban-md (Markdown files) ──→ single source of truth
```

Three layers: **Director** picks work. **Supervisors** control execution. **Executors** (BYO agents) write code. You sit above all three.

## Quick Start

Work on a single task:

```sh
batty work 3
```

This reads task #3 from your kanban board, creates a worktree, launches Claude Code with the task description, supervises the session (auto-answers routine prompts, runs tests on completion), and merges the result to main.

You see the full interactive agent session. Type into it anytime. Batty handles the boring parts.

Work through the entire board:

```sh
batty work all
```

Batty picks tasks by priority, respects dependencies, and works through them sequentially. Point it at your project and walk away — or watch and steer.

Run tasks in parallel:

```sh
batty work all --parallel 3
```

Three agents, three worktrees, three panes. Batty manages the merge queue so they land cleanly on main.

## Core Features

### Task-Driven Execution

Tasks live as Markdown files managed by [kanban-md](https://github.com/antopolskiy/kanban-md). Batty reads them, executes them, and updates their status. The kanban board is your command and control center.

```sh
kanban-md create "Fix auth token refresh" --priority high
batty work 1    # read task, create worktree, launch agent, supervise, test, merge
```

### Interactive Supervision

Agents run in interactive PTY sessions — not hidden behind a progress bar. You see exactly what Claude or Codex is doing. Batty supervises on top:

- Auto-answers routine prompts ("Continue? [y/n]" → yes)
- Auto-approves safe tool calls per policy
- Passes real questions through to you
- Detects stuck states and retries

### Policy Tiers

Every automated action flows through explicit policy:

| Tier | Behavior |
|---|---|
| `observe` | Log only — Batty watches, you drive |
| `suggest` | Show suggestion, you confirm |
| `act` | Auto-respond to routine prompts, escalate unknowns |

Policies are defined per-project in `.batty/config.toml`:

```toml
[defaults]
agent = "claude"
policy = "act"
dod = "cargo test --workspace"
max_retries = 3

[policy.auto_answer]
"Continue? [y/n]" = "y"
```

### Test-Gated Completion

Tasks aren't "done" until tests pass. When an agent signals completion, Batty runs your definition-of-done command. Tests pass → commit → rebase on main → merge. Tests fail → feed failure output back to the agent for retry.

Completion is measured, not guessed.

### Audit Trail

Every automated decision — prompt answers, test runs, commits, merges — is logged to a structured JSON timeline. Inspect any autonomous decision after the run.

### Agent-Agnostic

Batty doesn't ship its own AI. Swap Claude Code for Codex CLI for Aider without changing your workflow. Agents are first-class, never locked in.

### Git Worktree Isolation

Every task runs in its own worktree. No branch conflicts. No dirty working directories. Batty creates the worktree, the agent works in it, and Batty merges it back cleanly.

## Architecture

Batty is built progressively — the CLI agent runner works today in any terminal. The GUI terminal shell comes later.

```
┌─────────────────────────────────────────┐
│  Batty Terminal (Phase 4+)              │
│  Tauri v2 + Solid.js + xterm.js        │
├─────────────────────────────────────────┤
│  Rust Core (Phase 1)                    │
│  portable-pty · tokio · clap            │
│  Agent adapter layer                    │
│  PTY supervision + prompt detection     │
│  Policy engine                          │
│  Test gate runner                       │
│  Worktree lifecycle manager             │
│  Execution logger                       │
├─────────────────────────────────────────┤
│  kanban-md (external)                   │
│  Markdown task files with YAML          │
│  frontmatter — the command center       │
└─────────────────────────────────────────┘
```

## Philosophy

- **Compose, don't monolith.** Integrate best-in-class CLI tools. Build only the orchestration layer.
- **Markdown as backend.** All state is human-readable, agent-readable, git-versioned Markdown files.
- **Agents are processes, not features.** Real interactive PTY sessions. Transparency is non-negotiable.
- **Earn autonomy progressively.** Start supervised. Prove reliability. Increase automation.
- **CLI-first, GUI-optional.** Works in any terminal today. Rich panes come later.

See `.planning/dev-philosophy.md` for the full development philosophy.

## Project Status

Building in public. Currently in Phase 1 — the core agent runner (`batty work`).

See `.planning/roadmap.md` for the full roadmap and `.planning/architecture.md` for the hierarchical agent command architecture.

## Links

- Website: [batty.sh](https://batty.sh)
- GitHub: [github.com/battysh/batty](https://github.com/battysh/batty)
- Discord: [discord.gg/battyterm](https://discord.gg/battyterm)
- Twitter: [@battyterm](https://twitter.com/battyterm)
- Bluesky: [@battyterm.bsky.social](https://bsky.app/profile/battyterm.bsky.social)

## License

MIT
