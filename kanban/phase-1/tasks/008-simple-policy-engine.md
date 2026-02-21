---
id: 8
title: Simple policy engine
status: backlog
priority: critical
created: 2026-02-21T18:40:23.019142959-05:00
updated: 2026-02-21T18:40:23.019142959-05:00
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
