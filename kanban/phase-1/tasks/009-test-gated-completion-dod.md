---
id: 9
title: Test-gated completion (DoD)
status: backlog
priority: critical
created: 2026-02-21T18:40:23.042476942-05:00
updated: 2026-02-21T18:40:23.042476942-05:00
tags:
    - core
depends_on:
    - 7
    - 8
class: standard
---

DoD command from .batty/config.toml or task body override. Agent signals completion → run DoD command. Tests pass → proceed to merge. Tests fail → feed failure output back to agent, retry up to max_retries.
