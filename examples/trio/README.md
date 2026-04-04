# Trio

Architect + 2 engineers. The architect decomposes work, engineers execute in parallel on isolated worktrees.

**When to use:** You have a task that can be split into 2 independent subtasks. The architect plans, engineers build.

## Setup

```bash
cp -r examples/trio .batty/team_config
batty start --attach
batty send architect "Build a REST API with user registration and password reset"
```

## What you get

- 3 tmux panes: architect + 2 engineers
- Architect breaks tasks down, engineers pick them up
- Each engineer works in its own git worktree — no conflicts
- `talks_to` enforced: engineers talk to architect only
- Test gating on completion

## Equivalent template

```bash
batty init --template pair  # similar, but with 1 engineer
```
