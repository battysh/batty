---
id: 3
title: Update planning/roadmap.md phase statuses
status: done
priority: high
created: 2026-02-22T14:45:45.034696837-05:00
updated: 2026-02-22T14:53:10.995136298-05:00
started: 2026-02-22T14:52:34.495598295-05:00
completed: 2026-02-22T14:53:10.995135988-05:00
tags:
    - docs
    - planning
class: standard
---

## Problem

The roadmap has stale phase statuses and is missing phases entirely:

1. **Phase 2.4** heading says "(Next)" — should say "(Done)"
2. **Phase 2.5** — no status marker, should say "(Done)"
3. **Phase 2.6 missing entirely** — Quality & Configuration phase (batty install, batty config --json, lint workflow, cargo warning cleanup). Needs a section.
4. **Phase 2.7 missing entirely** — Minor Improvements phase (supervisor hotkeys, dangerous-mode wrappers, tier2 snapshots, docs pipeline, secret redaction). Needs a section.
5. **Phase 1 test count** says "98 tests passing" — actual count is now 319+
6. **Phase 3A** references Codex CLI adapter as a task — but the Codex adapter already exists in src/agent/codex.rs

## Acceptance Criteria

- All completed phases marked (Done) with accurate summaries
- Phase 2.6 and 2.7 sections added
- Test counts updated to reflect current state
- Phase 3A tasks reflect what's actually still needed (remove already-done items)

[[2026-02-22]] Sun 14:53
Updated roadmap phase statuses (2.4/2.5 done), added missing 2.6 and 2.7 sections, updated stale test count to current 323-test inventory, and removed stale 3A Codex adapter item (already implemented).
