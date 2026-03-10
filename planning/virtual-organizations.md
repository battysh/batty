# Virtual Organizations Roadmap

Batty's evolution from agent task runner to **virtual organization infrastructure** — instantiate arbitrary org structures of autonomous agents, tear them down, restructure, and iterate in seconds.

---

## Current State (Phase T1 Complete)

~4,000 lines of team orchestration code, 259 tests passing.

| Component | Status |
|-----------|--------|
| Hierarchical YAML config | Done |
| Role hierarchy with multiplicative instances | Done |
| tmux visual layout (zones/panes) | Done |
| Daemon (polling, monitoring, spawning) | Done |
| Message routing (daemon → agent) | Done |
| Output watching + completion detection | Done |
| Standup generation (scoped to reports) | Done |
| Git worktree isolation per engineer | Done |
| Event stream (JSONL) | Done |
| 7 org templates (solo → 20-person teams) | Done |
| Telegram bridge (channel trait) | Stubbed |
| CLI (init/start/stop/attach/status/send/assign) | Done |

### What We Have That's Valuable

- **Org-as-code**: YAML config → running team in seconds
- **Visual supervision**: tmux zones with status bars, live output
- **Prompt-per-role**: each member gets role-specific instructions
- **Nudge/standup loops**: periodic attention allocation
- **Worktree isolation**: parallel execution without git conflicts
- **Template library**: solo, pair, simple, squad, large, research, software, batty

---

## The Core Insight

**The org chart IS the algorithm.** How you structure communication determines what the system can solve.

- **Who talks to whom** = information flow
- **Who owns what** = responsibility boundaries
- **Nudge intervals** = attention allocation
- **Standup scope** = context compression
- **Worktree isolation** = parallel execution without conflicts

We can instantiate a 20-person research lab in 30 seconds, tear it down, restructure, and try again. No human org can do that. This is the superpower.

---

## Gap Analysis: Team → Virtual Organization

### 1. Rigid Hierarchy

**Current:** 4 fixed role types — `User | Architect | Manager | Engineer`. Tree structure only.

**Needed:** Arbitrary topology. Real orgs have matrix structures, cross-functional links, shared services, peer roles, consultants. The Diplomacy team doc has: PI, Strategy Lead, Dialogue Lead, Integration Lead, Game Theory Lead, Domain Expert — many are peers, not parent-child.

### 2. One-Way Communication

**Current:** Only the daemon pushes messages to agents. Agents cannot initiate messages to each other.

**Needed:** Bidirectional agent-initiated communication. An engineer should be able to say "hey manager, I'm blocked on the API schema" without waiting for a standup cycle. The `batty send` CLI is already in agents' PATH — they just need to use it, and the daemon needs to route it.

### 3. No Agent-to-Agent Autonomy

**Current:** Daemon detects completion via output hashing and notifies manager. Agents are passive recipients of instructions.

**Needed:** Agents as autonomous communicators. An architect decides to message a specific engineer. A QA lead requests a re-run from the CI engineer. A domain expert provides feedback to whoever needs it.

### 4. No Persistent State

**Current:** Daemon state is in-memory HashMap. Restart loses everything.

**Needed:** State survives crashes, pauses, machine migration. Event stream as source of truth.

### 5. Single-Project Scope

**Current:** One team per project, one daemon instance.

**Needed:** Multi-project orgs. Shared services across projects. An SRE team that serves multiple product teams.

### 6. No Dynamic Scaling

**Current:** Fixed team size defined at init time.

**Needed:** Add/remove agents at runtime. "We need 3 more engineers for this sprint."

---

## Roadmap

### Phase T2: Bidirectional Agent Communication

**The critical unlock.** Without this, agents are independent workers getting instructions, not an organization.

**Changes:**
- Agents use `batty send <role> "<msg>"` to talk to each other (CLI already exists)
- Auto-detect sender via `@batty_role` pane option (`detect_sender()` already implemented)
- Daemon routes bidirectionally: agent → agent, not just daemon → agent
- Message delivery: queue if target is busy, inject when idle
- Completion notifications route to whoever assigned the task (not hardcoded to manager)
- Agents can request help, report blockers, share findings

