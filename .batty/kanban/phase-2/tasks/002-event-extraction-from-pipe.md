---
id: 2
title: Event extraction from piped output
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:30:08.367826861-05:00
started: 2026-02-21T20:27:50.760247779-05:00
completed: 2026-02-21T20:30:08.36782637-05:00
tags:
    - core
depends_on:
    - 1
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:30:08.367826801-05:00
class: standard
---

Read piped output from tmux `pipe-pane` log file and extract structured events using regex.

## Events to extract

- `task_started` — executor picked a new task
- `file_created` / `file_modified` — executor created or edited a file
- `command_ran` — executor ran a command (with pass/fail result)
- `test_ran` — test execution (pass/fail, count)
- `prompt_detected` — executor is asking a question
- `task_completed` — executor marked a task done
- `commit_made` — executor committed

## Implementation

- Read from the pipe-pane log file using file watching (inotify on Linux, kqueue on macOS, or poll as fallback).
- Strip ANSI escape sequences from piped output before regex matching.
- Rolling event buffer — configurable size (default: last 50 events).
- Events stored as structured data, not raw text.
- Full raw output stays in the log file for audit — the event buffer is a compact summary.

This is the foundation for the supervisor — it turns raw terminal output into a structured context window.

[[2026-02-21]] Sat 20:30
Statement of work:
- Created src/events.rs: event extraction from piped tmux output
- EventPatterns: regex-based classification of executor output lines into structured events
- PipeEvent enum: TaskStarted, FileCreated, FileModified, CommandRan, TestRan, PromptDetected, TaskCompleted, CommitMade, OutputLine
- EventBuffer: thread-safe rolling buffer (configurable size, default 50) with format_summary()
- PipeWatcher: polls pipe-pane log file, tracks file position, strips ANSI, extracts events
- run_watcher_loop: threaded polling loop with stop flag
- 24 unit tests covering patterns, buffer, watcher, serialization, thread safety
