# Batty: Differentiation

Date: 2026-02-21

## Thesis

Wave is an AI-native terminal product.
Batty is an agent-agnostic control plane for autonomous work, built on tmux.

If Batty tries to compete as "our own AI assistant," it becomes a weaker Wave clone.
If Batty stays agent-agnostic and execution-focused, it has a clear and defensible position.

## Core Strategic Difference

- **Wave**: integrated AI experience inside a terminal workspace.
- **Batty**: orchestrate external agents (Claude Code, Codex, Aider) with policy, safety, and determinism. tmux is the runtime.

Batty's value is execution control, reliability, and throughput — not model quality.

## Side-by-Side

| Dimension | Wave | Batty |
|---|---|---|
| Product role | AI-native terminal app | Agent operations system on tmux |
| AI strategy | Built-in AI experience | BYO agents, model-agnostic |
| User value | Better terminal + AI UX | Deterministic agent execution |
| Runtime | Custom terminal | tmux sessions |
| Automation control | Assistant-style flows | Explicit policy tiers and governance |
| Completion model | User-judged | Test-gated, objective checks |
| Safety model | UX guardrails | Policy engine + audit trail |

## Hierarchical Agent Command

No other tool has this. Three layers:

| Layer | Role | What it does |
|---|---|---|
| **Director** | Strategy | Reads phase boards, picks next phase, dispatches supervisors |
| **Supervisor** | Tactics | Controls one phase: tmux supervision, policy, prompt handling, test gates |
| **Executor** | Labor | BYO coding agent running in a tmux pane, working through the phase board |

The human sits above all three with full visibility and override.

## Features That Make Batty Different

1. **Agent adapter layer** — Standard contract for external agents. No vendor lock-in.
2. **Hierarchical command** — Director dispatches. Supervisor controls execution. Executor writes code.
3. **tmux-native supervision** — Agents run in real tmux panes. User can type anytime. Batty supervises via pipe-pane (output) and send-keys (input).
4. **Policy engine** — Rules for auto-answering, escalation, restrictions, merge permissions. Progressive autonomy: observe → suggest → act → fully-auto.
5. **DoD gate system** — Per-task completion contracts with automated verification.
6. **Execution log** — Structured event timeline for humans and machines.
7. **Merge queue** — Parallel agents merging cleanly via serialized rebase queue.

## Messaging

Lead with:
- "Use the best agents. Batty controls execution."
- "Autonomous work with explicit safety and completion rules."
- "From prompt to verified done, with audit trail."

Avoid:
- "Our AI is better."
- "Another AI terminal."
- "A prettier terminal."

## OpenClaw: Philosophical Ally

OpenClaw validates the same composable, agent-agnostic philosophy from the autonomous end.

| Dimension | OpenClaw | Batty |
|---|---|---|
| Surface | Headless, messaging-first | Terminal-native, tmux-first |
| Interaction | Autonomous-first (overnight runs) | Supervised-first (policy-gated autonomy) |
| Agent strategy | BYO models via API keys | BYO agents as CLI processes |
| Data | Local Markdown files | Local Markdown (kanban-md), execution logs |
| Core pattern | Thin gateway connecting models + tools | Thin orchestration connecting agents + CLI tools |

Key difference: OpenClaw is for "I wake up and it's done." Batty is for "I watch it work and control the process." Both reject the monolith.

## Reality Check

Batty is differentiated only if these are true:
- I can swap Claude Code for Codex without changing my workflow.
- Batty supervises a whole phase of work, not just one task.
- Batty can automate safely under explicit policy.
- Completion is measured, not guessed.
- I can inspect every autonomous decision after the run.
