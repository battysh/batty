# PRD: Goal-Directed Architecture with Console TUI

## Problem

Batty requires users to pre-configure a team topology (team.yaml), populate a board with tasks, and manage execution through tmux panes. This creates three friction points:

1. **Configuration before conversation.** Users must define roles, engineer counts, and routing rules before they can describe what they want built. The architecture forces structural decisions before the problem is understood.

2. **Static topology.** The team shape is fixed at start. A task that needs 2 engineers in phase 1 and 20 in phase 4 requires manual reconfiguration. The architect can see the need but can't act on it.

3. **tmux as the interface.** Users interact through tmux panes, `batty send` commands, and `batty status` output. There's no persistent conversation with the system. Switching between agent views, board state, and communication requires terminal multiplexer fluency.

## Solution

Replace the pre-configured-team-then-execute model with a **goal-directed conversational interface**:

1. The user starts `batty` and talks to the architect in a console chat.
2. The architect operationalizes the goal, creates the board, configures the team, and drives execution.
3. The architect dynamically reconfigures team topology (scale engineers up/down, add managers) as the work evolves.
4. A TUI console application replaces tmux as the primary interface, with views for chat, board, agent status, and agent terminal peek.
5. The system runs as a background daemon. The console can be detached and reattached. Telegram remains an alternative interface.

## Non-Goals

- Replacing the shim architecture (v0.7.0 shim is the execution layer, this builds on top of it)
- Removing tmux entirely (tmux remains available as an optional display surface for PTY logs)
- Automated human hiring / marketplace integration (future phase)
- Multi-project management (one goal per batty instance)

## Architecture

### System Layers

```
┌─────────────────────────────────────────────────┐
│  Console TUI  or  Telegram                      │ ← human interface
│  (attach/detach, 4 views)                       │
└──────────────┬──────────────────────────────────┘
               │ Unix socket (daemon ↔ console)
┌──────────────▼──────────────────────────────────┐
│  Architect Agent (shim process)                 │ ← strategist
│  - Reads goal spec                              │
│  - Runs assessment loop                         │
│  - Generates board tasks                        │
│  - Issues topology commands                     │
│  - Communicates with human via chat             │
└──────────────┬──────────────────────────────────┘
               │ batty commands + message protocol
┌──────────────▼──────────────────────────────────┐
│  Daemon (orchestrator)                          │ ← always running
│  - Manages shim lifecycle                       │
│  - Routes messages between agents               │
│  - Handles topology changes (hot-reload)        │
│  - Serves console connections                   │
│  - Persists state for resume                    │
└───┬─────┬─────┬─────┬───────────────────────────┘
    │     │     │     │
  ┌─▼─┐ ┌▼──┐ ┌▼──┐ ┌▼──┐
  │mgr│ │e-1│ │e-2│ │e-3│  ← shim processes
  └───┘ └───┘ └───┘ └───┘
```

### Startup Flow

```
$ batty

1. Daemon starts (or attaches to existing)
2. Console TUI opens
3. If fresh start:
   - Architect shim spawns with goal-directed system prompt
   - Chat view: "What would you like to build?"
   - User describes intent
   - Architect asks clarifying questions
   - Architect creates goal.yaml, board, team.yaml
   - Architect issues: batty scale engineers 3
   - Daemon spawns manager + engineer shims
   - Work begins
4. If resume:
   - Daemon resumes from saved state (team.yaml, board, goal.yaml)
   - Architect re-reads context
   - Chat view: architect summarizes what happened since last attach
   - Work continues
```

### Console TUI Views

Four views, switched by hotkey or Tab:

#### Chat View [C] (default)

Bidirectional conversation with the architect. The primary human interface.

```
┌── batty ─────────────────────────────────────────┐
│ [C]hat  [B]oard  [A]gents  [1-9]                │
├──────────────────────────────────────────────────┤
│                                                  │
│ architect> I've analyzed your goal. Here's my    │
│ plan: Phase 1 will focus on...                   │
│                                                  │
│ you> sounds good, but prioritize the UCI         │
│ protocol first — I need to test against          │
│ Stockfish early                                  │
│                                                  │
│ architect> Good call. I'll restructure the       │
│ board to put UCI in wave 1. Updating now.        │
│                                                  │
│ [system] architect scaled engineers to 3         │
│ [system] eng-1 assigned #401: UCI protocol       │
│                                                  │
│ you> _                                           │
├──────────────────────────────────────────────────┤
│ eng-1 🔴 eng-2 🟢 eng-3 🔴  │ budget: $147/$200 │
└──────────────────────────────────────────────────┘
```

