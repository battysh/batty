---
id: 9
title: Test-gated completion (DoD)
status: done
priority: critical
created: 2026-02-21T18:40:23.042476942-05:00
updated: 2026-02-21T19:31:28.936587847-05:00
started: 2026-02-21T19:29:08.990382683-05:00
completed: 2026-02-21T19:31:28.936587286-05:00
tags:
    - core
depends_on:
    - 7
    - 8
class: standard
---

DoD command from .batty/config.toml or task body override. Agent signals completion → run DoD command. Tests pass → proceed to merge. Tests fail → feed failure output back to agent, retry up to max_retries.

[[2026-02-21]] Sat 19:31
## Statement of Work

- **What was done:** Implemented the Definition of Done (DoD) module — test-gated completion verification with retry support.
- **Files created:** src/dod/mod.rs — DodResult, DodOutcome, DodConfig structs, resolve(), run_dod_command(), run_dod_cycle(), format_failure_feedback()
- **Files modified:** src/main.rs — registered dod module
- **Key decisions:** Runs commands via sh -c for shell expression support. DodConfig::resolve() merges task-level overrides with project defaults. on_failure callback enables caller to feed failures back to agent. Output truncated to 4096 chars for agent feedback.
- **How to verify:** cargo test dod — 17 tests covering resolution, command execution, retry cycles, output capture, and feedback formatting.
- **Open issues:** None — ready for wiring in task #12.
