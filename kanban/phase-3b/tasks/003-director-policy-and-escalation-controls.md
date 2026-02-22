---
id: 3
title: Director policy and escalation controls
status: backlog
priority: high
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
