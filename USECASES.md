# Use Cases

Real scenarios where Batty helps. Each includes the problem, how Batty solves it, and a config snippet you can adapt.

---

## 1. Solo Dev: Parallel Feature Work

**Problem:** You have 3 independent features to build. Running one agent at a time means the other two sit idle. Opening multiple terminals leads to merge conflicts and agents overwriting each other's changes.

**Solution:** Batty runs 3 engineer agents in parallel, each in its own git worktree. No conflicts during active work. Test gates catch integration issues at merge time.

```yaml
name: my-app
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [engineer]

  - name: engineer
    role_type: engineer
    agent: claude
    instances: 3
    use_worktrees: true
    talks_to: [architect]
```

```bash
batty init --template simple
batty start --attach
batty send architect "Build user auth, payment integration, and email notifications"
```

The architect decomposes the work into independent tasks. Each engineer picks one up, works in isolation, and nothing merges until `cargo test` (or your test command) passes.

**Template:** `batty init --template simple`

---

## 2. Architect + Engineers: Structured Task Decomposition

**Problem:** Task decomposition quality is the bottleneck, not coding speed. Throwing agents at vague requirements produces bad results. You need one agent planning and several executing.

**Solution:** An architect agent analyzes the request and breaks it into well-scoped subtasks. A manager dispatches to available engineers. Engineers execute in worktrees. Tests gate completion.

```yaml
name: my-project
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [manager]

  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    talks_to: [architect, engineer]

  - name: engineer
    role_type: engineer
    agent: codex
    instances: 5
    use_worktrees: true
    talks_to: [manager]
```

The hierarchy matters: engineers only talk to the manager, not to each other. This prevents communication explosion (5 agents = 20 possible channels without rules, but only 10 with `talks_to` enforcement).

**Template:** `batty init --template squad`

---

## 3. Mixed Agent Team: Best Tool for Each Job

**Problem:** Different agents have different strengths. Claude Code is good at architecture and planning. Codex is fast at focused implementation. Aider works well for targeted edits. You want to use all of them.

**Solution:** Batty is agent-agnostic. Assign different agents to different roles.

```yaml
name: mixed-team
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [manager]

  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    talks_to: [architect, fast-eng, edit-eng]

  - name: fast-eng
    role_type: engineer
    agent: codex
    instances: 3
    use_worktrees: true
    talks_to: [manager]

  - name: edit-eng
    role_type: engineer
    agent: aider
    instances: 2
    use_worktrees: true
    talks_to: [manager]
```

Claude plans, Codex builds new features fast, Aider handles targeted refactors. Each in its own tmux pane — watch them all work, or detach and come back later.

---

## 4. Remote Supervision via Telegram

**Problem:** You want the team running while you're away from your desk. SSH works but you want quick status checks and the ability to send direction from your phone.

**Solution:** Add a Telegram-connected user role. Send messages and receive updates from anywhere.

```yaml
name: my-project
roles:
  - name: human
    role_type: user
    channel: telegram
    talks_to: [architect]
    channel_config:
      provider: telegram
      target: "YOUR_CHAT_ID"
      bot_token: "${BATTY_TELEGRAM_BOT_TOKEN}"
      allowed_user_ids: [YOUR_USER_ID]

  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [human, engineer]

  - name: engineer
    role_type: engineer
    agent: claude
    instances: 3
    use_worktrees: true
    talks_to: [architect]
```

```bash
batty telegram   # guided setup
batty start      # launch the team
# now send direction from your phone via Telegram
```

The team runs in tmux on your server. You supervise from Telegram. `batty attach` when you're back at your desk.

**Template:** `batty init --template simple` then `batty telegram`

---

## Choosing a Template

| Scenario | Template | Agents | Description |
|----------|----------|-------:|-------------|
| Just trying it out | `solo` | 1 | Single engineer, no hierarchy |
| Pair programming with AI | `pair` | 2 | Architect + 1 engineer |
| Standard project | `simple` | 6 | Human + architect + manager + 3 engineers |
| Parallel sprint | `squad` | 7 | Architect + manager + 5 engineers |
| Large project | `large` | 19 | 3 management layers + 15 engineers |

Start with `pair` or `simple` and scale up as you get comfortable.

```bash
batty init --template pair    # start small
batty init --template simple  # standard setup
batty init --template squad   # more parallelism
```
