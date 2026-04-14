# Batty as a Factory of Features — Gap Analysis

Analysis from the nether_earth_remake overseer session (2026-04-12 → 2026-04-14).

## Observed metrics

| Pattern | Count | Implication |
|---------|-------|-------------|
| Auto-merges | 52 | Happy path works |
| `conflicts with main` | **35** | 40% of merge attempts conflict |
| `apply_patch verification failed` | **97** | Engineers fight the edit tool constantly |
| Working-state recoveries (v0.11.5) | 39 | Fix firing but underlying cause remains |
| Stale inbox expiries | **623** | Inbox is the bottleneck |
| Manual overseer interventions | ~60 | Not yet autonomous |

## Gap taxonomy

Factory = predictable throughput, minimal human intervention, self-observing, graceful degradation, continuous recovery.

### 1. Work collision (conflicts are systemic, not accidental)

Multiple engineers touched `src/app.rs` (1000+ lines) concurrently → 35 merge conflicts. A factory doesn't let two workstations sand the same spot. **Fix:** file-level WIP locks + rebase-on-dispatch so branches never drift from main while in flight.

Tickets: #647 (existing), **new: file-level partition lock**.

### 2. Productivity vs coordination gap

Engineers produce completion signals without writing code. The dispatcher treats "completed with 750 bytes response" same as "committed 300 lines". A factory measures output (tree diff), not motion.

Tickets: #648 (existing) + **new: productivity-weighted dispatch scoring**.

### 3. No retry budget for transient failures

97 patch verification failures = 97 wasted cycles. No exponential backoff, no max-retry, no switch-to-alternative-strategy. A factory has retry budgets per task with escalation.

**New ticket: retry budget + escalation policy**.

### 4. Main branch has no safety net

After 10 merges, `src/app.rs` grew concurrent edits that broke compiling for some engineers. Nobody runs `cargo check` on main periodically. A factory runs smoke tests on main every N minutes; if main breaks, dispatch pauses.

**New ticket: periodic main smoke test + dispatch gate**.

### 5. Inbox is not a control plane

623 stale messages expired. Triage alerts, status reports, task assignments all share one queue. A factory has separate control planes: (a) work orders, (b) status telemetry, (c) human escalations. Content shouldn't fight for attention with control signals.

Tickets: #650 (existing) + **new: inbox channel separation**.

### 6. Board depth has no target

Architect creates tasks when triggered. Sometimes creates 6, sometimes 0. No continuous "maintain 4-8 todo tasks" policy. A factory has inventory management: production rate tracked, replenishment triggered when below threshold.

**New ticket: continuous board-depth maintenance with rate-based replenishment**.

### 7. No per-engineer productivity budget

eng-1-3 stuck for hours while eng-1-1 delivered 10 commits. Dispatch scoring is based on tag/file history but not on recent productivity. A factory would route work away from stalled workers and flag them for reset.

**New ticket: productivity-aware dispatch + auto-quarantine**.

### 8. No feature completion tracking

78 commits landed — but which features are "done"? Authentic controller? Direct control? Phase 3 complete? No epic-level tracking. A factory has bill of materials: parent features with child tasks, percent-complete surfaces at standup.

**New ticket: epic/feature tracking with BOM**.

### 9. Review bottleneck = single point of failure

Manager alone handles all manual reviews. When manager is busy/stuck, reviews pile up. A factory has parallel QA stations or conditional review routing (e.g. trivial changes auto-approve, complex changes go to manager).

**New ticket: tiered review routing**.

### 10. No learnings feedback loop

`learnings/` exists in batty config but isn't wired into dispatch. Every time app.rs conflicts, same fix. A factory learns: "app.rs concurrent edits always conflict → serialize tasks touching it".

**New ticket: learnings → dispatch influence**.

### 11. No daily summary / state of the line

Team runs overnight. Human has no idea what happened unless they watch logs. A factory prints shift reports: commits landed, bugs introduced, stations down, top blockers.

**New ticket: daily shift report generator**.

### 12. No blast radius detection

One bad merge could break main. Dirty worktree prevented startup for 20+ minutes during the run. A factory detects "main broken by commit X" and rolls back automatically or gates further dispatch.

**New ticket: broken-main detection + auto-rollback**.

### 13. Engineer homogeneity limits throughput

3 identical codex engineers. Some tasks are pure refactors (low conflict risk), some are feature additions (high risk). No routing by task type. A factory has specialization — bench, line, assembly stations with different tooling.

**New ticket: role-specialized engineers (claude vs codex, test-writer vs feature-writer)**.

### 14. No cost/time accounting

Tasks run indefinitely. No "this task has taken 3x estimated, escalate" gate. A factory has SLO per step.

**New ticket: per-task SLO enforcement**.

### 15. Agent session recycling

Shim sessions accumulate context and eventually hallucinate (looking at /tmp paths that don't exist). A factory cycles tools periodically to prevent drift — every N completions, spawn a fresh session.

**New ticket: scheduled shim session recycling**.

## Summary: the 5 highest-leverage improvements

If you could only ship 5 things, in order of impact:

1. **Rebase-on-dispatch** (subsumes half of #647) — prevents 80% of conflicts before they happen.
2. **Productivity-weighted dispatch** (#648 + new) — stops wasting cycles on stalled engineers.
3. **Periodic main smoke test + dispatch gate** — catches broken main in <5min instead of hours.
4. **File-level partition lock** — prevents 2 engineers from fighting over `src/app.rs`.
5. **Inbox control plane separation** (#650 + new) — decouples signaling from content to stop the 623-message flood.

Everything else is quality-of-life. These five would shift batty from "supervised prototype" to "mostly autonomous factory".
