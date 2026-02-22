---
id: 10
title: Add Rust linters and auto-lint workflow
status: backlog
priority: medium
tags:
    - rust
    - quality
    - ci
class: standard
---

Add a standard lint/format workflow so code quality checks are easy and consistent.

## Requirements

1. Add lint commands for:
   - `cargo fmt -- --check`
   - `cargo clippy --all-targets --all-features -- -D warnings`
2. Add an auto-fix command for local dev:
   - `cargo fmt`
   - `cargo clippy --fix --allow-dirty --allow-staged`
3. Wire lint checks into CI (if not already present) for pull requests.
4. Document lint and auto-lint commands in `README.md` (or a contributor/dev doc).
5. Keep command names stable and script-friendly.

## Verification

1. Lint command fails on style/lint violations.
2. Auto-lint command fixes supported issues.
3. CI blocks merges on lint failures.
