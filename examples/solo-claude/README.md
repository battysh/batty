# Solo Claude

Single Claude Code agent with no hierarchy. The simplest Batty setup — one agent, one pane.

**When to use:** You want Batty's test gating and session persistence for a single agent.

## Setup

```bash
cp -r examples/solo-claude .batty/team_config
batty start --attach
batty send engineer "Refactor the auth module to use JWT"
```

## What you get

- One tmux pane running Claude Code
- Test gating on task completion
- Session persistence (detach and reattach)
- Event logging in `.batty/logs/`

## Equivalent template

```bash
batty init --template solo
```
