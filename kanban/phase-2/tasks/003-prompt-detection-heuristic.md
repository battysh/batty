---
id: 3
title: Prompt detection heuristic
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:32:12.784050856-05:00
started: 2026-02-21T20:30:20.006879318-05:00
completed: 2026-02-21T20:32:12.784050386-05:00
tags:
    - core
depends_on:
    - 2
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:32:12.784050806-05:00
class: standard
---

Detect when the executor is waiting for input by monitoring the piped output.

## Heuristic

No new bytes in piped output for N seconds + last line matches a prompt pattern = executor is asking something.

## State machine

```
WORKING   →  new output arriving         → extract events, do nothing else
PAUSED    →  output stopped (N seconds)  → check if last line is a prompt
QUESTION  →  prompt detected             → trigger response (Tier 1 or Tier 2)
ANSWERING →  response injected           → wait for executor to resume
→ back to WORKING
```

## Configuration

In `.batty/config.toml`:
- `silence_timeout_secs` — how long to wait before checking for prompt (default: 3)
- `prompt_patterns` — additional regex patterns beyond built-in agent patterns
- Per-agent patterns already exist from Phase 1 (`src/prompt/mod.rs`)

## Implementation

- Integrate with the event extraction pipeline from task #2.
- Reuse Phase 1's `PromptPatterns` and `PromptKind` from `src/prompt/mod.rs`.
- The silence heuristic is new — Phase 1 only checked patterns inline.

[[2026-02-21]] Sat 20:32
Statement of work:
- Created src/detector.rs: prompt detection state machine
- SupervisorState: Working, Paused, Question, Answering
- PromptDetector: combines silence detection with regex pattern matching
- on_output(): processes new output, fires inline prompt detection
- tick(): periodic check for silence-based prompt detection
- answer_injected()/human_override(): state transition controls
- DetectorConfig: configurable silence_timeout (default 3s) and answer_cooldown (default 1s)
- 16 unit tests covering all state transitions, timing, ANSI handling
- Added PartialEq to DetectedPrompt for state comparison
