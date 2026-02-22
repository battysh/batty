---
id: 8
title: Real executor+supervisor smoke (Claude pair)
status: backlog
priority: medium
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - claude
  - smoke
class: standard
---

Run real executor + real supervisor in tmux as a smoke gate.

## Requirements

1. Use real Claude executor command in tmux.
2. Use real Claude supervisor command in Tier 2.
3. Assert orchestration lifecycle signals (`created`, `supervising`, `completed`).
4. Keep test opt-in via env flag.

## Done When

- Smoke test is runnable and stable enough for manual pre-release gating.
