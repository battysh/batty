---
id: 10
title: Add Rust linters and auto-lint workflow
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T00:04:27.466663031-05:00
started: 2026-02-21T23:57:08.307897652-05:00
completed: 2026-02-22T00:04:27.46666266-05:00
tags:
    - rust
    - quality
    - ci
claimed_by: flora-light
claimed_at: 2026-02-22T00:04:27.466662981-05:00
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

## Statement of Work

- **What was done:** Added stable lint and auto-lint commands (`make lint`, `make lint-fix`), updated CI to enforce lint gate on pull requests, and documented the workflow in `README.md`.
- **Files created:** `Makefile` (script-friendly lint targets for local dev and CI use).
- **Files modified:** `.github/workflows/ci.yml` (lint gate via `make lint` on PRs), `README.md` (lint/auto-lint command docs and underlying command expansion), `src/orchestrator.rs`, `src/work.rs`, `src/worktree.rs` (clippy-clean adjustments surfaced by strict linting).
- **Key decisions:** Used `make` target names as stable command interface because Cargo aliases cannot reliably chain both required lint steps as one command; kept strict clippy mode (`-D warnings`) in CI so lint failures block merges.
- **How to verify:** `make lint` (fails on violations, passes once clean), `make lint-fix` (applies supported fixes), and confirm CI workflow runs `make lint` on pull requests.
- **Open issues:** None for this task.
