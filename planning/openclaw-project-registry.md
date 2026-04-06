# OpenClaw Project Registry Design

## Goal

Support many Batty-supervised projects under one OpenClaw supervisor without relying on cwd guessing, tmux session discovery, or implicit team-name heuristics.

The registry is the stable contract. Batty runtime code remains project-local; OpenClaw and future multi-project tooling resolve projects through this registry first.

## Design Constraints

- `projectId` is the only routing-safe identifier.
- `projectRoot`, `teamName`, and `sessionName` are metadata, not routing authority.
- Registration is explicit. Runtime operations must not guess by cwd or session name.
- Registry metadata is versioned and migration-aware from day one.
- The registry is user-scoped so one operator can supervise many repositories.

## Storage

- Default path: `~/.batty/project-registry.json`
- Override for tests or custom deployments: `BATTY_PROJECT_REGISTRY_PATH`
- Format: versioned JSON document
- Schema reference: [project-registry.schema.json](/Users/zedmor/batty/.batty/worktrees/eng-1-2/docs/reference/project-registry.schema.json)

## Registry Contract

Top-level fields:

- `kind`: fixed discriminator, currently `batty.projectRegistry`
- `schemaVersion`: integer schema version, currently `1`
- `projects`: list of registered projects

Per-project fields:

- `projectId`: stable unique ID, lowercase ASCII slug, primary key
- `name`: operator-facing display name
- `projectRoot`: absolute canonical repository root
- `boardDir`: absolute canonical board path
- `teamName`: Batty team name from `team.yaml`
- `sessionName`: explicit tmux/OpenClaw session identity
- `channelBindings`: explicit external channel map, each entry `{ channel, binding }`
- `owner`: optional owner string
- `tags`: free-form labels
- `policyFlags`: explicit booleans for supervision and cross-project behavior
- `createdAt`, `updatedAt`: unix timestamps

## Identity Rules

Routing:

- Mutating or supervisory operations must take `projectId`.
- `getProject(projectId)` is the canonical lookup operation.
- `listProjects()` returns metadata only; callers choose an explicit `projectId` before acting.

Validation:

- `projectId` must be unique.
- `projectRoot` must be unique.
- `teamName` must be unique.
- `sessionName` must be unique.
- `boardDir` must live under `projectRoot`.

This keeps operator-visible identifiers non-ambiguous even when many projects share one OpenClaw supervisor.

## Migration Plan

Versioning strategy:

- Every file carries `kind` and `schemaVersion`.
- Batty only writes the latest supported version.
- Batty refuses unknown future versions rather than silently coercing them.
- New schema versions must land with an explicit migrator and fixture tests.

Migration workflow:

1. Load raw JSON.
2. Inspect `kind` and `schemaVersion`.
3. If version is supported, decode and validate.
4. If version is older but migratable, convert to the latest in memory, validate, then persist the upgraded form on the next write.
5. If version is newer or unknown, fail closed with a clear error.

The current implementation ships only `schemaVersion: 1`, but the file format and loader are intentionally structured around version dispatch.

## CLI / API Surface

Library API:

- `register_project`
- `unregister_project`
- `list_projects`
- `get_project`

CLI:

- `batty project register`
- `batty project unregister <project-id>`
- `batty project list`
- `batty project get <project-id>`

Registration requires explicit project identity and metadata. Lookup and removal are keyed by `projectId`.

## Example

```json
{
  "kind": "batty.projectRegistry",
  "schemaVersion": 1,
  "projects": [
    {
      "projectId": "batty-core",
      "name": "Batty Core",
      "projectRoot": "/repos/batty",
      "boardDir": "/repos/batty/.batty/team_config/board",
      "teamName": "batty",
      "sessionName": "batty-batty",
      "channelBindings": [
        { "channel": "telegram", "binding": "chat:123456" }
      ],
      "owner": "platform",
      "tags": ["openclaw", "pilot"],
      "policyFlags": {
        "allowOpenclawSupervision": true,
        "allowCrossProjectRouting": false,
        "allowSharedServiceRouting": true,
        "archived": false
      },
      "createdAt": 1770000000,
      "updatedAt": 1770000000
    }
  ]
}
```