Features:
- Scrollable message history
- System events inline (agent state changes, task assignments, topology changes)
- Persistent status bar showing agent states and budget
- Input line with history (up arrow for previous messages)

#### Board View [B]

Delegates to `kanban-md` TUI. The board is the shared workspace — both human and architect read and write it.

Features:
- Full kanban-md interactive board (navigate, create, edit, move tasks)
- Human can add tasks (architect sees them on next cycle)
- Human can reprioritize (move tasks between columns, reorder)
- Task detail view shows assignee, branch, commit, test results

#### Agents View [A]

Dashboard showing all running agents and their operational status.

```
┌── Agents ────────────────────────────────────────┐
│                                                  │
│ NAME       ROLE       STATE   TASK    INBOX  AGE │
│ architect  architect  🟢 idle  —       0     12m │
│ manager    manager    🔴 work  review  2      8m │
│ eng-1      engineer   🔴 work  #401    0      6m │
│ eng-2      engineer   🟢 idle  —       1      6m │
│ eng-3      engineer   🔴 work  #403    0      4m │
│                                                  │
│ Throughput: 2.1 tasks/hr  │  Review queue: 1    │
│ Budget spent: $53/$200    │  ELO: 1200 (target: │
│                           │  2800)               │
├──────────────────────────────────────────────────┤
│ [Enter] peek agent  [r] restart  [k] kill       │
└──────────────────────────────────────────────────┘
```

Features:
- Live-updating agent states (from shim events)
- Current task per agent
- Inbox count, health age
- Aggregate metrics (throughput, review queue, goal progress)
- Enter on a row to peek at that agent's terminal

#### Agent Peek View [1-9]

Live terminal output from a specific agent's shim. Read-only view of the agent's PTY output, streamed from the shim's PTY log.

```
┌── eng-1 (working on #401: UCI protocol) ─────────┐
│                                                   │
│ $ cargo test --test uci                           │
│ running 12 tests                                  │
│ test uci::parse_position ... ok                   │
│ test uci::parse_go ... ok                         │
│ test uci::bestmove_format ... FAILED              │
│                                                   │
│ ---- uci::bestmove_format stdout ----             │
│ assertion failed: expected "bestmove e2e4"        │
│ got "bestmove e2-e4"                              │
│                                                   │
│ ❯ I see the issue — the bestmove format uses      │
│   dashes but UCI expects concatenated squares...  │
│                                                   │
├───────────────────────────────────────────────────┤
│ [Esc] back to chat  [Tab] next view               │
└───────────────────────────────────────────────────┘
```

