---
id: 6
title: Document milestone tag requirement in completion contract
status: backlog
priority: medium
tags: [docs]
---

## Gap Description

The completion contract in `src/completion.rs` requires at least one task tagged `milestone` for a phase to pass completion. If no task has the `milestone` tag, the completion fails with:

```
no milestone task found (expected a task tagged 'milestone')
```

This requirement is not documented in:
- `docs/getting-started.md`
- `docs/architecture.md`
- The PHASE.md template guidance
- The batty-workflow.md steering document

Users creating new kanban boards have no way to know they need a `milestone` tag on at least one task.

## Fix Approach

1. Add a note about the `milestone` tag requirement to `docs/getting-started.md` in the "Phase Setup" section
2. Add it to `docs/troubleshooting.md` as a common issue
3. Consider mentioning it in the launch context's "Required Completion Artifacts" section

Alternatively, consider making the milestone check optional (warn but don't fail) when no milestone-tagged tasks exist, since many simple boards won't have explicit milestones.

## Files to Modify

- `docs/getting-started.md` — add note about milestone tag
- `docs/troubleshooting.md` — add troubleshooting entry
- Optionally: `src/completion.rs` — soften milestone check to warning

## How to Verify

1. Check that docs mention the `milestone` tag requirement
2. If milestone check is softened: create a board with no milestone-tagged tasks and verify completion still passes (with warning)
