# Workflow Migration

Batty's workflow rollout is designed to be backward-compatible by default.

## Safe Defaults

- If `workflow_mode` is omitted from `.batty/team_config/team.yaml`, Batty defaults to `legacy`.
- `legacy` preserves current runtime behavior. Existing teams and boards continue to run without workflow-specific metadata.
- Older task files continue to parse even when they do not contain workflow fields.
- Unknown future workflow fields in task frontmatter are ignored safely by current parsers.

## Rollout Modes

- `legacy`: current Batty behavior, no workflow metadata required.
- `hybrid`: incremental adoption. Workflow features can be introduced selectively while legacy runtime behavior remains available.
- `workflow_first`: explicit opt-in. Use this only when you are ready to treat workflow state as the primary control surface.
- `board_first`: explicit opt-in. Use this when the board should drive dispatch and recovery directly while manager inbox traffic is reserved for review, blockers, and escalations.

## Validation

Run:

```bash
batty validate
```

Validation now reports the effective workflow mode and prints migration notes:

- older configs with no `workflow_mode` are called out as defaulting to `legacy`
- `hybrid` is reported as an incremental migration state
- `workflow_first` is reported as an opt-in mode that should be paired with completed workflow metadata and orchestrator rollout
- `board_first` is reported as an opt-in mode that keeps the orchestrator active while suppressing board-derived manager relay prompts

## Recommended Adoption Path

1. Leave existing projects on the default `legacy` mode.
1. Add `workflow_mode: hybrid` when you want to begin introducing workflow-aware tooling.
1. Move to `workflow_mode: board_first` when the board should own assignment flow and managers should stop acting as relay agents.
1. Move to `workflow_mode: workflow_first` only after your board/process migration is complete.
