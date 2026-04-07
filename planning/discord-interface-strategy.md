# Discord Interface Strategy — PRD

## Vision

Discord replaces tmux as the primary human interface for batty. tmux is the agent runtime — what agents see. Discord is where humans monitor, control, and direct the team. The goal: **type `$go` on your phone, go to sleep, wake up to merged features, and read the story of what happened in Discord.**

Every Discord message should be interesting to read. If a human wouldn't care about an event, it shouldn't appear. If it does appear, it should answer: "what happened and why should I care?"

## Current State (v0.10.0)

### What exists
- `src/team/discord.rs` (354 lines) — HTTP API client for Discord Bot API
- `src/team/discord_bridge.rs` (425 lines) — event routing, command parser, embed builder
- Three-channel routing: `#commands`, `#events`, `#agents`
- Command parser: `$go`, `$stop`, `$status`, `$board`, `$assign`, `$merge`, `$help`, etc.
- Rich embeds with role colors (architect=blue, engineer=green, reviewer=orange)
- Noise filtering (heartbeat, message_routed, state_reconciliation hidden)
- Spoiler tags for long task specs in assignment events
- Rate limiting (5 events per sync cycle, skip backlog on startup)

### What's missing
- Plain text forwarding to architect (non-`$` messages as directives)
- Live board channel (`#batty-board` with auto-updating pinned message)
- `batty discord` CLI setup wizard
- Telegram `$` command parity
- Event content that tells a complete story (some events still dry)
- Message editing (for board channel)
- Reaction-based controls (future)

## Design Principles

### 1. Every message tells a story
Bad: `dispatch overlap skipped — src/team/task_loop.rs — Task: #560`
Good: `Task #560 can't be assigned yet — it touches the same files as in-progress #563. Conflicting: src/team/task_loop.rs`

### 2. Channels have distinct personalities
- **#commands** — conversation. Human talks to architect. Escalations interrupt here. Feels like chatting with a project lead.
- **#events** — newsfeed. Task lifecycle, merges, board changes. Scannable from phone. Feels like a GitHub activity feed.
- **#agents** — health monitor. Agent spawns, stalls, restarts, context exhaustion. Feels like a Datadog dashboard.
- **#board** — live dashboard. Single auto-updating message. Feels like a kanban board widget.

### 3. Noise is aggressively filtered
These NEVER appear in Discord:
- Daemon heartbeats
- Message routing logs
- State reconciliation
- Claim extensions/progress
- Worktree refresh (unless it fails)
- Board task archival

### 4. Discord > tmux > CLI
- Discord is for humans monitoring and directing
- tmux is for agents working (human rarely looks at it)
- CLI is for ops/debugging (batty status, batty doctor)

## Architecture

```
┌─────────────────────────────────────────────┐
│                DISCORD                       │
│                                              │
│  #commands    #events     #agents   #board   │
│  ┌────────┐  ┌────────┐  ┌──────┐  ┌─────┐  │
│  │ Human  │  │ Task   │  │Agent │  │Live │  │
│  │ ↔      │  │ life-  │  │health│  │board│  │
│  │Architect│  │ cycle  │  │      │  │     │  │
│  └───┬────┘  └───┬────┘  └──┬───┘  └──┬──┘  │
│      │           │          │         │      │
└──────┼───────────┼──────────┼─────────┼──────┘
       │           │          │         │
       ▼           ▼          ▼         ▼
┌─────────────────────────────────────────────┐
│              BATTY DAEMON                    │
│                                              │
│  Command    Event      Shim      Board       │
│  Parser     Router     Health    Snapshot     │
│  (inbound)  (outbound) (outbound)(outbound)  │
│                                              │
│  DiscordBot: send_embed, send_plain,         │
│              edit_message, poll_commands      │
└─────────────────────────────────────────────┘
```

## Implementation Plan

### Phase 1: Event Quality (IN PROGRESS)
Make every Discord message interesting to read.

- [x] Noise filtering (heartbeat, routing, reconciliation)
- [x] Role emoji titles (🏗️ Architect, 🔧 Engineer, etc.)
- [x] Task assignment with spoiler-collapsed specs
- [x] Rich descriptions for spawned, stall, escalation events
- [x] Dispatch overlap explanation with file conflicts
- [x] Auto-doctor actions with context
- [x] Verification phase with human-readable status
- [ ] Rate limit backoff with friendly "catching up" message
- [ ] Merge events with commit summary and line counts
- [ ] Test result events with pass/fail counts

### Phase 2: Bidirectional Commands (TODO — #556)
Human types in Discord, batty acts.

