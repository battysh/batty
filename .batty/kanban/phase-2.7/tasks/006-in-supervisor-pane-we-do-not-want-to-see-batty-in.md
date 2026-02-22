---
id: 6
title: In supervisor pane we do not want to see [batty] in the stream
status: done
priority: medium
created: 2026-02-22T00:57:07.134345373-05:00
updated: 2026-02-22T16:11:51.622515025-05:00
started: 2026-02-22T13:22:07.705995296-05:00
completed: 2026-02-22T16:11:51.590475398-05:00
class: standard
---

[[2026-02-22]] Sun 16:06
Removed [batty] prefix from orchestrator log stream output path (LogFileObserver) and updated parser fixture to match uncluttered supervisor pane lines. Running test suite next.

[[2026-02-22]] Sun 16:11
## Statement of Work

- **What was done:** Removed the `[batty]` prefix from orchestrator log stream lines so the supervisor pane shows cleaner, less noisy output while retaining existing event semantics.
- **Files created:** None.
- **Files modified:** `src/orchestrator.rs` - updated `LogFileObserver` output formatting for auto-answer/escalation/suggestion/event lines; updated `parse_last_auto_prompt_reads_latest_entry` test fixture to match unprefixed stream lines.
- **Key decisions:** Kept `[batty]` in tmux status/title surfaces and CLI-facing status messages, but removed it specifically from the stream tailed in the supervisor pane to address operator readability without changing event parsing behavior.
- **How to verify:** Run `cargo test log_file_observer_writes`, `cargo test parse_last_auto_prompt_reads_latest_entry`, and `cargo test`.
- **Open issues:** None.
