---
id: 8
title: Improve `batty config` output formatting
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T23:54:02.576074172-05:00
started: 2026-02-21T23:51:02.539456858-05:00
completed: 2026-02-21T23:54:02.576073791-05:00
tags:
    - cli
    - ux
claimed_by: flora-light
claimed_at: 2026-02-21T23:54:02.576074122-05:00
class: standard
---

Make `batty config` output cleaner and easier to scan during development and demos.

## Requirements

1. Reduce noisy startup output for this command (or provide a quiet mode).
2. Group config by sections with consistent alignment:
   - defaults
   - supervisor
   - source path
3. Render arrays/maps in a readable format (not debug dumps).
4. Add optional machine-readable output (`--json`) for scripting.
5. Document examples in `README.md`.

## Verification

1. `batty config` shows concise human-readable output.
2. `batty config --json` returns valid JSON.
3. Output is stable when `.batty/config.toml` is missing (defaults mode).

## Statement of Work

- **What was done:** Reworked `batty config` output into sectioned human-readable formatting and added `--json` machine output with stable key ordering for auto-answer maps.
- **Files created:** None.
- **Files modified:** `src/cli.rs` (added `config --json` flag parsing test), `src/main.rs` (new renderers, config command behavior, quiet default for config), `README.md` (config output mode examples).
- **Key decisions:** Kept `config` quiet by default (no startup info log noise), while retaining verbose logging when `-v` is used; rendered arrays/maps explicitly instead of debug dumps; used a deterministic `BTreeMap` for JSON auto-answer ordering.
- **How to verify:** `target/debug/batty config`; `target/debug/batty config --json`; from a clean temp directory run `target/debug/batty config` and `target/debug/batty config --json` to confirm defaults mode behavior.
- **Open issues:** None for this task.
