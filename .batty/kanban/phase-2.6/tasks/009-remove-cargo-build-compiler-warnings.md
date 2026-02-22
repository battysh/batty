---
id: 9
title: Remove compiler warnings from `cargo build`
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T23:56:37.496748312-05:00
started: 2026-02-21T23:54:30.685876255-05:00
completed: 2026-02-21T23:56:37.496747892-05:00
tags:
    - rust
    - quality
    - devex
claimed_by: flora-light
claimed_at: 2026-02-21T23:56:37.496748232-05:00
class: standard
---

Make default developer builds cleaner by removing avoidable warnings.

## Requirements

1. Run `cargo build` and collect current warnings.
2. Remove warnings where the code path is valid and should be used.
3. For intentionally-unused code, either:
   - move behind feature/test gates, or
   - add targeted `#[allow(...)]` with short justification.
4. Keep behavior unchanged; this is a cleanliness/refactor task.
5. Document any intentionally-retained warnings (if unavoidable).

## Verification

1. `cargo build` produces zero warnings, or only explicitly documented exceptions.
2. `cargo test` still passes.

## Statement of Work

- **What was done:** Removed all default `cargo build` warnings by scoping dormant/test-only APIs behind `#[cfg(test)]` and adding targeted `#[allow(dead_code)]` attributes where code is intentionally retained for fallback paths.
- **Files created:** None.
- **Files modified:** `src/agent/mod.rs`, `src/detector.rs`, `src/events.rs`, `src/supervisor/mod.rs`, `src/tier2.rs`, `src/tmux.rs`, `src/work.rs`.
- **Key decisions:** Preserved behavior by avoiding refactors to runtime paths; used narrow annotations with short justifications for intentionally-unused surfaces and test-only helpers.
- **How to verify:** `cargo build` (no warnings); `cargo test` (all tests passing).
- **Open issues:** No retained build warnings.
