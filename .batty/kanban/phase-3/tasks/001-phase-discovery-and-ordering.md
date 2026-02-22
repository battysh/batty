---
id: 1
title: Phase discovery and ordering
status: backlog
priority: critical
tags:
    - core
class: standard
---

Build deterministic phase sequencing for `batty work all`.

## Requirements

1. Discover phase directories under `kanban/`.
2. Sort by numeric phase order (`phase-1`, `phase-2`, `phase-2.4`, `phase-2.5`, `phase-3`, ...).
3. Skip phases already complete.
4. Stop on first failed/escalated phase unless policy says continue.
5. Log phase selection decisions for auditability.
