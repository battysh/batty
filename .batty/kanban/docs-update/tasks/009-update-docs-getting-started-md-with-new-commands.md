---
id: 9
title: Update docs/getting-started.md with new commands and modes
status: done
priority: medium
created: 2026-02-22T14:46:21.303468482-05:00
updated: 2026-02-22T14:56:07.578628801-05:00
started: 2026-02-22T14:55:29.866094328-05:00
completed: 2026-02-22T14:56:07.578628474-05:00
tags:
    - docs
    - getting-started
class: standard
---

## Problem

Getting started guide is fairly current but missing recent additions:

1. **Missing `batty remove` command** — should be documented as the inverse of `batty install`
2. **Missing dangerous mode** — important config option users should know about
3. **Missing supervisor hotkeys** — Ctrl+b P (pause) and Ctrl+b R (resume)
4. **Missing `batty board --print-dir`** — useful for scripting

## Acceptance Criteria

- `batty remove` documented
- Dangerous mode section or mention added
- Supervisor hotkeys mentioned
- All flags/options from the current CLI reflected

[[2026-02-22]] Sun 14:56
Expanded getting-started with batty remove usage, board --print-dir, dangerous_mode config section, supervisor pause/resume hotkeys, and a quick reference covering current command flags.
