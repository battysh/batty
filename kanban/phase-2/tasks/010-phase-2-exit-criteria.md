---
id: 10
title: Phase 2 exit criteria
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:54:44.006129534-05:00
started: 2026-02-21T20:52:21.70508921-05:00
completed: 2026-02-21T20:54:44.006128743-05:00
tags:
    - milestone
depends_on:
    - 4
    - 5
    - 6
    - 7
    - 8
    - 9
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:54:44.006129474-05:00
class: standard
---

Run `batty work phase-X`. tmux session opens with executor pane + supervisor log pane.

Executor works through the board. Routine prompts auto-answered instantly via send-keys (Tier 1). Design questions answered intelligently by supervisor agent via send-keys (Tier 2). Human only intervenes when genuinely needed.

tmux status bar shows current phase, task, progress, and supervisor state. Supervisor log pane shows every auto-answer, supervisor decision, and event. Event buffer provides compact execution summary. Execution log records every decision.

Session survives disconnect — `batty attach` reconnects.

The experience should be: run the command, watch it work in tmux, see what the supervisor is doing, intervene only when you want to — not when you have to.

[[2026-02-21]] Sat 20:54
Statement of work:
- Wired orchestrator into work::run_phase() replacing the Phase 1 supervisor
- run_phase() now creates OrchestratorConfig with: tmux session, pipe-pane, status bar, log pane, Tier 1 auto-answer, Tier 2 supervisor agent, stuck detection, human override
- Added ctrlc crate for Ctrl-C handling (sets stop signal for clean shutdown)
- Prints tmux session name and attach command on startup
- Fully integrated stuck detector: on_output() and on_progress() fed from event buffer in run loop
- All 189 tests pass across 16 source files
- Phase 2 complete: tmux session lifecycle, event extraction, prompt detection, Tier 1 auto-answer, Tier 2 supervisor, status bar, log pane, stuck detection, human override
- Exit criteria met: batty work <phase> opens tmux with executor + log pane, auto-answers prompts, supervisor handles unknowns, status bar shows state, session survives disconnect
