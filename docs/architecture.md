# Architecture

Batty coordinates three roles in one supervised runtime:

- Director: phase-level review and merge/rework decisions
- Supervisor: runtime watcher and prompt responder
- Executor: coding agent operating in the tmux executor pane

## Runtime Data Flow

```text
executor pane output
  -> tmux pipe-pane stream
  -> events::EventBuffer / extraction
  -> detector::PromptDetector
      -> Tier 1 policy auto-answer (regex map)
      -> Tier 2 supervisor request (agent call)
  -> tmux send-keys injection into executor pane
  -> orchestrator log + status updates
```

`batty work <phase>` drives this loop while task/board progress and completion gates are evaluated in parallel.

## Two-Tier Prompt Handling

### Tier 1: Immediate Policy Answers

- Source: configured regex-response map (`.batty/config.toml`, defaults + overrides)
- Trigger: detector sees a known prompt match
- Action: answer is injected immediately via `tmux send-keys`
- Use case: deterministic confirmations like `Continue? [y/n]`

### Tier 2: Supervisor Escalation

- Source: supervisor agent process (Claude/Codex) with composed context
- Trigger: unknown prompt, ambiguity, or silence fallback
- Action: Batty asks supervisor, validates response, injects result
- Use case: contextual questions requiring project awareness

## Module Map (`src/`)

| Module | Responsibility |
|---|---|
| `main.rs` | CLI entrypoint; command dispatch for `work/attach/resume/config/install/remove/board` |
| `cli.rs` | clap command/option definitions and CLI parser tests |
| `paths.rs` | Canonical `.batty/` path resolution helpers |
| `work.rs` | Phase run pipeline: context composition, launch orchestration, resume hooks |
| `orchestrator.rs` | Core supervision loop (event ingestion, detector loop, policy/tier actions, status updates) |
| `tmux.rs` | tmux command wrapper for session/window/pane/status operations |
| `events.rs` | Pipe-pane event parsing/buffering and structured event extraction |
| `detector.rs` | Prompt detection state machine (silence windows, pattern matching, fallback triggers) |
| `tier2.rs` | Supervisor invocation, context snapshotting, answer extraction, escalation control |
| `completion.rs` | Phase completion contract and completion signal detection |
| `worktree.rs` | Run worktree create/reuse/cleanup and branch naming conventions |
| `install.rs` | `batty install` / `batty remove` asset management and prerequisite checks |
| `agent/mod.rs` | AgentAdapter trait + adapter registry/selection |
| `agent/claude.rs` | Claude adapter process command composition |
| `agent/codex.rs` | Codex adapter process command composition |
| `config/mod.rs` | `.batty/config.toml` schema, loading, defaults, validation |
| `policy/mod.rs` | Policy tiers and decision model (observe/suggest/act/auto) |
| `prompt/mod.rs` | Prompt pattern set and matching utilities |
| `log/mod.rs` | JSONL execution logs and structured lifecycle event writes |
| `task/mod.rs` | kanban-md task file/board parsing and task selection helpers |
| `supervisor/mod.rs` | Supervisor runtime abstractions and execution helpers |
| `dod/mod.rs` | Definition-of-done gates (tests/checks before task/phase closure) |
| `bin/docsgen.rs` | Documentation generator used by `scripts/generate-docs.sh` |

## Reference Docs

- Project architecture: `planning/architecture.md`
- Development philosophy: `planning/dev-philosophy.md`