**Result:** Agents become active participants in the organization, not passive recipients.

### Phase T3: Flexible Roles

Remove the rigid 4-type enum. Let org-chart semantics emerge from config.

**Changes:**
- Replace `User | Architect | Manager | Engineer` with: `agent | human | observer`
- All org semantics come from config fields: `talks_to`, `owns`, `reports_to`, prompt template
- Any role can be a "manager" — it's just a role with `reports` underneath it
- Any role can be an "architect" — it's just a role with `owns: ["docs/**", "planning/**"]`
- Standup/nudge scoping derives from `talks_to` graph, not hardcoded hierarchy
- New built-in role capabilities as config flags:
  - `can_assign_tasks: true` — can use `batty assign`
  - `can_merge: true` — can merge worktrees
  - `has_board: true` — owns a kanban board section
  - `use_worktrees: true` — gets isolated git worktree

**Example: Diplomacy Team**
```yaml
roles:
  - name: pi
    role_type: agent
    agent: claude
    prompt: pi.md
    talks_to: [strategy-lead, dialogue-lead, integration-lead, gt-lead]
    owns: ["docs/architecture.md", "planning/**"]
    nudge_interval_secs: 1800
    can_assign_tasks: true

  - name: strategy-lead
    role_type: agent
    agent: claude
    prompt: strategy-lead.md
    talks_to: [pi, integration-lead]
    reports_to: pi
    has_board: true
    can_assign_tasks: true

  - name: strategy-ic
    role_type: agent
    agent: codex
    instances: 3
    prompt: strategy-ic.md
    talks_to: [strategy-lead]
    reports_to: strategy-lead
    use_worktrees: true

  - name: dialogue-lead
    role_type: agent
    agent: claude
    prompt: dialogue-lead.md
    talks_to: [pi, integration-lead, strategy-lead]
    reports_to: pi
    has_board: true

  - name: domain-expert
    role_type: agent
    agent: claude
    prompt: domain-expert.md
    talks_to: [pi, strategy-lead, dialogue-lead]
    nudge_interval_secs: 3600  # less frequent — consultant role
```

**Result:** Any real-world org chart can be expressed. Research labs, software teams, consulting firms, game-playing teams.

### Phase T4: Communication Graph

Build a real graph from `talks_to` declarations. Enable richer communication patterns.

**Changes:**
- Directed communication graph built at startup from config
- Message types:
  - `direct` — one-to-one message
  - `broadcast` — to all direct reports
  - `announce` — to all members in the org
  - `request` — expects a response (tracked)
  - `report` — status update to reports_to chain
- Routing rules:
  - Can only message roles listed in your `talks_to` (enforced)
  - Unless `announce` type (visible to all)
- Channel types:
  - `tmux` — pane injection (default for agents)
  - `file` — markdown mailbox in `.batty/messages/<role>/`
  - `external` — Telegram, Slack, Discord, email
- Message history: append to `.batty/messages/log.jsonl`
- Delivery confirmation: sender gets ACK when message is injected

**Result:** Information flows through the org like a real organization. No back-channels, no information silos (unless you design them).

### Phase T5: Persistent State + Recovery

Make virtual organizations durable. Survive crashes, pauses, machine restarts.

**Changes:**
- Serialize full daemon state to `.batty/team_config/state.json` on every tick
- On `batty start`, detect existing state file and resume from it
- Event stream becomes append-only log (event sourcing pattern)
- State reconstruction: replay events from last checkpoint
- Graceful shutdown: `batty stop` saves state, sends shutdown messages to agents
- Session recovery: `batty start` after crash picks up where it left off
- Agent context preservation: save last N lines of each pane on shutdown

**Result:** Virtual organizations are persistent entities, not ephemeral processes.

### Phase T6: Org Composition + Templates

Make orgs composable from building blocks.

**Changes:**
- Sub-team configs as separate YAML files, included via `$ref`
- Composable init: `batty init --template research --add-subteam qa:3 --bridge telegram:pi`
- Template inheritance: `extends: research` with overrides
- Cross-org messaging: org A's integration lead talks to org B's API lead
- Shared services: an SRE team config can be included by multiple projects
- Template marketplace: community-contributed org structures

