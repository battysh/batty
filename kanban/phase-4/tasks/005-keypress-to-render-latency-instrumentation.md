---
id: 5
title: Keypress-to-render latency instrumentation
status: backlog
priority: high
created: 2026-02-21T18:40:59.518679388-05:00
updated: 2026-02-21T18:40:59.518679388-05:00
tags:
    - perf
depends_on:
    - 4
class: standard
---

Measure latency from keypress to rendered character. Target: <20ms. Pivot trigger: if >20ms, swap PTY transport to WebSocket. Adaptive output batching (~2ms min). Frame skipping on Rust side.
