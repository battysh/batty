# Phase 3B: AI Director Review

**Status:** Not Started
**Board:** `kanban/phase-3b/`
**Depends on:** Phase 3A complete

## Goal

Add AI director automation as an upgrade on top of the already-working sequencer and human review gate.

## Tasks (5 total)

1. **Director review agent** — evaluate diff + phase summary + logs and return merge/rework/escalate.
2. **Director rework orchestration** — feed director feedback back into executor loop automatically.
3. **Director policy and escalation controls** — apply autonomy tiers to director decisions.
4. **Director decision audit trail** — persist rationale, confidence, and outcomes.
5. **Phase 3B exit criteria** — director path works with safe human override.

## Exit Criteria

- Director can review completed phases and emit structured decisions.
- Rework loop can run from director feedback with retry limits.
- Human can override any director decision.
- All director actions are visible in logs and review artifacts.
