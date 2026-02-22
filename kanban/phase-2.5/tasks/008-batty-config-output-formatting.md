---
id: 8
title: Improve `batty config` output formatting
status: backlog
priority: medium
tags:
    - cli
    - ux
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
