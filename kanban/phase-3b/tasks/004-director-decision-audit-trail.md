---
id: 4
title: Director decision audit trail
status: backlog
priority: high
tags:
    - core
    - audit
depends_on:
    - 1
class: standard
---

Persist director decision records in structured logs.

Minimum fields:
- phase id
- decision (`merge`/`rework`/`escalate`)
- short rationale
- confidence if available
- linked artifacts (summary, diff ref, log ref)
- final outcome after human override or execution
