---
id: 8
title: Fix default DoD command hardcoded to "cargo test"
status: backlog
priority: medium
tags: [bug, config]
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
