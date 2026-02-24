---
id: 4
title: Fix stale "planned for phase 4" error message
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T20:54:50.089902905-05:00
started: 2026-02-23T20:54:22.159642624-05:00
completed: 2026-02-23T20:54:50.089902574-05:00
tags:
    - bug
    - docs
---

## Bug Description

`batty work all --parallel N` produces the error:

```
`batty work all --parallel` is planned for phase 4; use --parallel 1 for now
```

Phase 4 is complete. The feature was either implemented (remove the guard) or intentionally left unimplemented (update the message to remove the "phase 4" reference).

## Root Cause

`src/main.rs` line 718: the error message references "planned for phase 4" which is now stale since Phase 4 shipped.

## Fix Approach

Since `run_all_phases()` doesn't support a `parallel` parameter, the guard is correct but the message is misleading. Update to:

```rust
"`batty work all --parallel` is not yet supported; use `batty work <phase> --parallel N` for individual phases"
```

## Files to Modify

- `src/main.rs` — line ~718

## How to Verify

1. `batty work all --parallel 2` shows updated error message
2. Confirm it no longer mentions "phase 4"

[[2026-02-23]] Mon 20:54
Updated stale guard message in src/main.rs for `batty work all --parallel`. New message: `batty work all --parallel` is not yet supported; use `batty work <phase> --parallel N` for individual phases. Verification: cargo run --bin batty -- work all --parallel 2 --dry-run now emits the updated message without any phase-4 reference.
