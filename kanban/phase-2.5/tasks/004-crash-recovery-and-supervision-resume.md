---
id: 4
title: Crash recovery and supervision resume
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T23:24:45.769260629-05:00
started: 2026-02-21T23:13:16.787967241-05:00
completed: 2026-02-21T23:24:45.769260289-05:00
tags:
    - core
    - reliability
claimed_by: oaken-south
claimed_at: 2026-02-21T23:24:45.769260579-05:00
class: standard
---

If Batty crashes or is terminated mid-phase, tmux keeps running. Batty must reconnect and resume supervision safely.

## Requirements

1. Add `batty resume <phase|session>` command (or equivalent behavior in `batty work`).
2. Reconnect to active tmux session and pane ids.
3. Resume pipe-pane reading from last known offset.
4. Rebuild detector/supervisor state from logs and recent pane output.
5. Continue status bar and orchestrator log updates.
6. Refuse unsafe duplicate supervisor attachments.

## Validation

- Kill Batty process during active phase run.
- Restart Batty and resume supervision.
- Verify no prompt double-injection and no lost logs.

This is a Phase 2.x requirement, not a polish-only feature.

## Statement of Work

- **What was done:** Added explicit resume flow (`batty resume <phase|session>`) and implemented crash-safe supervision resumption for active tmux runs.
- **Files created:** None.
- **Files modified:** `src/cli.rs` - new `resume` subcommand; `src/main.rs` - command dispatch; `src/work.rs` - resume orchestration entrypoint and session/worktree/log resolution; `src/orchestrator.rs` - resume mode, supervision state persistence, lock-based duplicate attach guard, detector/event-buffer rebuild from logs + pane tail; `src/events.rs` - resume offset support and checkpoint offsets; `src/detector.rs` - detector state seeding helper; `src/tmux.rs` - safe `pipe-pane -o` helper; `README.md` - user docs for resume command.
- **Key decisions:** Persisted watcher checkpoint offset as a resume-safe value that rewinds partial buffered lines; used a PID lock lease to refuse unsafe duplicate supervisor attachments while allowing stale-lock recovery after crashes.
- **How to verify:** `cargo test -q` (full suite, includes tmux/PTY tests), plus focused checks: `cargo test -q completion::tests`, `cargo test -q orchestrator::tests::supervision_state_roundtrip`, `cargo test -q events::tests::watcher_resume_from_position_reads_only_new_content`.
- **Open issues:** Manual kill/restart smoke run via `batty work`/`batty resume` should still be done in a real interactive shell session as final operator validation.
