---
id: 8
title: Add docs/reference/modules.md source code reference
status: done
priority: low
created: 2026-02-22T14:46:17.433965277-05:00
updated: 2026-02-22T14:57:10.156252524-05:00
started: 2026-02-22T14:56:13.196310687-05:00
completed: 2026-02-22T14:57:10.156252131-05:00
tags:
    - docs
    - reference
class: standard
---

## Problem

There is no source code reference for contributors. Someone looking at the repo for the first time has to read every file to understand how the pieces fit together.

## What to Create

A new docs/reference/modules.md that serves as a developer reference:

1. **Module index** — every src/ file with one-line purpose
2. **Key traits and types** — AgentAdapter, PromptDetector, OrchestratorConfig, PolicyDecision, etc.
3. **Test coverage summary** — 319+ tests, what each module tests
4. **How to add a new agent adapter** — brief guide referencing the AgentAdapter trait in src/agent/mod.rs

## Acceptance Criteria

- New file created at docs/reference/modules.md
- Covers all modules in src/
- Accurate trait/type descriptions matching actual code
- Useful for a new contributor

[[2026-02-22]] Sun 14:57
Created docs/reference/modules.md with full src module index, key traits/types, test-coverage snapshot (323 tests), and a step-by-step guide for adding new agent adapters. Added page to mkdocs nav under Reference.
