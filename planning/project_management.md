# Batty: Project Management Strategy

Date: 2026-02-21
Updated: 2026-02-21

## Decision: Integrate kanban-md, Build on Top

Batty does not build its own project management system from scratch. We integrate [kanban-md](https://github.com/antopolskiy/kanban-md) — a Go CLI tool where each task is a `.md` file with YAML frontmatter — and layer Batty's execution capabilities on top.

This follows the composable tools philosophy: kanban-md owns task state, Batty owns execution.

Replace with a native `batty task` CLI only if kanban-md becomes a limiting factor.

## Why kanban-md

- Single static Go binary, no runtime deps.
- Each task is a `.md` file with YAML frontmatter in a `kanban/` directory — Git-friendly, human-readable.
- CLI-first: `create`, `move`, `list`, `pick` (atomic claim), `board` (TUI), `show`, `metrics`.
- Designed for multi-agent workflows: atomic `pick` for agent task claiming, `handoff`, compact output mode.
- Pre-built Claude Code skills.
- YAML frontmatter is easy to extend with Batty-specific fields without breaking kanban-md's parsing.
- MIT license.

## Alternatives Considered

| Tool | Why not (for now) |
|---|---|
| Backlog.md | Node.js dependency, heavier than needed |
| Build our own | Slower time-to-market, premature before we know real needs |
| markdown-kanban (VS Code extension) | VS Code only, no CLI, too limited |
| Taskell | Haskell, single-file format, no frontmatter metadata |

## The `batty work` Flow

kanban-md manages tasks. Batty executes them. The core command is `batty work <task-id>`.

```
kanban-md add "Fix auth bug"          # creates task #3 in kanban/
kanban-md list                        # see the board

batty work 3                          # Batty reads task #3 from kanban/
  -> reads task description + metadata from kanban/003.md
  -> creates git worktree (isolation)
  -> splits pane
  -> launches agent (claude/codex/aider) with task description
  -> user sees full interactive agent session in pane
  -> Batty supervises on top:
      - auto-answers routine prompts per policy
      - feeds test failures back if agent gets stuck
      - runs DoD gate when agent signals done
  -> tests pass
  -> auto-commit in worktree
  -> merge worktree back to main
  -> kanban-md move 3 done            # Batty updates task status
```

The user can interact with the agent at any time — it's a fully interactive terminal pane. Batty's supervision is a layer on top, not a replacement for the agent's native UX.

## Extending kanban-md Task Files

kanban-md uses YAML frontmatter. Batty can add its own fields without breaking kanban-md:

```yaml
---
id: 3
title: Fix auth bug
status: in-progress
priority: high
tags: [backend, auth]
# Batty extensions (ignored by kanban-md)
batty_agent: claude
batty_policy: act-with-approval
batty_dod: "pytest tests/test_auth.py"
batty_max_retries: 3
---

## Description
The login endpoint returns 500 when OAuth token is expired.
Fix the token refresh logic in auth.py.

## Acceptance Criteria
- [ ] Token refresh works for expired tokens
- [ ] Existing tests pass
- [ ] New test covers the expired token case
```

## Dogfooding: Building Batty with kanban-md

We use kanban-md to manage Batty's own development from day one. Every Batty feature is a kanban-md task. As Batty becomes capable of `batty work`, we start executing our own tasks through Batty — dogfooding the core workflow.

## Future: When to Build `batty task`

Build a native replacement only if:

- kanban-md's format becomes a bottleneck for Batty-specific features.
- We need tighter integration than file-level interop allows.
- The Go binary dependency becomes a deployment problem.

Until then, composability wins over ownership.
