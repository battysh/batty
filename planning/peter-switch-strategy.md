# Batty: Peter Steinberger User Card

Date: 2026-02-21

## Ideal User Profile

Peter optimizes for shipping velocity with AI agents. Technical enough to build his own tooling, impatient with friction, willing to trade ceremony for speed when Git can absorb risk.

He already has a working stack (Ghostty + Codex/Claude + tmux), so Batty only wins if it's clearly faster and cleaner than his current manual setup.

## What He Values

1. **Speed** — Minimal prompts, minimal waiting, fast startup.
2. **Low-friction orchestration** — Multiple agents in parallel, easy visual tracking, quick handoff.
3. **CLI-first** — CLIs over MCPs. Small tools, good `--help`, JSON output, clean exit codes.
4. **Context efficiency** — No tool bloat, no hidden context tax.
5. **Transparency** — Review diffs and logs, not chat theater. Clear run state, auditable behavior.
6. **Reliability** — Works in real projects, not demos. Predictable under long-running sessions.

## Non-Negotiables for Switching

- Must feel as fast as his current tmux + Ghostty workflow.
- Must preserve native agent UX (no abstraction that fights Codex/Claude).
- Must support parallel agent operations cleanly.
- Must be keyboard-first and scriptable.
- Must not force proprietary "Batty AI."

## Value Proposition

Not "another terminal." Not "a better tmux."

Batty promises: **keep your current speed, add supervised autonomy and better coordination.**

- "Use your best agents. Batty controls execution."
- "Minimal by default, orchestration when needed."
- "From prompt to verified done with policy + audit trail."

## Must-Have Features (P0)

1. **Multi-agent tmux sessions** — Launch, label, monitor many runs. Fast pane creation via `batty work`.
2. **Agent-agnostic** — First-class support for Claude Code, Codex CLI, Aider. No vendor lock-in.
3. **Policy modes** — `observe`, `suggest`, `act-with-approval`, `fully-auto`. Visible mode + action log.
4. **DoD gates** — Per-task completion checks (test/lint/build/custom).
5. **CLI-native** — `batty` commands composable in scripts. JSON output for automation.
6. **Minimal core** — tmux + status bar + orchestrator pane. No mandatory chrome.

## Nice-to-Have (P1)

1. **Session templates** — Recreate full multi-agent setups instantly per repo/task type.
2. **Run playback** — Timeline of prompts/actions/checks/results.
3. **Team-scale orchestration** — Shared run recipes and policy presets.

## Migration Path

1. **Coexistence** — Keep Ghostty. Use Batty only for orchestrated runs (launches tmux sessions).
2. **Replace pain points** — Better multi-agent tracking than manual split panes. Better recovery than manual process juggling.
3. **Prove gains** — Faster completion on real tasks. Fewer manual interruptions. Cleaner audit trail.

## Adoption Proof

Peter switches if:
- Startup to first agent run under 3 seconds.
- Can manage 4+ concurrent runs without confusion.
- At least 20% reduction in manual orchestration effort.
- No regressions in CLI-first workflow.

## What Makes Him Churn

- UI-heavy experience by default.
- Hidden background magic that breaks trust.
- Slow or laggy interactions.
- Lock-in to Batty-native AI.
- Complex setup exceeding current workflow effort.
