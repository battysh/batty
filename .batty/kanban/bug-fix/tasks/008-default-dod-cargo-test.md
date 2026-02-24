---
id: 8
title: Fix default DoD command hardcoded to "cargo test"
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T20:58:22.834576855-05:00
started: 2026-02-23T20:57:26.34956173-05:00
completed: 2026-02-23T20:58:22.834576539-05:00
tags:
    - bug
    - config
---

## Bug Description

`src/completion.rs` line 19 defines:

```rust
const DEFAULT_DOD_COMMAND: &str = "cargo test";
```

When config has `dod = null` (no DoD configured), the completion contract still reports `dod_command: "cargo test"`. For non-Rust projects, this would fail or give misleading results.

The DoD should only run when explicitly configured. When `dod` is null/unset, the completion contract should skip the DoD gate entirely (which it does correctly — `dod_executed: false`), but it should NOT report a default command that implies `cargo test` will be run.

## Fix Approach

Either:
1. Change `DEFAULT_DOD_COMMAND` to something like `"(none)"` or empty string when not configured
2. Make the `dod_command` field in `CompletionDecision` an `Option<String>` to clearly indicate when no DoD is configured

This is relatively low priority since the DoD isn't actually executed when not configured, but it's confusing in logs and could mislead debugging.

## Files to Modify

- `src/completion.rs` — line 19 and related logic

## How to Verify

1. Set `dod` to null in config
2. Run a phase
3. Check execution.jsonl — `dod_command` should indicate no DoD configured

[[2026-02-23]] Mon 20:58
Fixed completion DoD default behavior in src/completion.rs. Changes: removed hardcoded `cargo test` fallback for completion reporting; added `(none)` sentinel when DoD is not configured; DoD now executes only when defaults.dod is explicitly set; no-DoD path skips execution and is treated as passing the DoD gate. Added unit test `completion_passes_without_dod_when_unset` to verify completion succeeds with dod_executed=false and dod_command="(none)". Verification: cargo test completion::tests -- --nocapture (7 passed).
