---
id: 5
title: Fix conflicting claim identities in parallel mode launch context
status: backlog
priority: high
tags: [bug, parallel]
---

## Bug Description

In `batty work <phase> --parallel 2 --dry-run`, the launch context contains contradictory claim identity instructions. The slot header says:

```
slot.claim_agent_name: forge-path
Use claim.agent_name = forge-path for all kanban-md --claim operations
```

But further down in the same launch context:

```
claim.agent_name: spray-lodge
claim.source: persisted
Use this exact claim agent name for all `kanban-md ... --claim` commands
```

The agent receives two different claim identities and conflicting instructions about which to use. This could cause claim conflicts in kanban-md.

## Root Cause

The parallel path generates per-slot claim identities but the base launch context also injects the persisted/generated claim identity from the single-agent path. Both appear in the final prompt.

## Fix Approach

In the parallel slot context composition, the per-slot claim identity should be the authoritative one. Either:
1. Remove the base `claim.agent_name` section when composing parallel slot contexts
2. Override the base claim with the slot-specific claim in the context

## Files to Modify

- `src/work.rs` — parallel launch context composition (around `run_phase_parallel`)

## How to Verify

1. `batty work <phase> --parallel 2 --dry-run`
2. Each slot's launch context should have ONE consistent claim identity
3. No conflicting claim_agent_name instructions in the same context
