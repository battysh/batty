---
id: 6
title: Add batty remove to docs/reference/cli.md
status: done
priority: medium
created: 2026-02-22T14:45:58.474762562-05:00
updated: 2026-02-22T14:54:49.149897969-05:00
started: 2026-02-22T14:54:10.23265352-05:00
completed: 2026-02-22T14:54:49.149897624-05:00
tags:
    - docs
    - cli-ref
class: standard
---

## Problem

The CLI reference (auto-generated from clap) is missing the `batty remove` command that was added in commit 19f12ba. The docs generation script needs to be re-run, or the remove command section needs to be added manually.

## Steps

1. Check if `scripts/generate-docs.sh` can be re-run to regenerate
2. If not, manually add a `## batty remove` section matching the style of other commands
3. Verify all other commands in the reference still match the current clap definitions

## Acceptance Criteria

- `batty remove` command documented with full usage, arguments, and options
- All existing command sections verified current

[[2026-02-22]] Sun 14:54
Regenerated CLI reference via scripts/generate-docs.sh; batty remove now documented (top-level command list + full section with target/dir flags).
