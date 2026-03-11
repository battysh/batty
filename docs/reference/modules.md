# Module Reference

Contributor-facing map of Batty source modules.

## Module Index

| Path | Purpose |
| --- | --- |
| `src/main.rs` | CLI entrypoint, project-root resolution, and top-level command dispatch. |
| `src/cli.rs` | clap command and flag definitions for the team-mode CLI. |
| `src/team/mod.rs` | Team lifecycle: init, start, stop, attach, validate, send, merge. |
| `src/team/config.rs` | `.batty/team_config/team.yaml` parsing, defaults, and validation. |
| `src/team/hierarchy.rs` | Role expansion, instance naming, and reporting relationships. |
| `src/team/layout.rs` | tmux pane layout builder for team zones and manager/engineer groupings. |
| `src/team/daemon.rs` | Background runtime loop: spawn agents, poll state, route messages. |
| `src/team/inbox.rs` | Maildir inbox storage for inter-role messages. |
| `src/team/message.rs` | Message types and prompt composition for pane injection. |
| `src/team/comms.rs` | Outbound channel abstraction for user roles, including Telegram delivery. |
| `src/team/telegram.rs` | Telegram Bot API client and setup wizard behind `batty telegram`. |
| `src/team/standup.rs` | Standup report generation from current member state. |
| `src/team/board.rs` | Board rotation and archive helpers for the shared kanban board. |
| `src/team/task_loop.rs` | Task claiming, refresh, tests, and merge handoff logic for engineers. |
| `src/team/events.rs` | Structured team runtime event sink. |
| `src/team/watcher.rs` | tmux-pane watcher and idle/completion detection. |
| `src/team/templates/` | Built-in team templates and role prompt scaffolds. |
| `src/tmux.rs` | tmux command wrapper for sessions, panes, layout, and capability probes. |
| `src/agent/mod.rs` | `AgentAdapter` trait, registry, and shared spawn contract. |
| `src/agent/claude.rs` | Claude Code adapter implementation. |
| `src/agent/codex.rs` | Codex CLI adapter implementation. |
| `src/prompt/mod.rs` | Prompt detection patterns and ANSI-safe matching helpers. |
| `src/worktree.rs` | Git worktree creation, reuse, sync, and cleanup helpers. |
| `src/task/mod.rs` | Task parsing utilities for kanban-backed work items. |
| `src/events.rs` | Event extraction from captured agent output. |
| `src/log/mod.rs` | Structured JSONL logging primitives. |
| `src/paths.rs` | Shared filesystem path helpers under `.batty/`. |
| `src/config/mod.rs` | Optional `.batty/config.toml` runtime defaults loader. |
| `src/bin/docsgen.rs` | Generator for `docs/reference/cli.md` and `docs/reference/config.md`. |

## Key Traits and Types

| Type | Location | Why it matters |
| --- | --- | --- |
| `AgentAdapter` | `src/agent/mod.rs` | Contract every supported agent CLI implements. |
| `SpawnConfig` | `src/agent/mod.rs` | Normalized launch description used by the daemon. |
| `TeamConfig` | `src/team/config.rs` | Top-level team definition loaded from `team.yaml`. |
| `RoleDef` | `src/team/config.rs` | Per-role config: prompts, instances, routing, worktree settings. |
| `ResolvedMember` | `src/team/hierarchy.rs` | Concrete runtime member after role expansion and manager assignment. |
| `LayoutPlan` | `src/team/layout.rs` | Pane arrangement plan before tmux split commands are applied. |
| `TeamDaemon` | `src/team/daemon.rs` | Main runtime coordinator for panes, inboxes, standups, and board loop. |
| `InboxMessage` | `src/team/inbox.rs` | On-disk message unit exchanged between Batty roles. |
| `ChannelConfig` | `src/team/config.rs` | Delivery config for user-facing channels such as Telegram. |
| `TelegramBot` | `src/team/telegram.rs` | Native Telegram Bot API client used for polling and outbound messages. |
| `SessionWatcher` | `src/team/watcher.rs` | Tracks activity, prompts, and completion signals in a tmux pane. |
| `Task` | `src/task/mod.rs` | Parsed task metadata from the shared board. |
| `PhaseWorktree` | `src/worktree.rs` | Worktree metadata used for engineer isolation and merge flow. |
| `LogEntry` | `src/log/mod.rs` | Structured JSONL runtime event persisted for debugging and audits. |
| `PipeWatcher` | `src/events.rs` | Incremental output reader that extracts structured events from pane logs. |

## Contributor Notes

- Most modules carry colocated `#[cfg(test)]` suites; the main risk areas are
  tmux behavior, layout portability, worktree safety, inbox routing, and
  Telegram parsing.
- Docs reference pages are partially generated. Regenerate them with
  `./scripts/generate-docs.sh` before committing doc-affecting CLI or config changes.
- The current public runtime is team-oriented. If you find references to the
  older phase-oriented `batty work` flow, treat them as stale unless the code
  clearly still supports that path.

## Adding a New Agent Adapter

1. Create a new adapter file at `src/agent/<name>.rs` implementing `AgentAdapter`.
1. Provide `spawn_config`, `prompt_patterns`, and `format_input` behavior for that CLI.
1. If needed, override `instruction_candidates` and `wrap_launch_prompt` for agent-specific context handling.
1. Register the adapter in `adapter_from_name` in `src/agent/mod.rs`.
1. Add unit tests in the adapter module for:
   - program and args composition
   - prompt pattern detection
   - input formatting behavior
1. Validate with:
   - `cargo test`
   - `batty init`
   - `batty start --attach`
