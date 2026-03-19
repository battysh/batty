# Bug Report: Team Appears Idle While Delivered Engineer Results Sit In Lead Inboxes

## Summary

In a live multi-agent Batty session, engineers delivered concrete result packets to their leads, but the leads remained idle long enough that:

- `batty status` / `batty load` showed severe underutilization
- board state lagged behind reality
- the architect had to manually re-steer the program despite usable evidence already being present

This is not a message-delivery failure. Messages were delivered successfully. The failure mode is that delivered engineer results can sit in lead inboxes without causing prompt lead-side triage, board normalization, or fresh assignment, so the whole team looks and behaves idle.

## Why This Matters

Operationally, this is a serious throughput problem:

- engineers do work and report it
- leads do not react quickly enough
- idle time accumulates even though critical evidence is already available
- the architect has to poll inboxes manually to discover work that should already have been consumed

The result is wasted compute and a misleading view of program progress.

## Environment

- Repo: `~/batty`
- Date observed: `2026-03-19`
- Session: `batty-mafia-adversarial-research`
- Team pattern: architect -> leads -> engineers

## Observed Behavior

### 1. Engineers reported real results

Examples observed in lead inboxes:

- `red-eng-1-2 -> red-lead`
  - `Task #191 completed`
  - `Task #191 blind-path update completed`
  - `Task #178 implementation evidence`
- `black-eng-1-3 -> black-lead`
  - exact `#192` narrowed D2-D4 eval contract

These packets were visible as `delivered`, not pending or missing.

### 2. Leads still appeared idle

At the same time:

- `batty status` showed both leads idle
- most engineers were also shown idle
- `batty load` dropped to `10%` (`1 / 10 members working`)

This created the operational picture of a stalled team, even though fresh engineer evidence had already arrived.

### 3. Architect had to manually recover the loop

The architect had to:

- inspect lead inboxes directly
- discover that concrete engineer results were already present
- normalize board state manually
- re-home blocked tasks to clean slots
- push new routing back through the leads

Without that manual intervention, the team would have remained underloaded despite usable evidence already existing.

## Expected Behavior

When engineers deliver concrete result packets to a lead, Batty should make it much harder for the program to silently stall at the lead layer.

At minimum, one of these should happen:

1. the lead should be surfaced as actively needing triage
2. load/status should reflect that there is unconsumed work in the lead inbox
3. there should be an explicit “needs lead action” or “untriaged engineer result” signal
4. stale delivered engineer packets should be easy to detect without manual inbox spelunking

## Actual Behavior

Delivered engineer results can sit in lead inboxes while:

- leads appear idle
- team load appears near-zero
- the rest of the team is not automatically reloaded
- the architect must manually infer that the bottleneck is lead-side triage

## Reproduction Pattern

This was observed repeatedly under the following pattern:

1. Engineer completes a scoped task and sends result packet to lead
2. Message is marked `delivered`
3. Lead does not immediately consume / act on the packet
4. `batty status` still shows lead `idle`
5. Engineers remain or become idle because the next assignment is not issued
6. `batty load` collapses even though useful evidence already exists in the system

## Not The Root Cause

This does **not** appear to be primarily:

- a Batty send failure
- a queued-message failure
- an engineer failure to report

The messages were delivered correctly. The observed bottleneck is the lack of strong triage pressure / visibility after delivery.

## Suspected Root Cause

Batty currently seems to model “working” mostly as active agent execution, not “has delivered engineer results waiting for lead action.”

That means a lead with actionable unread/unprocessed engineer packets can still look idle in the main operator surfaces.

As a result:

- the real bottleneck is hidden
- utilization looks worse only after the fact
- the architect has to inspect inboxes manually to diagnose the stall

## Product-Level Fix Ideas

Any one of these would help a lot:

1. Add a lead-side triage signal in `batty status`
   - Example: `idle (untriaged engineer results: 2)`

2. Count delivered engineer result packets awaiting lead action as load
   - This would stop `batty load` from reporting near-zero while useful work is stuck in inboxes.

3. Add a dedicated inbox classification
   - Example: `result_pending_review`, `needs_lead_action`, or `blocks_downstream_work`

4. Add stale-result detection
   - If an engineer result packet is delivered and not followed by lead action within N minutes, raise a visible signal.

5. Add a lead dashboard / summary command
   - Show “latest engineer results requiring triage” rather than only raw message history.

## Impact On Current Session

This issue caused:

- repeated apparent team idleness
- delayed board normalization
- delayed task re-homing to clean write slots
- avoidable architect overhead in manual inbox inspection and rerouting

## Bottom Line

The main bug is not message delivery. The main bug is that Batty makes it too easy for delivered engineer results to become invisible at the lead-triage layer, which in turn makes the team look idle and behave idle until the architect manually intervenes.
