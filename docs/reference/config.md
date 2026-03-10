# Configuration Reference

Team configuration is defined in `.batty/team_config/team.yaml`.

## Top-Level Fields

| Key | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | string | yes | Team name. Used as tmux session name (`batty-<name>`). |
| `board` | object | no | Kanban board settings. |
| `standup` | object | no | Periodic standup configuration. |
| `layout` | object | no | tmux pane layout zones. |
| `roles` | array | yes | Role definitions (at least one non-user role required). |

## Board Settings

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `board.rotation_threshold` | integer | `20` | Number of completed tasks before board rotation triggers. |

## Standup Settings

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `standup.interval_secs` | integer | `1200` | How often the daemon runs standups (seconds). |
| `standup.output_lines` | integer | `30` | Number of recent output lines captured per agent for standup context. |

## Layout

The layout defines how tmux panes are arranged in zones (vertical columns).

```yaml
layout:
  zones:
    - name: architect
      width_pct: 20
    - name: managers
      width_pct: 20
    - name: engineers
      width_pct: 60
      split: { horizontal: 4 }
```

| Key | Type | Description |
| --- | --- | --- |
| `layout.zones[].name` | string | Zone identifier. |
| `layout.zones[].width_pct` | integer | Percentage of terminal width (all zones must sum to <= 100). |
| `layout.zones[].split.horizontal` | integer | Number of horizontal splits within the zone. |

## Role Definition

Each entry in `roles` defines one agent role.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | Role name. Must be unique. Used in instance naming and message routing. |
| `role_type` | enum | required | One of: `user`, `architect`, `manager`, `engineer`. |
| `agent` | string | none | Agent program: `claude`, `codex`, etc. Required for non-user roles. |
| `instances` | integer | `1` | Number of instances to spawn. Engineers are multiplicative across managers. |
| `prompt` | string | none | Prompt template filename (relative to `team_config/`). |
| `talks_to` | array | `[]` | Role names this role can message. Empty = default hierarchy rules. |
| `channel` | string | none | Communication channel for user roles (e.g., `telegram`). |
| `channel_config` | object | none | Channel-specific config (`target`, `provider`). |
| `nudge_interval_secs` | integer | none | How often to inject the nudge prompt (from `## Nudge` section of prompt template). |
| `receives_standup` | boolean | none | Whether this role receives standup reports. |
| `standup_interval_secs` | integer | none | Per-role standup interval override. |
| `owns` | array | `[]` | Glob patterns for files this role owns (informational). |
| `use_worktrees` | boolean | `false` | Create isolated git worktrees for each instance. |

### Role Types

| Type | Description |
| --- | --- |
| `user` | Human endpoint. No tmux pane. Uses a channel (e.g., Telegram) for communication. Cannot have `agent`. |
| `architect` | Top of hierarchy. Plans architecture, sends directives to managers. |
| `manager` | Middle layer. Creates tasks, assigns work to engineers, reports up to architect. |
| `engineer` | Execution layer. Implements tasks, works in worktrees. |

### Instance Naming

- **Single instance** (`instances: 1`): uses the role name directly (e.g., `architect`)
- **Multiple instances** (`instances: 3`): appends index (e.g., `manager-1`, `manager-2`, `manager-3`)
- **Engineers under managers**: multiplicative naming `eng-<mgr>-<eng>` (e.g., `eng-1-1`, `eng-2-3`)

Total engineers = `manager.instances` x `engineer.instances`.

### Communication Routing

If `talks_to` is configured, only listed roles are allowed. If empty, default hierarchy applies:

- User <-> Architect
- Architect <-> Manager
- Manager <-> Engineer

The `human` sender (CLI user outside tmux) can always message any role.

## Default Template (simple)

```yaml
name: my-project

board:
  rotation_threshold: 20

standup:
  interval_secs: 600
  output_lines: 40

layout:
  zones:
    - name: architect
      width_pct: 33
    - name: managers
      width_pct: 33
    - name: engineers
      width_pct: 34
      split: { horizontal: 3 }

roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    prompt: architect.md
    talks_to: [manager]
    nudge_interval_secs: 900

  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    prompt: manager.md
    talks_to: [architect, engineer]

  - name: engineer
    role_type: engineer
    agent: claude
    instances: 3
    prompt: engineer.md
    talks_to: [manager]
    use_worktrees: true
```
