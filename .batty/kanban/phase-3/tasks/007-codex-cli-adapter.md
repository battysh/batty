---
id: 7
title: Codex CLI adapter
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:14:46.439849742-05:00
started: 2026-02-22T15:14:23.847757129-05:00
completed: 2026-02-22T15:14:46.439849425-05:00
tags:
    - core
depends_on:
    - 6
class: standard
---

Second agent adapter. Validates that the architecture generalizes beyond Claude Code. Study Codex CLI output patterns, implement event extraction patterns and prompt detection. Test with `batty work <phase>` using Codex as executor.

[[2026-02-22]] Sun 15:14
Validated Codex adapter path is already implemented and wired through AgentAdapter. Verified codex spawn config, instruction-file preference (AGENTS.md first), launch prompt wrapper, prompt pattern detection, and adapter lookup integration via targeted tests: cargo test agent::codex::tests:: ; cargo test prompt::tests::codex_ ; cargo test agent::tests::lookup_adapter_by_name.
