---
id: 6
title: Claude Code adapter
status: done
priority: critical
created: 2026-02-21T18:40:22.966151562-05:00
updated: 2026-02-21T19:23:26.343082466-05:00
started: 2026-02-21T19:22:02.286053773-05:00
completed: 2026-02-21T19:23:26.343082175-05:00
tags:
    - core
depends_on:
    - 5
class: standard
---

First adapter targeting Claude Code. Prompt detection: permission prompts (Allow tool X?), questions, Continue? y/n. Completion signal detection. Error pattern recognition. Must not break Claude's native interactive UX — user can still type into the session.
