---
id: 6
title: batty work all — phase sequencer
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:14:12.266359079-05:00
started: 2026-02-22T15:13:43.485958648-05:00
completed: 2026-02-22T15:14:12.266358751-05:00
tags:
    - core
depends_on:
    - 1
    - 2
    - 3
    - 4
    - 5
class: standard
---

Loop: find next incomplete phase → `batty work <phase>` → review gate → merge → next phase. Phase ordering follows directory structure (phase-1, phase-2, ...). A phase is complete when all tasks are done.

Stop conditions: all phases complete, user Ctrl-C, error threshold. If a phase fails review repeatedly, pause and report to human.

[[2026-02-22]] Sun 15:13
Implemented 2026-02-22T20:13:49.114989Z  INFO loaded config from /home/zedmor/dev/batty/.batty/worktrees/phase-3-run-001/.batty/config.toml sequencer flow. Main command routing now treats target  specially, runs foreground by default (no auto-detach), and currently rejects --parallel > 1 with explicit phase-4 message. Added run_all_phases orchestration: discover numeric incomplete phases via sequencer module, log phase-selection decisions, iterate phases in order, run each through existing run pipeline (including review/rework/merge), and stop on first failure by default (optional continue policy via BATTY_CONTINUE_ON_FAILURE=true). Added dry-run sequencer tests and validated sequencing behavior with ordered phase boards. Validation: cargo test work::tests:: ; cargo test review::tests:: ; cargo test sequencer::tests:: ; cargo test log::tests::all_event_types_serialize. Full cargo test still blocked by tmux/openpty sandbox permissions.

[[2026-02-22]] Sun 15:14
Correction: implemented batty work all sequencer flow with special command routing in main.rs and run_all_phases orchestration in work.rs. This includes deterministic discovery/ordering, skip-complete behavior, sequencer decision logging, fail-fast default policy, optional continue-on-failure policy, and dry-run sequencing tests.
