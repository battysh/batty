# OpenClaw Active-Project Routing Policy

## Goal

Resolve inbound OpenClaw messages to the right Batty project only when the signal is strong enough to be safe. If confidence is not high, Batty must ask the human to confirm the project instead of guessing.

## State Model

Routing uses two persisted documents:

- Registry: project metadata and routing hints
- Routing state: operator-selected active project scopes

Routing state is separate because "currently active" is operator context, not project identity.

## `setActiveProject`

`setActiveProject(projectId, scope)` stores one active selection for exactly one scope:

- `global`
- `channel { channel, binding }`
- `thread { channel, binding, threadBinding }`

Updating a scope replaces the previous selection for that same scope.

## `resolveProjectForMessage`

Resolution inputs:

- message text
- optional channel provider name
- optional channel binding
- optional thread binding

Resolution outputs:

- selected `projectId` or `null`
- confidence: `high`, `medium`, `low`
- `requiresConfirmation`
- human-readable selection reason
- ranked candidates for UI/debugging

## Matching Order

1. Explicit `projectId` mention
2. Explicit alias mention
3. Exact thread binding match
4. Exact project name mention
5. Exact channel binding match
6. Active thread selection
7. Active channel selection
8. Unique tag match
9. Global active project
10. Single registered project fallback

## Auto-Routing Rules

Auto-route only when one of these is true:

- explicit `projectId` match
- explicit alias match
- exact thread-binding match
- non-control message with a strong channel/name match and no close competitor

Require confirmation when:

- multiple projects score similarly
- the only match is a tag, global active project, or weak channel hint
- the message looks like a control action and the project was not named explicitly

## Control Actions

Treat verbs like these as control actions:

- `stop`
- `restart`
- `pause`
- `resume`
- `merge`
- `deploy`
- `delete`
- `archive`
- `assign`
- `register`
- `unregister`

These actions must not ride on low-confidence context like a global active project.

## Examples

Auto-route:

- `"check batty"` when `batty` is a unique alias
- `"status?"` inside a bound Slack thread that maps to one project
- `"open alpha review queue"` when `alpha` is the `projectId`

Require confirmation:

- `"restart it"` when only a global active project is set
- `"check the core project"` when several projects share tag `core`
- `"merge this"` from a channel with multiple project candidates and no explicit project mention
