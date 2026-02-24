---
id: 7
title: Fix documentation audit issues
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T20:57:23.097843569-05:00
started: 2026-02-23T20:55:29.077658724-05:00
completed: 2026-02-23T20:57:23.097843239-05:00
tags:
    - docs
---

## Issues Found in Documentation Audit

### Critical
1. **Phase 4 status header incorrect**: `.batty/kanban/phase-4/PHASE.md` line 3 says `**Status:** Not Started` but all 7 tasks are done. Change to `**Status:** Done`.

### Medium
2. **Test count stale**: `planning/roadmap.md` line 20 says "394+ tests" but actual count is 370. Update.
3. **Command naming inconsistency**: Some docs use `batty board-list` while CLI reference uses `batty list` as primary. Standardize:
   - `docs/getting-started.md` line 72
   - `docs/troubleshooting.md` lines 118, 142
   - `CLAUDE.md` / `AGENTS.md` line 79
4. **Execution loop status outdated**: `planning/execution-loop.md` lines 4-5 and 58-66 reference phases 3A/3B as "planned" when they're implemented.

## Files to Modify

- `.batty/kanban/phase-4/PHASE.md`
- `planning/roadmap.md`
- `planning/execution-loop.md`
- `docs/getting-started.md`
- `docs/troubleshooting.md`
- `CLAUDE.md`
- `AGENTS.md`

## How to Verify

1. `batty list` shows phase-4 as Done
2. Grep for "board-list" in docs — should only appear as alias mention
3. Grep for "394" — should not appear
4. Grep for "planned for Phase 3" in execution-loop — should not appear

[[2026-02-23]] Mon 20:57
Completed documentation audit fixes.\n\nChanges:\n- Updated .batty/kanban/phase-4/PHASE.md status to Done.\n- Updated stale test-count references from 394/394+ to 370 in planning/roadmap.md, docs/reference/modules.md, and README.md.\n- Standardized board listing command to `batty list` (kept board-list as alias mention) in docs/getting-started.md, AGENTS.md, CLAUDE.md, and README.md.\n- Updated planning/execution-loop.md status text to reflect implemented Phase 3A/3B review+director flow and implemented merge automation.\n\nVerification:\n- rg -n "394" README.md planning docs AGENTS.md CLAUDE.md -> no matches\n- rg -n "planned for Phase 3|planned/aspirational|planned for phase 3" planning/execution-loop.md -> no matches\n- rg -n "board-list" docs AGENTS.md CLAUDE.md README.md planning -> only alias mentions remain.
