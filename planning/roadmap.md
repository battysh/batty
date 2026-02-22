# Batty Roadmap

## Thesis

Developers need a workflow model for building with agents: structured phases, supervised execution, quality gates, audit trails. Batty implements this model on top of tmux.

## Principles

- tmux is the runtime. Not a stopgap — the permanent architecture.
- Compose, don't monolith. tmux + kanban-md + BYO agents.
- Ship fast. Validate with real users before adding complexity.
- Markdown as backend. Files in, files out, git tracks everything.

---

## Phase 1: Core Agent Runner (Done)

`batty work <phase>` — Rust CLI that reads a kanban board, spawns an agent in a PTY, supervises the session. Policy engine, prompt detection, test gates, execution logging.

98 tests passing. All 11 tasks done.

---

## Phase 2: tmux-based Intelligent Supervisor (Done)

`batty work phase-1` launches a tmux session. Executor in main pane, orchestrator log in bottom pane, status in tmux status bar.

- **tmux session lifecycle** — create, attach, pipe-pane, send-keys, reconnect
- **Event extraction** — read piped output, extract structured events via regex
- **Prompt detection** — silence + pattern heuristic = executor is asking something
- **Tier 1 auto-answer** — regex match → send-keys (instant, ~70-80% of prompts)
- **Tier 2 supervisor agent** — API call with project context → send-keys (on-demand, stateless)
- **tmux status bar** — phase/task/progress/supervisor state
- **Orchestrator log pane** — bottom split, tail -f on event log
- **Stuck detection** — looping/stalled/crashed → nudge or escalate
- **Human override** — human types in tmux, supervisor steps back

**Exit:** Executor works through board. Routine prompts auto-answered. Real questions answered by supervisor. Status bar + orchestrator pane show everything. Session survives disconnect.

---

## Phase 2.4: Supervision Harness Validation (Next)

Make supervisor behavior provable before runtime hardening.

- **Deterministic harness contract** — scenario matrix with expected supervisor outcomes
- **Real tmux invariants** — executor pane targeting and persistent UI checks
- **Mock matrix in tmux** — deterministic executor/supervisor fixtures
- **Real supervisor with mocked executor** — Claude + Codex integration checks
- **Real supervisor+executor smoke runs** — opt-in end-to-end validation paths
- **Prompt catalog + runbook** — stable prompts, env flags, and pass/fail criteria

**Exit:** Harness suite passes in real tmux, with documented real-agent integration and smoke tests.

---

## Phase 2.5: Runtime Hardening + Dogfood

Run Batty on its own hardening phase and close the reliability gaps that block day-to-day use.

- **Worktree isolation first** — every phase run uses a worktree before AI review exists
- **Prompt composition** — deterministic launch context: `CLAUDE.md` + `PHASE.md` + board state + config
- **Completion detection contract** — define exactly when a phase is complete
- **Mid-phase recovery** — Batty reconnects and resumes supervision after process crash
- **tmux capability checks** — detect version/features and degrade gracefully
- **Dogfood gate** — Batty executes phase-2.5 end-to-end and merges with review

**Exit:** We complete a real Batty development phase using Batty itself.

---

## Phase 3A: Sequencer + Human Review Gate

Separate phase chaining from AI evaluation so we can ship useful automation earlier.

- **`batty work all`** — phase sequencer, runs phases in order
- **Phase summary + review packet** — standardized artifacts for human review
- **Human review gate** — merge / rework / escalate decisions without director agent
- **Rework loop** — rerun phase with reviewer feedback
- **Merge + cleanup** — merge, test, clean worktree
- **Codex CLI adapter** — validates agent-agnostic architecture

**Exit:** `batty work all` runs multiple phases safely with human review and rework loop.

---

## Phase 3B: AI Director Review (Upgrade)

Add automated review once the human-gated sequencer is stable.

- **Director review agent** — diff + summary + logs → merge / rework / escalate
- **Director decision policy** — explicit autonomy tier and escalation rules
- **Director rework orchestration** — automatic re-run after rework decision
- **Audit trail** — all director decisions logged and reviewable

**Exit:** Director can reliably review and route decisions with human override.

---

## Phase 4: Parallel Execution

`batty work all --parallel N` — multiple tmux windows, one per phase.

- Git worktree per parallel phase
- Merge queue — serialize merges, rebase, re-test
- tmux window switching to monitor multiple phases

---

## Phase 5: Polish + Ship

Target: 10 users, 1 GitHub star.

- Config file, error handling, crash recovery
- CLI completions (zsh/bash/fish)
- README, demo GIF, `cargo install batty-cli`, Homebrew
- Blog post explaining the workflow model

---

## Tech Stack

| Layer | Choice |
|---|---|
| Core | Rust (clap + tokio) |
| Runtime | tmux |
| Tasks | kanban-md |
| Config | TOML |
| Logs | JSON lines |

---

## Risks

1. **Prompt detection** — parsing raw PTY output. Mitigated by two-tier architecture (regex + supervisor agent).
2. **Unsafe automation** — mitigated by policy tiers (observe → suggest → act) and audit logs.
3. **tmux compatibility** — tmux behavior differs by version. Mitigated by capability detection and compatibility paths.
4. **Adoption gap** — claims about speed and orchestration must be measured. Mitigated by explicit dogfood and user benchmarks.

---

## Scope Cutting

Cut last first:

1. Phase 5 (polish) — ship rough.
2. Phase 4 (parallel) — sequential proves the thesis.
3. Phase 3B (AI director) — human review in 3A is enough to ship.
4. Phase 3A rework automation details — can start with manual retries.
5. Phase 2 supervisor depth — human can answer more questions manually.
6. Phase 1 — **never cut.**
