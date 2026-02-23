---
id: 1
title: Build task dependency DAG from kanban-md board
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T00:57:55.864081574-05:00
started: 2026-02-23T00:55:35.566631449-05:00
completed: 2026-02-23T00:57:55.841237562-05:00
tags:
    - core
    - dag
class: standard
---

Parse task files in a phase board, extract `depends_on` frontmatter, and build a directed acyclic graph for scheduling parallel work.

## Requirements

- Read all task `.md` files from the board's `tasks/` directory
- Parse YAML frontmatter to extract `id`, `status`, and `depends_on` fields
- Build an adjacency list representing task dependencies
- **Cycle detection:** validate the graph is acyclic; fail with a clear error naming the cycle
- **Frontier computation:** given completed task IDs, compute the "ready set" — tasks whose deps are all satisfied and that haven't started
- **Topological sort:** produce a valid execution order (used as fallback for `--parallel 1`)
- Edge cases: tasks with no deps (always ready), deps on non-existent IDs (error), empty board

## Implementation Notes

- New module: `src/dag.rs`
- Use `depends_on` field from kanban-md task frontmatter (already supported)
- DAG is rebuilt on every scheduling tick (tasks complete -> recompute frontier); keep it fast
- Unit tests: cycle detection, frontier computation, topo sort, missing deps, empty graph

[[2026-02-23]] Mon 00:57
Implemented src/dag.rs with board task DAG parsing from tasks/, missing-dependency validation, cycle detection with explicit cycle path reporting, deterministic topological sort, and ready frontier computation for backlog/todo tasks. Added 6 unit tests (cycle, missing deps, ready set, topo sort, empty graph, and tasks-dir loading). Verified with cargo test dag:: and cargo test.
