# Phase 2.4 Supervision Harness Runbook

This runbook defines the repeatable validation process for the phase 2.4 supervision harness.

Reference contract: `planning/supervision-harness-contract.toml`

## Prompt Catalog

1. Token echo: `Type exactly TOKEN_<AGENT>_123 and press Enter`
2. Enter-only: `Press enter to continue`
3. Escalation ambiguity: `Choose between A/B without context`
4. Non-injectable stress: long-paragraph supervisor response (must be rejected)

## Deterministic tmux Harness Suite

Run all deterministic harness checks (contract + mock scenarios in real tmux):

```sh
cargo test orchestrator::tests::harness_
```

Expected outcome:
- `harness_contract_is_machine_readable_and_complete` passes.
- Mock scenarios (`mock-direct`, `mock-enter`, `mock-escalate`, `mock-fail`, `mock-verbose`) pass.
- Real-agent tests are listed as ignored unless explicitly enabled.

## Opt-in Real Supervisor Tests (Mocked Executor)

Claude:

```sh
BATTY_TEST_REAL_CLAUDE=1 cargo test orchestrator::tests::harness_real_supervisor_claude_with_mock_executor -- --ignored --nocapture
```

Codex:

```sh
BATTY_TEST_REAL_CODEX=1 cargo test orchestrator::tests::harness_real_supervisor_codex_with_mock_executor -- --ignored --nocapture
```

Expected outcome:
- Token scenario injects `TOKEN_<AGENT>_123`.
- Enter-only scenario injects empty input (`<ENTER>` semantics).

## Opt-in Real Executor + Supervisor Smoke

Claude pair:

```sh
BATTY_TEST_REAL_E2E_CLAUDE=1 cargo test orchestrator::tests::harness_real_executor_and_supervisor_claude_smoke -- --ignored --nocapture
```

Codex pair:

```sh
BATTY_TEST_REAL_E2E_CODEX=1 cargo test orchestrator::tests::harness_real_executor_and_supervisor_codex_smoke -- --ignored --nocapture
```

Expected lifecycle signals:
1. `created`
2. `supervising`
3. `executor exited`

## Logs and Signals

When running normal `batty work` sessions, inspect:

1. `.batty/logs/orchestrator.log`
2. `.batty/logs/<phase>-pty-output.log`

Pass indicators:
1. `● supervision target pane %<id>`
2. `? supervisor call`
3. `✓ auto-answered` or explicit escalation event for non-injectable cases
4. `✓ executor exited`

Failure indicators:
1. Missing supervision target pane event
2. Injection attempted after `ESCALATE:`
3. Long prose injected instead of rejected
4. Pane-role invariant mismatch (log pane as target)
