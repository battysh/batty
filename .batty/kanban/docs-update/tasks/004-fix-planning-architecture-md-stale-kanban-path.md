---
id: 4
title: Fix planning/architecture.md stale kanban path
status: done
priority: medium
created: 2026-02-22T14:45:50.050678982-05:00
updated: 2026-02-22T14:53:41.007213518-05:00
started: 2026-02-22T14:53:15.38904987-05:00
completed: 2026-02-22T14:53:31.392008311-05:00
tags:
    - docs
    - planning
class: standard
---

## Problem

The Data Flow section references the old kanban path:
```
kanban/phase-1/                    ← phase board
```

Should be:
```
.batty/kanban/phase-1/             ← phase board
```

The project consolidated everything under .batty/ — architecture.md still references the old location.

## Acceptance Criteria

- All path references use .batty/kanban/ instead of kanban/

[[2026-02-22]] Sun 14:53
Replaced stale Data Flow path  with  to match current board location.

[[2026-02-22]] Sun 14:53
Corrected path in planning/architecture.md Data Flow block from kanban/phase-1/ to .batty/kanban/phase-1/.
