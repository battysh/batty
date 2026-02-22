---
id: 7
title: CLI install for Claude+Codex skills and steering docs
status: backlog
priority: high
created: 2026-02-21T21:05:50.863448639-05:00
updated: 2026-02-21T21:05:50.863448639-05:00
tags:
    - cli
    - agent
    - onboarding
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
