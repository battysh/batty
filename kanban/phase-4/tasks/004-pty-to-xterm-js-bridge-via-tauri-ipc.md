---
id: 4
title: PTY-to-xterm.js bridge via Tauri IPC
status: backlog
priority: critical
created: 2026-02-21T18:40:59.495885113-05:00
updated: 2026-02-21T18:40:59.495885113-05:00
tags:
    - core
depends_on:
    - 1
    - 3
class: standard
---

Connect portable-pty (Rust) to xterm.js (frontend) via Tauri IPC. PTY transport behind an abstraction trait (Tauri IPC initially, swappable to WebSocket). Reuse the same portable-pty code from batty work.
