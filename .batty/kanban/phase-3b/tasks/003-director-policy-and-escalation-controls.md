---
id: 3
title: Director policy and escalation controls
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:56:08.008697305-05:00
started: 2026-02-22T15:52:17.411042515-05:00
completed: 2026-02-22T15:56:07.985043971-05:00
tags:
    - core
    - director
depends_on:
    - 1
class: standard
---

Apply policy tiers to director actions:

- `observe`: log recommendation only
- `suggest`: prepare recommendation for human confirmation
- `act-with-approval`: require explicit human approval before merge/rework actions
- `fully-auto`: execute director decisions automatically with override hooks

Escalation triggers:
- low confidence
- policy violation
- repeated failed rework cycles

[[2026-02-22]] Sun 15:56
Implemented director policy controls: config now supports director.enabled, director.autonomy (observe/suggest/act-with-approval/fully-auto), director.program/args, and director.min_confidence. Director decisions now escalate on low confidence, fully-auto policy violations (missing confidence), and repeated failed rework cycles (max retry breach). Observe/suggest modes fall back to explicit human review; act-with-approval requires explicit approval; fully-auto supports env override hooks via BATTY_DIRECTOR_OVERRIDE.