Features:
- Auto-scrolling terminal output (ANSI color preserved)
- Read-only (human observes, doesn't inject)
- Agent name, state, and current task in header
- Esc returns to previous view

### Goal Specification

Goals are stored in `.batty/goal.yaml`, created by the architect from the initial conversation:

```yaml
description: "Build a chess engine performing at ELO 2800"
requestor: "human"
budget:
  total: 200
  spent: 0
  currency: "usd"

criteria:
  - name: elo_rating
    type: automated
    command: "python eval/measure_elo.py --games 200"
    target: ">= 2800"
  - name: tests_pass
    type: automated
    command: "cargo test"
    target: "exit 0"
  - name: uci_compliant
    type: automated
    command: "python eval/uci_compliance.py"
    target: "all checks pass"
  - name: code_quality
    type: agent_review
    frequency: "each cycle"
  - name: stakeholder_approval
    type: human
    channel: "chat"
    frequency: "when major milestone reached"

constraints:
  - "Must compile with stable Rust"
  - "No external engine code (built from scratch)"
  - "UCI protocol compatible"
```

The architect creates this after the initial conversation, refines it as the work progresses, and uses it to drive the assessment loop.

### Architect Assessment Loop

The architect runs a continuous assess → strategize → decompose → execute → evaluate cycle:

```
1. ASSESS
   - Run automated evaluation commands from goal.yaml
   - Parse results, update criteria status
   - Compute gap to goal

2. STRATEGIZE
   - Given gap + history, identify highest-leverage work
   - Decide topology: how many engineers, parallel vs sequential
   - If stuck for 2+ cycles: escalate to human or request expert eval

3. DECOMPOSE
   - Generate board tasks for this cycle
   - Assign priorities and dependencies
   - Populate board via kanban-md

4. CONFIGURE
   - Issue topology commands if needed:
     batty scale engineers N
     batty scale add-manager review
   - Daemon hot-reloads team.yaml and spawns/kills shims

5. EXECUTE
   - Manager + engineers work through board tasks
   - Architect monitors via agent status events
   - Human can intervene via chat at any time

6. EVALUATE
   - Cycle completes (all current tasks done or blocked)
   - Re-run assessment
   - Log cycle metrics (tasks completed, goal progress, budget spent)
   - Return to step 1
```

### Dynamic Topology

The architect controls team shape through explicit commands that modify team.yaml. The daemon watches for changes and reconciles:

**Scale engineers:**
```
batty scale engineers 8
```
- Adds/removes engineer shim processes
- New engineers get worktrees and are available for dispatch
- Removed engineers finish current task first (graceful), or are killed (if idle)

**Add/remove managers:**
```
batty scale add-manager review-mgr
batty scale remove-manager review-mgr
```
- Supports multiple managers for review throughput

**Reconfigure routing:**
```
batty topology set parallel    # all engineers independent
batty topology set pipeline    # sequential: eng-1 → eng-2 → eng-3
batty topology set custom      # architect edits team.yaml directly
```

The daemon hot-reloads team.yaml on change:
1. Detect file modification (inotify/kqueue or poll)
2. Parse new config
3. Diff against current running state
4. Spawn new shims / kill removed shims
5. Update routing tables
6. Log topology change event

### Daemon Console Socket

The daemon exposes a Unix socket at `.batty/console.sock` for TUI connections.

**Protocol:**
```
Console → Daemon:
  ChatMessage { text: String }           # user typed in chat
  RequestAgentList                       # for agents view
  RequestAgentPtyStream { agent_id }     # for agent peek
  RequestBoardPath                       # for board view
  TopologyCommand { cmd: String }        # scale/configure

Daemon → Console:
  ChatMessage { from: String, text }     # architect response or system event
  AgentStatusUpdate { agents: Vec<AgentStatus> }
  PtyData { agent_id, bytes: Vec<u8> }   # live PTY stream
  SystemEvent { event: String }          # topology change, task completion, etc.
  BoardPath { path: PathBuf }            # path for kanban-md to open
```

Multiple consoles can connect simultaneously. All see the same state.

### Resume and Persistence

On `batty stop` or daemon shutdown:
- Save: team.yaml (current topology), board state, goal.yaml, conversation history, cycle history, agent session IDs
- Saved to `.batty/resume/`

On `batty` restart:
- Daemon reads resume state
- Respawns architect with conversation history in context
- Respawns team per saved team.yaml
- Architect summarizes what happened and continues

### Telegram Integration

Telegram remains an alternative to the console TUI for the chat view:
- Human sends messages to architect via Telegram
- Architect responses appear in Telegram
- System events (task completions, topology changes) are sent to Telegram
- Commands like `:status`, `:board`, `:scale 5` work in Telegram too

The console and Telegram are both frontends to the same architect conversation. Messages from either appear in both.

## Implementation Plan

### Wave 1: Goal-Directed Architect

Rewrite the architect's system prompt and add the assessment loop. No TUI yet — use existing `batty chat` as the interface.

- **Architect system prompt rewrite** — goal-directed prompt: "The human will tell you what they want. You operationalize it, create goal.yaml, plan the work, configure the team, and drive execution. You have access to: `batty scale`, `batty board`, `batty eval`."
- **Goal spec format** — define goal.yaml schema (description, criteria, budget, constraints). Architect creates it from conversation. `batty eval` runs the automated criteria.
- **Assessment loop** — architect periodically runs evaluation, logs results, adjusts strategy. Triggered after each cycle completion or on timer.
- **Topology commands** — `batty scale engineers N` modifies team.yaml, daemon hot-reloads and spawns/kills shims. `batty scale add-manager` / `batty scale remove-manager` for manager scaling.

**Exit:** `batty chat` starts a conversation with architect. Architect creates goal.yaml, populates board, scales team, drives execution through assessment cycles. Topology is dynamic.

### Wave 2: Console TUI Shell

Build the ratatui-based TUI with chat view and agents view. Board view delegates to kanban-md.

- **TUI application skeleton** — ratatui app with tab-based views, hotkey switching, status bar. Connects to daemon via `.batty/console.sock`.
- **Chat view** — scrollable message display, input line, message history. Sends/receives via daemon socket. System events shown inline.
- **Agents view** — table of agents with live status from daemon. Name, role, state, current task, inbox count. Aggregate metrics in footer.
- **Board view** — launches kanban-md TUI in the current terminal, returns to batty TUI on exit. Passes board path from daemon.

**Exit:** `batty` launches the TUI. Chat with architect works. Board view opens kanban-md. Agents view shows live status. Detach with `q`, reattach with `batty`.

### Wave 3: Agent Peek and Polish

Add agent terminal viewing and polish the experience.

- **Agent peek view** — stream PTY log from shim, render with ANSI colors in TUI. Auto-scroll. Hotkey `1-9` or Enter from agents view.
- **Daemon console socket** — Unix socket server in daemon, protocol for chat messages, agent status, PTY streams. Multiple simultaneous console connections.
- **Detach/reattach** — `q` detaches console (daemon keeps running). `batty` reattaches. On reattach, architect provides status summary.
- **Telegram sync** — messages from Telegram appear in console chat view and vice versa. Both frontends see the same conversation.

**Exit:** Full TUI with all 4 views. Detach/reattach works. Telegram and console are synchronized. Agent peek shows live terminal output.

### Wave 4: Resume and Budget

Persistence across restarts and budget tracking.

- **Resume state** — on stop, persist: goal.yaml, team.yaml, board state, conversation history (last N messages), cycle metrics. On restart, architect resumes with context.
- **Budget tracking** — track compute spend (estimated from API usage if available, or time-based estimate). Display in status bar. Architect factors remaining budget into strategy decisions.
- **Cycle history** — structured log of assessment cycles: tasks completed, metrics before/after, strategy changes, topology changes. Available to architect for trend analysis.

**Exit:** Stop and restart preserves full context. Budget tracked and visible. Cycle history informs strategy.

## Success Criteria

1. A user can type `batty`, describe "build me a chess engine at ELO 2800", and the system creates the plan, spins up agents, and begins autonomous execution.
2. The architect dynamically scales from 2 to 8+ engineers as the work moves from foundation to optimization.
3. The human can detach, go to lunch, reattach, and see a summary of progress.
4. The board is a living artifact that the architect generates and the human can edit.
5. Assessment cycles measure progress against the goal and adjust strategy automatically.
6. The TUI provides at-a-glance visibility into agents, board, and conversation without requiring tmux knowledge.

## Dependencies

- Shim architecture (v0.7.0) — must be complete and stable
- kanban-md TUI — existing, called as subprocess
- ratatui — Rust TUI framework (new dependency)
- Existing daemon, message routing, board operations — reused

## Risks

1. **Architect prompt quality** — the goal-directed prompt needs careful engineering to produce good strategies consistently. Mitigate with iteration on prompt + few-shot examples of good assessment cycles.
2. **Hot-reload complexity** — dynamically adding/removing agents while work is in progress. Mitigate with graceful shutdown (finish current task) for removed agents and clean-state init for new agents.
3. **Console TUI scope** — TUI development can sprawl. Mitigate by delegating board view to kanban-md and keeping other views minimal (chat is text, agents is a table, peek is a log stream).
4. **Assessment loop cost** — running evaluations every cycle consumes budget. Mitigate by making eval frequency configurable and caching results when code hasn't changed.
5. **Conversation context limits** — long conversations with the architect hit context windows. Mitigate by summarizing older conversation into memory, keeping only recent messages in context.
