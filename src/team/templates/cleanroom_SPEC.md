# Clean-Room Spec Index

Store one behavior-only spec per unit under `specs/<behavior-slug>/SPEC.md`.

Each behavior spec must use this shape:

```md
# Behavior: <name>

## Purpose

Describe the observable behavior only.

## Inputs

- <external input>

## Outputs

- <visible output>

## State Transitions

- <externally observable state change>

## Edge Cases

- <edge case>

## Acceptance Criteria

- <black-box expectation>
```

Hard rules:

- No source code
- No pseudocode
- No register names
- No addresses or instruction sequences
- No decompiler output
