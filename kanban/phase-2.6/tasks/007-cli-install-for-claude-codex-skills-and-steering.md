---
id: 7
title: CLI install for Claude+Codex skills and steering docs
status: done
priority: high
created: 2026-02-21T21:05:50.863448639-05:00
updated: 2026-02-21T23:49:08.845816057-05:00
started: 2026-02-21T23:46:13.837105785-05:00
completed: 2026-02-21T23:49:08.845815666-05:00
tags:
    - cli
    - agent
    - onboarding
claimed_by: flora-light
claimed_at: 2026-02-21T23:49:08.845816006-05:00
class: standard
---

Add a Batty CLI command that installs both agent skills and steering docs for Claude and Codex in one step.

## Requirements

1. Add a command (or subcommand) that installs skill packs for both agents.
2. Install/update steering docs for both agents:
   - Claude steering (`CLAUDE.md` or equivalent target for Claude tooling)
   - Codex steering (`AGENTS.md` or equivalent target for Codex tooling)
3. Support explicit target selection:
   - install both (default)
   - install only Claude
   - install only Codex
4. Make command behavior deterministic and idempotent (safe to re-run).
5. Document the command and file destinations in `README.md`.

## Verification

1. Run command on a clean test directory and verify both agent destinations are created.
2. Re-run command and verify no duplicate/conflicting output.
3. Run with single-agent selection flags and verify only the requested target is installed.

## Statement of Work

- **What was done:** Added a new `batty install` command with deterministic/idempotent file installation for Claude, Codex, or both agent targets.
- **Files created:** `src/install.rs` (asset installer module with templates, write-if-changed behavior, and unit tests).
- **Files modified:** `src/cli.rs` (new `install` subcommand and parsing tests); `src/main.rs` (command wiring + install output); `README.md` (install command usage and destination files).
- **Key decisions:** Used a stable `--target both|claude|codex` selector and deterministic write order; used overwrite-on-change semantics so repeated runs are safe and produce `unchanged` output when no updates are needed.
- **How to verify:** `cargo test`; then run `cargo run -- install --dir "$(mktemp -d)"`, rerun the same command, and run `cargo run -- install --target claude|codex --dir "$(mktemp -d)"` to verify single-target behavior.
- **Open issues:** None for this task.
