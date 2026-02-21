---
id: 1
title: batty work all — sequential board runner
status: backlog
priority: critical
created: 2026-02-21T18:40:33.967096417-05:00
updated: 2026-02-21T18:40:33.967096417-05:00
tags:
    - core
class: standard
---

Loop: pick highest-priority unblocked task → batty work <id> → next. Respects depends_on graph (topological sort). Priority ordering: critical > high > medium > low. Stop conditions: board empty, user Ctrl-C, error threshold. Skip tasks that fail repeatedly.
