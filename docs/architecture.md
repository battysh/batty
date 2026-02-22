# Architecture

Batty orchestrates a three-tier workflow:

- Director: reviews completed phase outcomes
- Supervisor: monitors executor output and resolves escalations
- Executor: coding agent that performs task implementation work

## Runtime Layers

1. Prompt detection and auto-answer policy (Tier 1)
2. Supervisor escalation and response injection (Tier 2)
3. Session/log lifecycle management in tmux

## Source References

- Project architecture: <https://github.com/zedmor/batty/blob/main/planning/architecture.md>
- Development philosophy: <https://github.com/zedmor/batty/blob/main/planning/dev-philosophy.md>
