---
id: 3
title: Human review gate
status: backlog
priority: critical
tags:
    - core
depends_on:
    - 2
class: standard
---

On phase completion, generate a standardized review packet and require an explicit human decision.

Review packet includes:
- Diff against main: `git diff main...phase-X-run-NNN`
- phase-summary.md
- Statements of work from each task
- Execution log

Reviewer decisions:
- **Merge** — work meets standards
- **Rework** — writes specific feedback for the executor
- **Escalate** — pause and surface for manual handling

Decision must be persisted in structured logs for downstream automation.