**Example: Composing an Org**
```yaml
name: cicero-project
extends: research

# Override base template
overrides:
  pi:
    nudge_interval_secs: 900

# Add sub-teams
include:
  - file: qa-subteam.yaml
    reports_to: integration-lead
  - file: infra-subteam.yaml
    reports_to: pi

# Add external bridges
bridges:
  - role: pi
    channel: telegram
    config:
      target: "123456789"
      provider: openclaw
```

**Result:** Building orgs is like building with Lego. Mix and match sub-teams, bridges, and role definitions.

### Phase T7: Dynamic Scaling + Self-Organization

The org adapts at runtime.

**Changes:**
- `batty scale <role> <count>` — add/remove instances at runtime
- Daemon watches load signals (queue depth, stale agents, completion rates)
- Suggests scaling: "3 engineers have been idle for 10 min, consider scaling down"
- Auto-reassignment: when an agent crashes, redistribute its tasks
- Agent self-organization: agents can propose org changes via `batty propose`
  - "I need a dedicated researcher for this sub-problem"
  - Requires approval from reports_to chain
- Cost tracking: token usage per agent, per role, per hour
- Budget limits: "this org can spend at most $X/hour on API calls"

**Result:** The org is a living system, not a static structure.

---

## Use Cases

### 1. Software Development (Current)
Architect → Managers → Engineers. Code review, task boards, git worktrees. Already works.

### 2. Research Lab (Diplomacy-Class Problems)
PI → Sub-Leads → ICs with cross-functional communication. Milestone-driven, experiment-heavy. Needs Phase T3 for flexible roles.

### 3. Consulting Firm
Partners → Managers → Analysts with client-facing roles bridged via Telegram/Slack. Needs Phase T4 for external channels.

### 4. Game-Playing AI
Coordinator → Specialist agents (strategy, dialogue, evaluation) with tight integration loops. Needs Phase T4 for communication graph.

### 5. Content Production
Editor-in-chief → Section editors → Writers with review workflows. Needs Phase T3 for flexible roles and Phase T4 for review routing.

### 6. Security Red Team
Lead → Recon agents + Exploit agents + Report agents. Parallel exploration with finding aggregation. Needs Phase T2 for bidirectional comms.

### 7. Due Diligence / Analysis
Lead analyst → Domain specialists (legal, financial, technical, market). Parallel investigation with synthesis. Works with Phase T3.

---

## Design Principles

1. **Org-as-code.** The entire organization is defined in version-controlled YAML + Markdown. Fork an org, modify it, submit a PR.

2. **Communication shapes intelligence.** The org chart isn't bureaucracy — it's the algorithm. Different structures solve different problems. Experiment with topology.

3. **Agents are first-class members.** They send messages, own artifacts, report status, request help. They're not tools being invoked — they're team members operating autonomously.

4. **Human-in-the-loop by default, optional.** Every org has at least one human role (bridged via Telegram/Slack). But `role_type: observer` allows fully autonomous orgs with human monitoring.

5. **Composable over monolithic.** Small, reusable team configs that snap together. Don't build one giant YAML — compose from building blocks.

6. **Observable.** Every message, state change, and decision is logged. You can replay an org's entire history. Understand why it succeeded or failed.

7. **Disposable.** Spin up in seconds, tear down instantly. The cost of trying a new structure is near-zero. This enables evolutionary org design — try 10 structures, keep what works.

---

## Success Metrics

- **Time to first useful output** from a freshly instantiated org
- **Communication efficiency** — ratio of messages to completed tasks
- **Recovery time** — how fast an org resumes after crash/pause
- **Scaling linearity** — does adding agents improve throughput proportionally?
- **Template reuse** — how many projects use the same org templates?
- **Org iteration speed** — how fast can you restructure and re-deploy?

---

## References

- `docs/new_beginnings/dimplomacy.md` — CICERO team org chart reference
- `docs/new_beginnings/software.md` — software team communication patterns
- `docs/new_beginnings/idea.md` — original vision document
- `examples/chess.md` — chess engine challenge (first complex use case)
- `planning/architecture.md` — current technical architecture
