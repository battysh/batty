---
id: 6
title: Document milestone tag requirement in completion contract
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T20:55:23.00694598-05:00
started: 2026-02-23T20:54:53.919793787-05:00
completed: 2026-02-23T20:55:23.006945682-05:00
tags:
    - docs
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

[[2026-02-23]] Mon 20:55
Documented milestone requirement in docs. Changes: added "Phase Setup Requirements" section in docs/getting-started.md with milestone tag requirement, example frontmatter, and phase-summary.md completion artifact note; added troubleshooting entry "Completion fails: no milestone task found" in docs/troubleshooting.md with cause and fix command (kanban-md edit <ID> --add-tag milestone). Verification: rg -n "milestone|phase-summary.md|no milestone task found" docs/getting-started.md docs/troubleshooting.md.
