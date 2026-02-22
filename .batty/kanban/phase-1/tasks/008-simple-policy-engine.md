---
id: 8
title: Simple policy engine
status: done
priority: critical
created: 2026-02-21T18:40:23.019142959-05:00
updated: 2026-02-21T18:56:25.954899299-05:00
started: 2026-02-21T18:55:03.310013432-05:00
completed: 2026-02-21T18:56:25.954898978-05:00
tags:
    - core
depends_on:
    - 1
class: standard
---

Read from .batty/config.toml:
- observe: log only, never auto-answer
- suggest: print suggestion, wait for user confirmation  
- act: auto-respond to known prompts, log for audit
All auto-responses logged to execution log.
