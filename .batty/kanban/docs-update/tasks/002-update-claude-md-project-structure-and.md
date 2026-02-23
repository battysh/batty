---
id: 2
title: Update CLAUDE.md project structure and dependencies
status: done
priority: high
created: 2026-02-22T14:45:37.742918581-05:00
updated: 2026-02-22T14:52:27.820344388-05:00
started: 2026-02-22T14:51:36.237679641-05:00
completed: 2026-02-22T14:52:27.820344079-05:00
tags:
    - docs
    - claude-md
class: standard
---

## Problem

CLAUDE.md Project Structure section is outdated:

1. **Missing phases:** phase-2.4, phase-2.6, phase-2.7, phase-3b are not listed under .batty/kanban/
2. **Wrong descriptions:**
   - phase-2 listed without DONE status
   - phase-2.5 described as "Adjustments and ideas (parking lot)" — it was actually "Runtime Hardening + Dogfood" and is DONE
   - phase-3 listed as "Director & review gate" — actually split into phase-3 (3A: Sequencer) and phase-3b
3. **Missing directories:** docs/, assets/, scripts/, .agents/, .claude/ not shown in structure
4. **Key Dependencies section incomplete:** Missing anyhow, thiserror, ctrlc, tracing, tracing-subscriber, serde_json, term_size. Only shows 7 of 14+ actual dependencies.
5. **Missing commands:** No mention of `batty remove` command

## Acceptance Criteria

- Project Structure matches actual directory layout
- All phases listed with correct names, descriptions, and statuses
- Key Dependencies matches Cargo.toml
- All CLI commands mentioned

[[2026-02-22]] Sun 14:52
Synced CLAUDE.md structure with actual top-level dirs and full phase list (.batty/kanban including docs-update and phase-3b), expanded dependency snippet to match Cargo.toml dependencies, and added current CLI command surface including batty remove.
