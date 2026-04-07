# Discord Interface Strategy — PRD

## Vision

Discord replaces tmux as the primary human interface for batty. tmux is the agent runtime — what agents see. Discord is where humans monitor, control, and direct the team. The goal: **type `$go` on your phone, go to sleep, wake up to merged features, and read the story of what happened in Discord.**

This is the claw-code philosophy applied to batty: "A person can type a sentence from a phone, walk away, sleep, or do something else. The agents read the directive, break it into tasks, assign roles, write code, run tests, argue over failures, recover, and push when the work passes." The human never opens a terminal. The terminal sessions belong to the agents. The human's interface is Discord.

Every Discord message should be interesting to read. If a human wouldn't care about an event, it shouldn't appear. If it does appear, it should answer: "what happened and why should I care?"

## Learnings from the claw-code ecosystem

### The clawhip pattern (most important architectural insight)
All monitoring stays OUTSIDE the agent's context window. The agent focuses on code. A separate daemon watches git commits, tmux sessions, agent lifecycle, and GitHub events — then routes formatted notifications to Discord. The agent never knows about Discord. This is why batty's discord_bridge reads from events.jsonl rather than injecting into agents.

### What made claw-code's Discord UX work
- The human interface is the simplest possible thing (a chat text box)
- Complexity lives in the coordination layer the human never sees
- Only signal reaches Discord: "session.started", "session.blocked", "session.finished"
- @mentions ONLY for escalations — not for routine events
- Batch windowing prevents notification storms (5s for routine, 300s for CI)
- Three tiers of verbosity: `minimal` (one-liner), `session` (operational context), `verbose` (risk framing)

### OmX patterns that inform our UX
- `$ralph` persistent loops: human only needs to see START and END, not every intermediate step
- `$team` parallel workers: show which workers are active and what they're working on, NOT every code action
- The canonical workflow ($deep-interview → $ralplan → $team/$ralph) maps to: directive → clarify → plan → execute → done
- The plan approval step is the key human-in-the-loop moment

### What Ralphthon proved
Participants who designed agent systems and stepped back shipped more than those who coded manually all night. The bottleneck is not typing speed — it's architectural clarity, task decomposition, and system design.

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
- **#commands** — conversation. Human talks to architect. Escalations interrupt here. Feels like chatting with a project lead. This is the ONLY channel that should ping the human's phone.
- **#events** — newsfeed. Task lifecycle, merges, board changes. Scannable from phone. Feels like a GitHub activity feed. Never pings.
- **#agents** — health monitor. Agent spawns, stalls, restarts, context exhaustion. Feels like a Datadog dashboard. Never pings unless unhealthy.
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

### 5. @mention policy (from clawhip)
Only @mention the human for:
- **Escalations**: agent blocked, needs human decision
- **Failures**: tests broken on main, merge conflicts needing manual fix
- **Session completion**: "all tasks done, ready for your review"

NEVER @mention for: routine commits, task assignments, agent restarts, progress updates, board changes. The phone should only buzz when the human actually needs to act.

### 6. Batch windowing (from clawhip)
- Routine events (commits, assignments, claim changes): buffer 5 seconds, batch into one message
- CI/test events: buffer 30 seconds (tests often produce rapid-fire results)
- Critical events (failures, escalations, stalls): bypass all buffering, deliver immediately

### 7. Verbosity tiers (from OmX OpenClaw integration)
- **Compact** (default): one-line summary, key metadata. Used for routine events in #events.
- **Alert**: same content with ⚠️ prefix and @mention. Used for failures and escalations in #commands.
- **Verbose**: full context with task spec, error details, suggested actions. Used on-demand ($status, $board).

### 8. Show vs Hide
- **Show**: event type, affected task/agent, one-line summary, timestamp
- **Hide (behind ||spoiler||)**: full task spec, commit diff, test output, error stacktrace
- **Never show**: heartbeats, routing internals, claim extensions, worktree refreshes (unless failed)

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
- [x] Rate limit prevention (skip backlog on startup, 5-event batch limit)
- [ ] @mention policy: only ping #commands for escalations/failures
- [ ] Batch windowing: 5s routine, 30s CI, immediate for critical
- [ ] Merge events with commit summary and line counts
- [ ] Test result events with pass/fail counts
- [ ] "Catching up" indicator when processing event backlog
- [ ] Compact one-liner format as default (phone-scannable)

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
- [ ] Thread-per-task in #events (group all events for one task — OpenClaw pattern: channel=project, thread=task)
- [ ] Reaction controls (👍 to approve merge, 🔄 to retry)
- [ ] Separate notification bot identity (clawhip pattern — notification bot separate from command bot, so rate limits don't affect commands)
- [ ] Keyword-triggered alerts from agent PTY output (clawhip tmux.keyword pattern — watch for "error", "FAILED", "complete" in agent panes)
- [ ] Webhook mode (no bot needed, outbound only)
- [ ] Multiple team support (one Discord server, multiple project channels — OpenClaw per-channel isolation)
- [ ] Cron follow-up pattern (periodic standup summaries posted to #events, driven by daemon standup mechanism)
- [ ] Auto-detect directive type: "simple question" vs "development task" (OpenClaw claw-conductor pattern)

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
