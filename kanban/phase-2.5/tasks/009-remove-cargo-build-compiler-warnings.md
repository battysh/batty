---
id: 9
title: Remove compiler warnings from `cargo build`
status: backlog
priority: medium
tags:
    - rust
    - quality
    - devex
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
