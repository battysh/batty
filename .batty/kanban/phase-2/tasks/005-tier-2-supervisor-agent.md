---
id: 5
title: 'Tier 2: on-demand supervisor agent'
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:38:12.489992994-05:00
started: 2026-02-21T20:34:54.816117379-05:00
completed: 2026-02-21T20:38:12.489992333-05:00
tags:
    - core
depends_on:
    - 2
    - 4
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:38:12.489992934-05:00
class: standard
---

When Tier 1 can't match a prompt, make a single API call to a supervisor agent. Not a persistent session — one call per question, stateless.

## Context composition

Compose a structured snapshot for the supervisor:
- **Project docs** as system prompt (architecture, conventions — cached)
- **Event buffer** snapshot (compact summary of what executor has done, from task #2)
- **The detected question** (from task #3)

## Response flow

1. Supervisor receives structured snapshot (NOT raw terminal output).
2. Supervisor responds with an answer.
3. Batty injects answer via `tmux send-keys -t batty-phase-1 "<answer>" Enter`.
4. Log the supervisor's decision to execution log and supervisor log pane.

## Escalation

If the supervisor can't decide (low confidence, ambiguous requirements):
- Surface the question to the human.
- Show in supervisor log pane: `[batty] ⚠ NEEDS INPUT: "<question>"`
- Update tmux status bar: `⚠ waiting for human`
- Human types directly in the executor pane — native tmux behavior.

## Implementation

- New module or extend existing: supervisor agent API call (Anthropic API or similar).
- Context builder that reads project docs + formats event buffer.
- Configurable model/provider in `.batty/config.toml`.

[[2026-02-21]] Sat 20:38
Statement of work:
- Created src/tier2.rs: on-demand supervisor agent (single API call per question)
- Tier2Config: configurable program/args/timeout/system_prompt (default: claude -p)
- compose_context(): builds structured prompt with supervisor role, project docs, event buffer, question
- call_supervisor(): shells out to supervisor command, parses ESCALATE signals
- load_project_docs(): reads CLAUDE.md and architecture.md for context
- Integrated into orchestrator: Escalate decisions now try Tier 2 before human
- 10 unit tests for context composition, supervisor call, escalation, error handling
- handle_prompt() in orchestrator now has full Tier 1 + Tier 2 pipeline
