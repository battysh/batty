---
id: 7
title: PTY supervision — bidirectional interactive wrapper
status: done
priority: critical
created: 2026-02-21T18:40:22.992893896-05:00
updated: 2026-02-21T19:28:41.959342956-05:00
started: 2026-02-21T19:23:34.88189216-05:00
completed: 2026-02-21T19:28:41.959342635-05:00
tags:
    - core
depends_on:
    - 6
class: standard
---

The hard part. Spawn agent in PTY via portable-pty (bidirectional I/O):
- Forward agent output to user's terminal in real time
- Forward user's keystrokes to agent PTY (fully interactive)
- Pattern-match output against adapter's known prompts
- Routine prompts → auto-answer per policy
- Real questions → pass through to user, forward their answer
- Completion signal → trigger DoD gate
- Batty status messages prefixed with [batty] mixed into output

[[2026-02-21]] Sat 19:28
## Statement of Work

- **What was done:** Implemented the PTY supervision module — the core bidirectional interactive wrapper that spawns agents in a pseudo-terminal, forwards output to the user's terminal in real time, pattern-matches against known prompts, and auto-answers per policy.
- **Files created:** src/supervisor/mod.rs — SessionResult enum, SupervisorEvent enum, SessionConfig struct, run_session() function with 3-thread architecture (output reader, stdin forwarder, injection handler)
- **Files modified:** src/main.rs — registered supervisor module
- **Key decisions:** Used 3-thread architecture (output reader, stdin forwarder, injection handler). Auto-injection deferred to task #12 since portable-pty only allows one writer — currently prints [batty] suggestions to stderr. Partial line buffer checked for prompts that don't end with newline.
- **How to verify:** cargo test supervisor — 6 tests including real PTY process tests (echo, failing command, multiline output)
- **Open issues:** Auto-injection into PTY deferred to task #12 when writer split is implemented.
