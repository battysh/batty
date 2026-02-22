---
id: 6
title: Persist Tier2 intervention context snapshots
status: done
priority: medium
created: 2026-02-22T00:39:08.885075543-05:00
updated: 2026-02-22T00:49:47.483769589-05:00
started: 2026-02-22T00:44:40.416672032-05:00
completed: 2026-02-22T00:49:47.483769208-05:00
tags:
    - supervisor
    - logging
    - debug
claimed_by: cape-staff
claimed_at: 2026-02-22T00:49:47.483769539-05:00
class: standard
---

Add structured logging artifacts for Tier 2 intervention context so we can inspect exactly what the supervisor received per call.

## Requirements

1. For each Tier 2 intervention, write a context snapshot file under the run log directory (for example `.batty/logs/<run>/tier2-context-<n>.md`).
2. Include metadata linking snapshot to orchestrator events (timestamp, session, prompt kind, context length).
3. Keep orchestrator log lines concise while linking to snapshot paths.
4. Add safety guardrails for sensitive content (document redaction strategy or explicit opt-in behavior).
5. Ensure behavior is deterministic and does not break existing supervision flow.

## Verification

1. Trigger at least one Tier 2 intervention and confirm snapshot file is written.
2. Orchestrator log includes path/reference to the snapshot.
3. `cargo test` passes with tests for snapshot write behavior.

[[2026-02-22]] Sun 00:49
Implemented Tier2 context snapshots under run logs with sequential naming, metadata headers (timestamp/session/prompt kind/context length), concise orchestrator snapshot references, and deterministic secret-line redaction guardrails.

## Statement of Work

- **What was done:** Added persistent Tier2 context snapshot artifacts with metadata and redaction, plus orchestrator linkage and test coverage.
- **Files created:** `kanban/phase-2.7/tasks/006-persist-tier2-intervention-context-snapshots.md` - task artifact with notes and statement of work.
- **Files modified:** `src/orchestrator.rs` - added snapshot writer, sequential index detection, deterministic redaction, snapshot event references, and snapshot tests; `README.md` - documented snapshot path behavior and redaction strategy.
- **Key decisions:** Snapshot writes are best-effort/non-fatal to supervision flow, while redaction is deterministic keyword-based line masking to reduce secret leakage risk in persisted debug artifacts.
- **How to verify:** Trigger Tier2 fallback and confirm `.batty/logs/<run>/tier2-context-<n>.md` is written and referenced in orchestrator log; run `cargo test`.
- **Open issues:** None.
