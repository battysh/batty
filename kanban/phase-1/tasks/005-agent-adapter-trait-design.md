---
id: 5
title: Agent adapter trait design
status: backlog
priority: critical
created: 2026-02-21T18:40:22.937217626-05:00
updated: 2026-02-21T18:40:22.937217626-05:00
tags:
    - core
depends_on:
    - 4
class: standard
---

Standard adapter trait: spawn process in PTY, tap output stream, detect prompts, inject input. Per-agent config with regex patterns for prompt detection, completion signals, error patterns. Design against Claude Code + Codex CLI interfaces even if only one ships.
