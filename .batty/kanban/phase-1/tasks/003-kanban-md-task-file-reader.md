---
id: 3
title: kanban-md task file reader
status: done
priority: critical
created: 2026-02-21T18:40:22.885984667-05:00
updated: 2026-02-21T18:54:52.722119487-05:00
started: 2026-02-21T18:53:25.642535063-05:00
completed: 2026-02-21T18:54:52.722119157-05:00
tags:
    - core
depends_on:
    - 1
class: standard
---

Read task files from kanban/phase-N/tasks/ directory. Parse YAML frontmatter (id, title, status, priority, tags, depends_on). Parse Markdown body for task description. Parse optional '## Batty Config' section for per-task overrides (agent, policy, dod, max_retries).
