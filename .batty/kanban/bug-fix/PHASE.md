# Phase: bug-fix

**Status:** Not Started

## Goal

Fix bugs discovered during end-to-end integration testing of Batty against a fresh test project.

## Context

Integration testing was performed by creating a test project at `~/test/test_project`, running `batty install`, creating kanban boards with simple tasks, and exercising `batty work` in multiple modes (detached, dry-run, worktree, parallel). Several bugs and documentation gaps were discovered.

## Scope

- Fix critical runtime bugs blocking agent execution
- Fix stale error messages and documentation inconsistencies
- Improve `batty install` to scaffold config
- Fix completion contract documentation gap
- Fix docs issues identified in audit
