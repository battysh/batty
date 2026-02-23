---
id: 7
title: Integration tests and exit criteria
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T01:23:59.629458235-05:00
started: 2026-02-23T01:20:21.485144586-05:00
completed: 2026-02-23T01:23:59.605551716-05:00
tags:
    - testing
    - milestone
depends_on:
    - 1
    - 2
    - 3
    - 4
    - 5
    - 6
class: standard
---

End-to-end validation that parallel execution works and the project is shippable.

## Integration Tests

- Create a synthetic phase board with a known DAG (8+ tasks, mix of independent and dependent)
- `batty work <phase> --parallel 3` completes all tasks
- Dependency order is respected: no task starts before its deps finish
- No double-claims across agents
- Merges land cleanly via the serialization queue
- `--parallel 1` behaves identically to current single-agent mode (regression check)

## Edge Case Tests

- Board with circular deps: fails immediately with clear error naming the cycle
- Deadlock scenario: all remaining tasks blocked -> reports deadlock and exits
- Agent crash mid-task: task released, another agent picks it up
- Empty board: exits cleanly with no errors

## Exit Criteria

- `batty work <phase> --parallel 3` on a board with 8+ tasks succeeds
- DAG ordering respected in every run
- Merge queue handles concurrent completions without corruption
- All existing tests pass (no regressions from phases 1-3)
- `cargo install batty-cli` works from crates.io (or `--dry-run` passes)
- `batty completions zsh` produces valid completions

[[2026-02-23]] Mon 01:23
Completed phase-4 integration/exit validation: added synthetic 8-task DAG progression test, cycle fast-fail test in parallel entrypoint, scheduler edge-case tests for deadlock/crash release/empty board/no-double-dispatch, and merge-queue conflict/test-gate coverage. Verified full test suite (`cargo test -q`), completions generation (`batty completions zsh`), local install (`cargo install --path . --force --locked`), and publishability (`cargo publish --dry-run --allow-dirty`). Produced final review artifact `phase-summary.md` at repository root with tasks, files, tests, decisions, deferred items, and follow-up watch points.
