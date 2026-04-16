# Inbox Control-Plane Design: Tiered Message Queues

**Ticket:** #658
**Status:** Design proposal
**Date:** 2026-04-16

## Problem

During the nether_earth_remake run, the shared inbox accumulated 623+ stale
messages. Triage alerts, task assignments, completion reports, and status updates
all shared one Maildir queue per agent. Agents got overwhelmed — context windows
filled with stale noise and planning cycles returned `created=0`.

The tactical fix (#650) added digest-policy compression, but the structural
problem remains: all message types share one queue with one TTL.

## Current Flow

### Message Types (cataloged from source)

**Work Orders** (task assignments, rework):
- `InboxMessage::new_assign()` — `messaging.rs:169-193`
- Aged task checkpoint requests — `automation.rs:1639-1645`
- Auto-unblocked task notifications — `automation.rs:1839-1860`
- Owned task interventions — `interventions/owned_tasks.rs:130+`

**Telemetry** (status, nudges, heartbeats):
- Review nudges — `automation.rs:1512-1518`
- Idle nudges — `interventions/nudge.rs:68-70`
- Pipeline running dry — `automation.rs:1997`
- Recovery/resolved notices — `supervisory_notice.rs:171-200`

**Escalations** (blockers, human directives):
- Branch/task mismatch alerts — `automation.rs:336-345`
- Review timeout escalation — `automation.rs:1487-1495`
- Stuck task escalation — `interventions/owned_tasks.rs:82-108`
- Dispatch/utilization recovery — `interventions/dispatch.rs:461+`, `utilization.rs:266+`
- Review/triage backlog — `interventions/review.rs:224+`, `triage.rs:136-140`

**Content** (completion reports, standups):
- Completion packets — `completion.rs:109-110` (JSON with branch, tests, outcome)
- Manual review notices — `messaging.rs:1083-1103`
- Task ready for review — `inbox.rs:459-468`
- Generic messages — `messaging.rs:75-111`

### Current Routing

All messages flow through `queue_daemon_message()` (`delivery/routing.rs:679-686`)
into a single Maildir per agent at `.batty/inboxes/<member>/new/`. The
`MessageCategory` enum (`inbox.rs:411-481`) classifies at read time:

```
Escalation(0) > ReviewRequest(1) > Blocker(2) > Status(3) > Nudge(4)
```

Digestion collapses nudges by sender and status updates by task ID, but all
categories share the same queue, the same `pending_queue_max_age_secs` (600s),
and the same delivery retry logic.

### Existing Protections

- **Digestion** (`inbox.rs:501-569`): 12 raw messages -> ~5 digest entries
- **Deduplication** (`inbox.rs:150-176`): signature-based within time window
- **Intervention cooldowns** (`daemon.rs:196`): `HashMap<String, Instant>`
- **Stale escalation demotion** (`inbox.rs:609-645`): done-task escalations -> Status

## Proposed Design: Tiered Queues

### Queue Tiers

| Tier | Category | TTL | Rate Limit | Digest |
|------|----------|-----|------------|--------|
| `priority` | Escalations, blockers | 1 hour | None (always deliver) | Never collapse |
| `work` | Task assignments, rework, reviews | 30 min | 1 per task per 5 min | Collapse by task ID |
| `content` | Completions, standups, status | 15 min | 1 per task per 10 min | Keep latest per task |
| `telemetry` | Nudges, heartbeats, recovery | 5 min | 1 per type per 10 min | Collapse by sender + type |

### Storage Layout

```
.batty/inboxes/<member>/
  priority/new/    # escalations, blockers
  priority/cur/
  work/new/        # assignments, rework, reviews
  work/cur/
  content/new/     # completions, standups
  content/cur/
  telemetry/new/   # nudges, heartbeats
  telemetry/cur/
```

Each sub-queue is a standard Maildir. The existing `maildir` crate works
unchanged — we just open four Maildirs per agent instead of one.

### Classification at Write Time

Move classification from read-time (`digest_messages`) to write-time
(`deliver_to_inbox`). The `MessageCategory` enum already exists; extend it
to map to queue tiers:

```rust
impl MessageCategory {
    pub fn queue_tier(&self) -> &'static str {
        match self {
            Self::Escalation | Self::Blocker => "priority",
            Self::ReviewRequest => "work",
            Self::Status => "content",
            Self::Nudge => "telemetry",
        }
    }
}
```

`InboxMessage` gains an optional `category: Option<MessageCategory>` field
set by the sender. When absent, `classify_message()` runs at write time.

### Per-Queue TTL Expiry

Replace the single `expire_stale_pending_messages` with per-queue expiry:

```rust
pub fn expire_tiered_queues(
    inboxes_root: &Path,
    member: &str,
    ttls: &TieredTtlConfig,
) -> Result<ExpiredCounts> {
    // Each tier has its own max_age
}
```

Default TTLs from the table above, configurable via `workflow_policy`.

### Agent Prompt Integration

Agent inbox injection changes from "here are your N pending messages" to:

```
[PRIORITY — 2 messages]
  1. Branch mismatch: eng-1-2 on wrong branch for #675
  2. Stuck task escalation: eng-1-1 idle on #658 for 45m

[WORK — 1 message]
  1. Task #677: implement tiered inbox queues

[CONTENT — 0 messages]

[TELEMETRY — suppressed (3 expired)]
```

Telemetry is shown as a count only; full bodies are available on request.
This keeps the agent's context window focused on actionable items.

### Migration Path (backwards compatible)

1. **Phase 1**: Add `queue_tier()` to `MessageCategory`; write to tiered
   directories; read from both old and new locations. Feature-flagged via
   `workflow_policy.tiered_inboxes: bool` (default `false`).

2. **Phase 2**: Enable by default. Old flat inbox is still read but not
   written to. `expire_stale_pending_messages` runs on both layouts.

3. **Phase 3**: Remove flat inbox support. Clean up old directories.

## Test Plan

- Unit: `classify_message` maps all known message patterns to correct tiers
- Unit: per-queue TTL expiry respects tier-specific max_age
- Unit: digest output groups by tier with correct headers
- Integration: agent receives tiered prompt with priority messages first
- Integration: telemetry messages expire within 5 min while work orders persist
- Regression: existing `digest_messages` tests still pass during Phase 1

## Follow-up Implementation Tickets

1. **Add `queue_tier()` to MessageCategory + write-time classification**
   — Modify `deliver_to_inbox` to route by tier. ~100 lines.

2. **Per-queue TTL expiry with `TieredTtlConfig`**
   — Replace single expiry with per-tier sweep. ~80 lines.

3. **Tiered digest formatting for agent prompts**
   — Group messages by tier in injection, suppress telemetry bodies. ~120 lines.

4. **Feature flag + migration scaffolding**
   — `workflow_policy.tiered_inboxes`, dual-read from old+new layout. ~60 lines.

5. **Per-queue rate limiting**
   — Dedup by (sender, tier, task_id) with configurable windows. ~80 lines.
