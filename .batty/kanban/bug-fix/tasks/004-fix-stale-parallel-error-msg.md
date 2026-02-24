---
id: 4
title: Fix stale "planned for phase 4" error message
status: backlog
priority: medium
tags: [bug, docs]
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