- [ ] Plain text forwarding to architect as directives
- [ ] `$go N` launches team with N engineers
- [ ] Command responses as embeds (not plain text)
- [ ] `$board` shows formatted board state inline
- [ ] `$status` shows formatted team health inline
- [ ] Error responses with suggested fixes

### Phase 3: Live Board Channel (BACKLOG — #562)
`#batty-board` with single auto-updating pinned message.

- [ ] `edit_message()` and `pin_message()` in DiscordBot
- [ ] Board snapshot builder (in-progress, todo, review, done-today)
- [ ] 30-second update cycle via message edit
- [ ] Agent health summary in footer
- [ ] Channel ID: `1491096253495378070`

### Phase 4: CLI Setup (TODO — #561)
`batty discord` guided setup wizard.

- [ ] Token validation against Discord API
- [ ] Guild/channel picker
- [ ] Write config to team.yaml
- [ ] Test message to verify bot permissions
- [ ] `batty discord status` for connection health

### Phase 5: Telegram Parity (TODO)
Same `$` commands in Telegram single-channel mode.

- [ ] Shared command parser (already exists as TelegramCommand)
- [ ] Formatted responses with emoji structure
- [ ] Event forwarding to Telegram (filtered same as Discord)

### Phase 6: Advanced UX (FUTURE)
- [ ] Thread-per-task in #events (group all events for one task)
- [ ] Reaction controls (👍 to approve merge, 🔄 to retry)
- [ ] Voice channel alerts for critical failures
- [ ] Webhook mode (no bot needed, outbound only)
- [ ] Multiple team support (one Discord server, multiple project channels)

## Event Taxonomy

### #events — Task Lifecycle
| Event | Title | Description Example |
|-------|-------|-------------------|
| task_assigned | 📌 Task Assigned | **eng-1-1** picked up: **Task #555: Dispatch idle-with-runnable** ||full spec|| |
| task_claim_created | ✋ Task Claimed | **eng-1-1** claimed task **#555** |
| task_escalated | 🚨 Task Escalated | **eng-1-2** escalated **#508** > inbox noise blocking all work |
| task_stale | ⏰ Task Stale | **eng-1-3** on **#507** — no progress for 2 hours |
| verification_phase_changed | 🔍 Verification | **eng-1-1** is running tests for task **#555** |
| verification_evidence_collected | ✅ Tests Passed | Task **#555** — all tests green |
| auto_doctor_action | 🩺 Auto-Doctor | Fixed **eng-1-3**'s task **#553**: reset orphaned in-progress |
| dispatch_overlap_skipped | 🔀 Overlap Skipped | Task **#560** can't be assigned — touches same files as **#563** |

### #agents — Agent Health
| Event | Title | Description Example |
|-------|-------|-------------------|
| agent_spawned | 🚀 Agent Started | **eng-1-1** is online and ready for work |
| daemon_started | 🟢 Batty Started | Team is running |
| daemon_stopped | 🔴 Batty Stopped | Team session ended |
| stall_detected | 🚧 Agent Stalled | **eng-1-2** appears stuck — no progress for 30 minutes |
| context_exhausted | 💾 Context Exhausted | **eng-1-1** hit context limit — restarting with handoff |
| narration_rejection | 🚫 Narration Rejected | **eng-1-3** tried to narrate instead of code — rejected |

### #commands — Human Attention Required
| Event | Title | Description Example |
|-------|-------|-------------------|
| *errors* | ❌ Error | Tests broken on main — 3 failures in merge/completion |
| *escalations* | 🚨 Escalation | **eng-1-2** blocked on ambiguous spec — needs human decision |
| *blocked* | 🚧 Blocked | Merge conflict on src/team/daemon.rs — manual resolution needed |

### Filtered (never shown)
daemon_heartbeat, message_routed, state_reconciliation, task_claim_extended, task_claim_progress, task_claim_warning, loop_step_error, worktree_refreshed, board_task_archived

## Success Metrics

1. **Engagement**: Human checks Discord on phone instead of SSH-ing to the machine
2. **Signal-to-noise**: Every visible message is actionable or informative
3. **Latency**: Events appear in Discord within 5 seconds of occurrence
4. **Autonomy**: Human can direct the team for an entire session without opening a terminal
5. **Recovery**: Stalls and failures are visible in Discord before the human notices

## Related Tasks
- #520 — Unified communication layer (DONE — foundation)
- #556 — Discord plain-text directive forwarding (TODO)
- #561 — `batty discord` CLI setup wizard (TODO)
- #562 — Discord live board channel (BACKLOG)

## Related Docs
- `docs/getting-started.md` — Section 9: Discord setup
- `docs/config-reference.md` — `channel: discord` config
- `planning/roadmap.md` — Discord as recommended monitoring surface
