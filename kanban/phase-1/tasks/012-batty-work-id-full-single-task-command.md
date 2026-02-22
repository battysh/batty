---
id: 12
title: batty work <phase> — full phase runner command
status: done
priority: critical
created: 2026-02-21T18:40:23.111209121-05:00
updated: 2026-02-21T19:39:40.290451616-05:00
started: 2026-02-21T19:34:33.67949235-05:00
completed: 2026-02-21T19:39:40.290451285-05:00
tags:
    - core
    - milestone
depends_on:
    - 3
    - 7
    - 8
    - 9
    - 11
class: standard
---

Wire it all together: read phase board → spawn agent in PTY with phase context → supervise session as agent works through tasks → test gates at checkpoints → ensure commits → log decisions → update kanban-md statuses. This is the product.

The agent manages task flow (picking, implementing, marking done). Batty manages the session (supervision, policy, tests, logging).

Exit criteria: `batty work phase-1` in your current terminal. Claude appears interactively and works through the phase board. Batty auto-answers routine prompts. Tests run at checkpoints. All tasks get statements of work. Execution log inspectable after.

[[2026-02-21]] Sat 19:39
## Statement of Work

- **What was done:** Wired all Phase 1 components into a working batty work <phase> command — the milestone deliverable.
- **Files created:** src/work.rs — run_phase() orchestration, build_phase_prompt(), terminal_size()
- **Files modified:** src/main.rs (rewired Work command), Cargo.toml (term_size), src/agent/*.rs, src/dod/mod.rs, src/log/mod.rs, src/policy/mod.rs, src/prompt/mod.rs, src/task/mod.rs (dead_code annotations for future-use items)
- **Key decisions:** Agent receives full phase context as prompt (board summary, task list with deps). Supervisor events bridged to execution log via channel+thread. Terminal size auto-detected. Parallel flag accepted but not yet implemented (future phase).
- **How to verify:** cargo test work — 6 tests. cargo build && ./target/debug/batty work phase-1 (requires claude CLI installed).
- **Open issues:** DoD checks not triggered automatically yet (needs agent completion signal detection). Auto-injection still deferred (PTY writer split). Parallel execution deferred to phase 6.
