---
id: 1
title: Director review agent
status: backlog
priority: critical
tags:
    - core
    - director
class: standard
---

On phase completion, call a director agent with structured context:

- `git diff main...phase-X-run-NNN`
- `phase-summary.md`
- statements of work
- execution log excerpts
- project standards docs

Director returns one structured decision:
- `merge`
- `rework` (with explicit feedback)
- `escalate` (with reason)
