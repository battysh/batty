---
id: 7
title: PTY supervision — bidirectional interactive wrapper
status: backlog
priority: critical
created: 2026-02-21T18:40:22.992893896-05:00
updated: 2026-02-21T18:40:22.992893896-05:00
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
