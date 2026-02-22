# Module Reference

Contributor-facing map of Batty source modules.

## Module Index

| Path | Purpose |
|---|---|
| `src/main.rs` | CLI entrypoint and top-level command dispatch. |
| `src/cli.rs` | clap command/flag definitions (`work`, `attach`, `resume`, `config`, `install`, `remove`, `board`). |
| `src/paths.rs` | Canonical path helpers for `.batty/` assets and boards. |
| `src/work.rs` | `batty work`/`resume` orchestration pipeline and launch-context composition. |
| `src/orchestrator.rs` | Core supervision loop (event polling, detector loop, policy/tier decisions, status updates). |
| `src/tmux.rs` | tmux command wrapper (session, pane, status-bar, capability helpers). |
| `src/events.rs` | Pipe-pane stream handling and structured event buffering. |
| `src/detector.rs` | Prompt detection state machine (silence windows + pattern matching). |
| `src/tier2.rs` | Tier 2 supervisor integration, context snapshots, and answer extraction. |
| `src/completion.rs` | Phase completion-signal detection and completion decision logic. |
| `src/worktree.rs` | Phase run worktree lifecycle and cleanup decisions. |
| `src/install.rs` | Install/remove asset workflows and prerequisite checks. |
| `src/config/mod.rs` | `.batty/config.toml` schema, defaults, and load/validation flow. |
| `src/policy/mod.rs` | Policy engine (`observe`/`suggest`/`act`) and decision evaluation. |
| `src/prompt/mod.rs` | Prompt kinds/patterns and ANSI-safe matching helpers. |
| `src/log/mod.rs` | JSONL execution log types and writer APIs. |
| `src/task/mod.rs` | kanban task parsing, board metadata, and task selection helpers. |
| `src/supervisor/mod.rs` | Supervisor runtime helpers and process integration logic. |
| `src/dod/mod.rs` | Definition-of-done gate execution and result handling. |
| `src/agent/mod.rs` | `AgentAdapter` trait, adapter registry, and spawn contract. |
| `src/agent/claude.rs` | Claude adapter implementation and prompt pattern wiring. |
| `src/agent/codex.rs` | Codex adapter implementation and launch prompt wrapping. |
| `src/bin/docsgen.rs` | Docs generator for `docs/reference/*.md` and config docs. |

## Key Traits and Types

| Type | Location | Why it matters |
|---|---|---|
| `AgentAdapter` trait | `src/agent/mod.rs` | Adapter contract for any executor CLI (spawn config, prompt patterns, input formatting). |
| `SpawnConfig` | `src/agent/mod.rs` | Normalized process spawn description used by orchestration. |
| `PromptDetector` + `DetectorEvent` | `src/detector.rs` | State machine that turns output/silence into actionable prompt events. |
| `PromptPatterns` + `PromptKind` | `src/prompt/mod.rs` | Regex-based prompt classification for Tier 1/Tier 2 routing. |
| `PolicyEngine` + `Decision` | `src/policy/mod.rs` | Maps detected prompts to observe/suggest/act/escalate behavior. |
| `OrchestratorConfig` + `OrchestratorResult` | `src/orchestrator.rs` | Main runtime configuration and session exit outcomes. |
| `StatusBar` + `StatusIndicator` | `src/orchestrator.rs` | tmux status rendering for live supervision feedback. |
| `Tier2Config` + `Tier2Result` | `src/tier2.rs` | Supervisor escalation contract and result envelope. |
| `EventBuffer` + `PipeWatcher` | `src/events.rs` | Pipe-pane event ingestion and streaming extraction. |
| `ProjectConfig` | `src/config/mod.rs` | Full project config model loaded from `.batty/config.toml`. |
| `Task` | `src/task/mod.rs` | Parsed board task model used during execution/reporting. |
| `ExecutionLog` + `LogEntry` | `src/log/mod.rs` | Structured JSONL event persistence API. |
| `PhaseWorktree` | `src/worktree.rs` | Run-scoped worktree metadata used by `work/resume`. |
| `CompletionDecision` | `src/completion.rs` | Final completion signal used to stop a phase cleanly. |

## Test Coverage Snapshot

- Current test inventory: `323` tests (`cargo test -- --list`).
- Core modules include colocated `#[cfg(test)]` suites (detector, orchestrator, tmux, work, config, policy, agent adapters, docsgen, etc.).
- Tests emphasize:
  - prompt-detection transitions and fallback behavior
  - policy decisions and auto-answer routing
  - tmux command/capability handling
  - config parsing/default behavior
  - worktree/install/remove safety paths

## Adding a New Agent Adapter

1. Create a new adapter file at `src/agent/<name>.rs` implementing `AgentAdapter`.
2. Provide `spawn_config`, `prompt_patterns`, and `format_input` behavior for that CLI.
3. If needed, override `instruction_candidates` and `wrap_launch_prompt` for agent-specific context handling.
4. Register the adapter in `adapter_from_name` in `src/agent/mod.rs`.
5. Add unit tests in the adapter module for:
   - program/args composition
   - prompt pattern detection
   - input formatting behavior
6. Validate integration path with:
   - `cargo test`
   - `batty work <phase> --agent <name>` (or config default override).
