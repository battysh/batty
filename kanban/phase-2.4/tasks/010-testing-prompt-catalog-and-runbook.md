---
id: 10
title: Testing prompt catalog and runbook
status: backlog
priority: high
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - docs
  - testing
class: standard
---

Publish prompt catalog and execution runbook for harness validation.

## Required prompt catalog

1. **Token echo:** `Type exactly TOKEN_<AGENT>_123 and press Enter`
2. **Enter-only:** `Press enter to continue`
3. **Escalation ambiguity:** `Choose between A/B without context`
4. **Non-injectable stress:** long-paragraph supervisor response

## Runbook requirements

1. How to run deterministic tmux harness suite.
2. How to run opt-in real supervisor tests.
3. How to run opt-in real executor+supervisor smoke tests.
4. What logs to inspect and expected pass/fail signals.

## Done When

- Runbook is in repo and linked from phase docs.
- Team can execute validation process repeatably.
