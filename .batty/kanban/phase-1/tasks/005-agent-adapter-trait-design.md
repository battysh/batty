---
id: 5
title: Agent adapter trait design
status: done
priority: critical
created: 2026-02-21T18:40:22.937217626-05:00
updated: 2026-02-21T19:21:53.681814356-05:00
started: 2026-02-21T19:19:51.945938457-05:00
completed: 2026-02-21T19:21:53.681813935-05:00
tags:
    - core
depends_on:
    - 4
class: standard
---

Standard adapter trait: spawn process in PTY, tap output stream, detect prompts, inject input. Per-agent config with regex patterns for prompt detection, completion signals, error patterns. Design against Claude Code + Codex CLI interfaces even if only one ships.
