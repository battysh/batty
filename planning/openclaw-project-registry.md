# OpenClaw Project Registry Design

## Goal

Support many Batty-supervised projects under one OpenClaw supervisor without relying on cwd guessing, tmux session discovery, or implicit team-name heuristics.

The registry remains the stable per-project metadata contract. Active-project selection and message routing use a separate routing-state model so registry entries stay declarative and portable.

## Storage

- Registry path: `~/.batty/project-registry.json`
- Registry schema: [project-registry.schema.json](/Users/zedmor/batty/.batty/worktrees/eng-1-2/docs/reference/project-registry.schema.json)
- Routing-state path: `~/.batty/project-routing-state.json`
- Routing-state schema: [project-routing-state.schema.json](/Users/zedmor/batty/.batty/worktrees/eng-1-2/docs/reference/project-routing-state.schema.json)

## Registry Contract

Top-level fields:

- `kind`: fixed discriminator, `batty.projectRegistry`
- `schemaVersion`: integer schema version, currently `2`
- `projects`: list of registered projects

Per-project fields:

- `projectId`: stable unique ID, lowercase ASCII slug, primary key
- `name`: operator-facing display name
- `aliases`: lowercase routing aliases such as `batty` or `claw`
- `projectRoot`: absolute canonical repository root
- `boardDir`: absolute canonical board path
- `teamName`: Batty team name from `team.yaml`
- `sessionName`: explicit tmux/OpenClaw session identity
- `channelBindings`: explicit channel/thread map, each entry `{ channel, binding, threadBinding? }`
- `owner`: optional owner string
- `tags`: free-form lowercase routing labels
- `policyFlags`: explicit booleans for supervision and cross-project behavior
- `createdAt`, `updatedAt`: unix timestamps

## Identity Rules

- `projectId` remains the only mutation-safe identifier.
- `aliases`, `tags`, and bindings are routing hints, not authority for destructive actions by themselves.
- `projectRoot`, `teamName`, and `sessionName` are metadata, not routing authority.
- Registration remains explicit; runtime routing must not infer projects from cwd or session name.

## Migration Plan

- `schemaVersion: 1` migrates in memory to `schemaVersion: 2` by defaulting `aliases` to `[]` and `threadBinding` to `null`.
- Batty writes only the latest supported version.
- Unknown future versions fail closed.

## API Surface

Library API:

- `register_project`
- `unregister_project`
- `list_projects`
- `get_project`
- `set_active_project`
- `resolve_project_for_message`

CLI:

- `batty project register`
- `batty project unregister <project-id>`
- `batty project list`
- `batty project get <project-id>`
- `batty project set-active <project-id>`
- `batty project resolve "<message>"`
