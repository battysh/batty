# Architecture

Batty coordinates three roles in one supervised tmux session. This page covers the runtime data flow, prompt handling tiers, and module layout.

## The Three Roles

```
┌──────────────────────────────────────────────┐
│  Director                                    │
│  Reviews completed phases -> merge / rework  │
├──────────────────────────────────────────────┤
│  Supervisor                                  │
│  Watches executor output. Answers questions. │
├──────────────────────────────────────────────┤
│  Executor (Claude Code / Codex / Aider)      │
│  Picks tasks. Writes code. Runs tests.       │
├──────────────────────────────────────────────┤
│  tmux + kanban-md                            │
│  Runtime panes + task state. All files.      │
└──────────────────────────────────────────────┘
```

- **Director** -- Phase-level strategy. Reviews completed work and decides: merge, request rework, or escalate to a human. (Phase 3B: AI director; currently human.)
- **Supervisor** -- Runtime tactics. Watches executor output in real time, answers prompts the executor can't handle, composes context from project files.
- **Executor** -- The labor. A coding agent (Claude Code, Codex, Aider) running inside the tmux executor pane. Picks tasks from the board, implements, tests, commits.

## Runtime Data Flow

```
executor pane output
  -> tmux pipe-pane stream
  -> events::EventBuffer (extraction + buffering)
  -> detector::PromptDetector
      -> Tier 1: policy auto-answer (regex map, instant)
      -> Tier 2: supervisor request (agent call, 5-60s)
  -> tmux send-keys injection into executor pane
  -> orchestrator log + status bar updates
```

`batty work <phase>` drives this loop while evaluating task/board progress and completion gates in parallel.

## Two-Tier Prompt Handling

### Tier 1: Instant Policy Answers

- **Source:** Regex-response map from `.batty/config.toml` (defaults + user overrides)
- **Trigger:** Detector matches a known prompt pattern
- **Action:** Answer injected immediately via `tmux send-keys`
- **Example:** `Continue? [y/n]` -> `y`
- **Coverage:** ~70-80% of all agent prompts

### Tier 2: Supervisor Escalation

- **Source:** Supervisor agent (Claude Code or Codex) with composed project context
- **Trigger:** Unknown prompt, ambiguous question, or silence fallback
- **Action:** Batty asks the supervisor, validates the response, injects the answer
- **Example:** "Should I use async or sync for this handler?" -> supervisor analyzes codebase and decides
- **Context snapshots:** Written to `.batty/logs/<run>/tier2-context-<n>.md` for debugging

### Unknown Fallback

When output goes silent and no regex matches, Batty asks the supervisor anyway. This catches cases where the agent is waiting for input but the prompt doesn't match known patterns.

## Module Map

| Module                | Responsibility                                                                |
| --------------------- | ----------------------------------------------------------------------------- |
| `main.rs`             | CLI entrypoint and command dispatch                                           |
| `cli.rs`              | clap command/option definitions                                               |
| `paths.rs`            | `.batty/` path resolution                                                     |
| `work.rs`             | Phase run pipeline: context composition, launch, resume hooks                 |
| `orchestrator.rs`     | Core supervision loop: event ingestion, detector, policy/tier actions, status |
| `tmux.rs`             | tmux command wrapper (session, window, pane, status operations)               |
| `events.rs`           | Pipe-pane event parsing, buffering, structured extraction                     |
| `detector.rs`         | Prompt detection state machine (silence windows, pattern matching, fallback)  |
| `tier2.rs`            | Supervisor invocation, context snapshots, answer extraction                   |
| `completion.rs`       | Phase completion contract and signal detection                                |
| `worktree.rs`         | Run worktree create/reuse/cleanup, branch naming                              |
| `install.rs`          | `batty install` / `batty remove` asset management                             |
| `agent/mod.rs`        | AgentAdapter trait + adapter registry                                         |
| `agent/claude.rs`     | Claude Code adapter                                                           |
| `agent/codex.rs`      | Codex adapter                                                                 |
| `config/mod.rs`       | Config schema, loading, defaults, validation                                  |
| `policy/mod.rs`       | Policy tiers (observe / suggest / act / fully-auto)                           |
| `prompt/mod.rs`       | Prompt pattern set and matching                                               |
| `log/mod.rs`          | JSONL execution logs                                                          |
| `task/mod.rs`         | kanban-md task parsing and selection                                          |
| `supervisor/mod.rs`   | Supervisor runtime abstractions                                               |
| `dod/mod.rs`          | Definition-of-done gates                                                      |
| `sequencer.rs`        | Multi-phase sequencing for `batty work all`                                   |
| `review.rs`           | AI director review decisions (merge / rework / escalate)                      |
| `dag.rs`              | Task dependency DAG construction and cycle detection                          |
| `scheduler.rs`        | Parallel DAG-aware task scheduler                                             |
| `merge_queue.rs`      | Serialized merge queue for parallel worktree results                          |
| `shell_completion.rs` | Shell completion script generation                                            |
| `bin/docsgen.rs`      | Documentation generator                                                       |

## tmux Compatibility

| tmux version | Status                                   |
| ------------ | ---------------------------------------- |
| >= 3.2       | Full feature path (recommended)          |
| 3.1.x        | Supported with fallbacks                 |
| < 3.1        | Not supported (fails fast with guidance) |

Batty probes tmux capabilities on startup and logs: version, `pipe-pane` support, `pipe-pane -o` support, status style options, and split mode.

## Design References

- [Architecture (planning)](https://github.com/battysh/batty/blob/main/planning/architecture.md)
- [Development Philosophy](https://github.com/battysh/batty/blob/main/planning/dev-philosophy.md)
- [Execution Loop](https://github.com/battysh/batty/blob/main/planning/execution-loop.md)
