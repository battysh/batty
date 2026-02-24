---
id: 7
title: Fix documentation audit issues
status: backlog
priority: medium
tags: [docs]
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
