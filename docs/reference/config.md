# Configuration Reference

Team configuration lives in `.batty/team_config/team.yaml`.

## Top Level

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `name` | string | none | Team/session name; must be non-empty. |
| `board.rotation_threshold` | integer | `20` | Rotate completed board items after this many done tasks. |
| `standup.interval_secs` | integer | `1200` | Default standup cadence in seconds. |
| `standup.output_lines` | integer | `30` | Recent output lines included in standup context. |
| `layout.zones` | array | none | Optional tmux column layout; zone widths must total `<= 100`. |
| `roles` | array | none | Role definitions; at least one non-`user` role is required. |

## Layout Fields

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `layout.zones[].name` | string | none | Zone label; names containing `architect`, `manager`, or `engineer` map members by role type. |
| `layout.zones[].width_pct` | integer | none | Zone width percentage. |
| `layout.zones[].split.horizontal` | integer | none | Max members assigned directly into that zone before overflow falls through. |

Example:

```yaml
layout:
  zones:
    - name: architect
      width_pct: 20
    - name: managers
      width_pct: 25
    - name: engineers
      width_pct: 55
      split:
        horizontal: 6
```

## Role Fields

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `roles[].name` | string | none | Unique role name used for routing and instance names. |
| `roles[].role_type` | enum | none | One of `user`, `architect`, `manager`, `engineer`. |
| `roles[].agent` | string | none | Required for non-user roles; omitted for `user`. |
| `roles[].instances` | integer | `1` | Must be greater than `0`. |
| `roles[].prompt` | string | none | Prompt filename relative to `.batty/team_config/`. |
| `roles[].talks_to` | array[string] | `[]` | Explicit routing allowlist; empty uses default hierarchy. |
| `roles[].channel` | string | none | User-channel type; current documented value is `telegram`. |
| `roles[].channel_config` | object | none | Channel settings; see Telegram fields below. |
| `roles[].nudge_interval_secs` | integer | none | Periodic nudge interval for prompt templates with a `## Nudge` section. |
| `roles[].receives_standup` | boolean | none | Opt role into standup delivery. |
| `roles[].standup_interval_secs` | integer | none | Per-role standup override in seconds. |
| `roles[].owns` | array[string] | `[]` | Informational ownership globs. |
| `roles[].use_worktrees` | boolean | `false` | Use isolated git worktrees for that role's instances. |

## Instance Naming

| Scenario | Result |
| --- | --- |
| Single architect/manager role with `instances: 1` | Uses the role name, for example `architect` or `manager`. |
| Architect/manager role with `instances: N` | Uses `<role>-1` through `<role>-N`. |
| Single engineer role named `engineer` under managers | Uses legacy names like `eng-1-1`, `eng-1-2`, `eng-2-1`. |
| Custom or multiple engineer roles | Uses `<role>-<manager-index>-<engineer-index>`, for example `frontend-1-2`. |
| Engineer role with empty `talks_to` | Multiplies across all manager instances. |
| Engineer role with `talks_to: [manager-role]` | Only attaches to compatible manager roles or instances. |
| Engineers with no managers | Use flat names like `engineer`, `engineer-2`, `researcher-1`. |

Default routing, when `talks_to` is empty, is `user <-> architect <-> manager <-> engineer`. The CLI sender `human` can always send to any role.

## Telegram Channel Fields

Use these only on a `user` role with `channel: telegram`.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `roles[].channel_config.target` | string | none | Chat ID used when Batty sends outbound messages. |
| `roles[].channel_config.provider` | string | none | Provider name, typically `native` or `openclaw`. |
| `roles[].channel_config.bot_token` | string | none | Native Bot API token; if omitted, Batty also checks `BATTY_TELEGRAM_BOT_TOKEN`. |
| `roles[].channel_config.allowed_user_ids` | array[integer] | `[]` | Allowed inbound Telegram user IDs; empty denies all inbound messages. |

## Complete Example

```yaml
name: my-project
board:
  rotation_threshold: 20
standup:
  interval_secs: 900
  output_lines: 40
layout:
  zones:
    - name: architect
      width_pct: 20
    - name: managers
      width_pct: 25
    - name: engineers
      width_pct: 55
      split:
        horizontal: 6
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "123456789"
      provider: native
      bot_token: "123456:ABCDEF"
      allowed_user_ids: [123456789]
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
    prompt: architect.md
    receives_standup: true
  - name: manager
    role_type: manager
    agent: claude
    prompt: manager.md
    instances: 2
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    prompt: engineer.md
    instances: 3
    talks_to: [manager]
    nudge_interval_secs: 900
    standup_interval_secs: 1800
    owns: ["src/**", "tests/**"]
    use_worktrees: true
```
