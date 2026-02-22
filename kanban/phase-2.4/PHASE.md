# Phase 2.4: Supervision Harness Validation

**Status:** Next
**Board:** `kanban/phase-2.4/`
**Depends on:** Phase 2 complete

## Goal

Build a deterministic, repeatable integration-validation harness for supervisor behavior before Phase 2.5 runtime hardening.

## Why this phase exists

Recent runs exposed fragile behavior around Tier 2 supervisor interfacing, pane targeting, and prompt handling. Before adding more runtime complexity, we need hard validation gates that run in real tmux and explicitly cover:

- deterministic mock executor/supervisor scenarios
- pane persistence and target invariants
- real supervisor-agent integration with mocked executor
- real supervisor+executor smoke runs
- explicit prompt catalog for repeatable manual validation

## Tasks (10 total)

1. **Harness architecture + test contract** — define deterministic scenario matrix and expected outcomes.
2. **Deterministic mock executor fixture** — script-driven executor that emits controlled prompts and records injected input.
3. **Deterministic mock supervisor fixture** — script-driven supervisor modes: direct/enter/escalate/fail/verbose.
4. **tmux pane invariants under supervision** — enforce executor-pane targeting and persistent UI checks.
5. **Mock matrix in real tmux** — run scenario table in tmux and assert injection/escalation behavior.
6. **Real supervisor integration: Claude + mocked executor** — verify prompt→answer injection path with real Claude responses.
7. **Real supervisor integration: Codex + mocked executor** — verify prompt→answer injection path with real Codex responses.
8. **Real supervisor+executor smoke: Claude pair** — end-to-end tmux smoke test with real executor and supervisor.
9. **Real supervisor+executor smoke: Codex pair** — end-to-end tmux smoke test with real executor and supervisor.
10. **Validation prompts + runbook + exit criteria** — publish stable test prompts, env flags, and pass/fail checklist.

## Exit Criteria

- Deterministic harness suite passes in real tmux locally.
- Real supervisor integration tests are available and documented via opt-in env flags.
- Real executor+supervisor smoke tests are available and documented via opt-in env flags.
- Prompt catalog and runbook are published with expected outcomes.
- Phase 2.5 explicitly depends on completion of this validation phase.

## References

- Contract: `planning/supervision-harness-contract.toml`
- Runbook: `planning/phase-2.4-harness-runbook.md`
