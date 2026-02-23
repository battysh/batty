# Phase docs-update: Documentation Sync

**Status:** Next
**Board:** `.batty/kanban/docs-update/`
**Depends on:** Phase 2.7 in-progress (no code dependency — docs-only phase)

## Goal

Bring all project documentation into alignment with the current state of the codebase. After phases 2.4–2.7 shipped significant features, multiple docs have stale statuses, missing commands, phantom dependencies, and outdated project structure references.

## Scope

- Fix stale content in README.md, CLAUDE.md, and planning/ docs
- Update phase statuses and test counts in roadmap.md
- Add missing CLI commands (`batty remove`) to reference docs
- Expand thin docs (architecture, troubleshooting, getting-started)
- Audit planning/ docs against actual implementations
- Optionally create a contributor-facing module reference

## Non-Goals

- No code changes — this phase is documentation only
- No new features or refactoring
- No changes to the docs pipeline or MkDocs config

## Exit Criteria

- All tasks in `.batty/kanban/docs-update/tasks/` are `done`.
- Every doc file accurately reflects the current codebase (phases 1–2.7).
- No doc references phantom dependencies, wrong paths, or unimplemented features presented as working.
- `batty remove` and other recent additions are documented.
