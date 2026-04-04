# Full Team

Architect + manager + 3 engineers with auto-dispatch and test gating. The standard multi-agent setup.

**When to use:** You have a project with 3+ independent workstreams. The architect plans, manager dispatches, engineers execute in parallel.

## Setup

```bash
cp -r examples/full-team .batty/team_config
batty start --attach
batty send architect "Build an e-commerce backend: product catalog, cart, and checkout"
```

## What you get

- 5 tmux panes: architect + manager + 3 engineers
- Hierarchical communication: architect → manager → engineers (enforced by `talks_to`)
- Auto-dispatch: manager automatically assigns tasks from the kanban board
- Each engineer in its own git worktree
- Test gating — nothing merges until tests pass
- Layout zones: architect (30%), manager (20%), engineers (50%)
- Standups every 10 minutes with intervention system

## Equivalent template

```bash
batty init --template simple  # includes Telegram user role
```
