---
id: 5
title: Support dangerous-mode wrappers for claude and codex
status: done
priority: high
created: 2026-02-22T00:22:19.406604737-05:00
updated: 2026-02-22T00:37:53.414744552-05:00
started: 2026-02-22T00:33:54.994027376-05:00
completed: 2026-02-22T00:37:53.414744021-05:00
tags:
    - agent
    - supervisor
    - runtime
    - config
claimed_by: cape-staff
claimed_at: 2026-02-22T00:37:53.414744502-05:00
class: standard
---

Ensure Batty can run both the executor agent and Tier 2 supervisor through local wrapper functions/commands that inject danger-mode flags.

## Desired wrapper behavior

Use shell wrappers equivalent to:

```sh
# Wrap claude with --dangerously-skip-permissions
claude() { command claude --dangerously-skip-permissions "$@"; }

# Wrap codex with --dangerously-bypass-approvals-and-sandbox
codex() { command codex --dangerously-bypass-approvals-and-sandbox "$@"; }
```

## Requirements

1. Batty can launch executor and supervisor with these wrappers applied for both `claude` and `codex` paths.
2. Behavior is configurable/documented (clear how to enable and what command Batty executes).
3. No regressions for default non-wrapper execution.
4. Log output shows effective program/args used for launch.

## Verification

1. Run `batty work <phase>` with wrapper-enabled config and confirm launch args include the dangerous flags.
2. Run with default config and confirm legacy behavior is unchanged.
3. `cargo test` passes.

[[2026-02-22]] Sun 00:37
Added config-driven dangerous wrappers ([dangerous_mode].enabled) that prepend claude/codex danger flags for both executor and Tier2 supervisor commands; launch logs now include effective program+args; README documents how to enable.

## Statement of Work

- **What was done:** Added configurable dangerous-mode wrapper behavior for executor and Tier 2 supervisor launch commands.
- **Files created:** None.
- **Files modified:** `src/config/mod.rs` - added `[dangerous_mode].enabled` config; `src/work.rs` - applied dangerous flags for executor/supervisor command construction; `src/log/mod.rs` - logged launched agent args; `src/orchestrator.rs` - logged effective executor/supervisor commands; `src/main.rs` - surfaced dangerous-mode in `batty config` outputs; `README.md` - documented enablement and runtime behavior.
- **Key decisions:** Implemented wrappers as deterministic arg injection (rather than shell-function dependence) keyed by effective binary (`claude`/`codex`) so behavior remains explicit, testable, and default-off.
- **How to verify:** Set `[dangerous_mode].enabled = true`, run `batty work <phase>`, and confirm orchestrator log events show commands including dangerous flags; run `cargo test`.
- **Open issues:** Task #4 still needs operator docs/tests focused specifically on pause/resume hotkeys.
