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

All 11 tasks done. Current project test inventory is 394 tests.

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

## Phase 2.4: Supervision Harness Validation (Done)

Make supervisor behavior provable before runtime hardening.

- **Deterministic harness contract** — scenario matrix with expected supervisor outcomes
- **Real tmux invariants** — executor pane targeting and persistent UI checks
- **Mock matrix in tmux** — deterministic executor/supervisor fixtures
- **Real supervisor with mocked executor** — Claude + Codex integration checks
- **Real supervisor+executor smoke runs** — opt-in end-to-end validation paths
- **Prompt catalog + runbook** — stable prompts, env flags, and pass/fail criteria

**Exit:** Harness suite passes in real tmux, with documented real-agent integration and smoke tests.

---

## Phase 2.5: Runtime Hardening + Dogfood (Done)

Run Batty on its own hardening phase and close the reliability gaps that block day-to-day use.

- **Worktree isolation first** — every phase run uses a worktree before AI review exists
- **Prompt composition** — deterministic launch context: `CLAUDE.md` + `PHASE.md` + board state + config
- **Completion detection contract** — define exactly when a phase is complete
- **Mid-phase recovery** — Batty reconnects and resumes supervision after process crash
- **tmux capability checks** — detect version/features and degrade gracefully
- **Dogfood gate** — Batty executes phase-2.5 end-to-end and merges with review

**Exit:** We complete a real Batty development phase using Batty itself.

---

## Phase 2.6: Backlog Rollover from 2.5 (Done)

Close out the rolled-over reliability and developer-experience work from 2.5.

- **Dogfood completion** — Batty executes phase-2.6 against its own board
- **Install workflow** — `batty install` for Claude/Codex steering + skills
- **Config output polish** — improved `batty config` output (including JSON mode)
- **Build hygiene** — compiler warning cleanup
- **Lint workflow** — `make lint` / `make lint-fix` and CI checks

**Exit:** Remaining 2.5 backlog items are merged and stable.

---

## Phase 2.7: Minor Improvements (Done)

Ship low-risk quality and workflow improvements after hardening.

- **Supervisor hotkeys** — pause/resume control in tmux sessions
- **Dangerous-mode wrappers** — safer command execution boundaries
- **Tier 2 context snapshots** — persisted supervisor context for debugging/audit
- **Secret redaction guardrail** — redact likely secret-bearing lines before persistence
- **Docs pipeline improvements** — generated docs and consistency cleanups

**Exit:** Minor improvements are merged without regressions in core workflows.

---

## Phase 3A: Sequencer + Human Review Gate (Done)

Separate phase chaining from AI evaluation so we can ship useful automation earlier.

- **`batty work all`** — phase sequencer, runs phases in order
- **Phase summary + review packet** — standardized artifacts for human review
- **Human review gate** — merge / rework / escalate decisions without director agent
- **Rework loop** — rerun phase with reviewer feedback
- **Merge + cleanup** — merge, test, clean worktree
- **Phase ordering and dependency handling** — deterministic sequencing from board metadata

**Exit:** `batty work all` runs multiple phases safely with human review and rework loop.

---

## Phase 3B: AI Director Review (Done)

Add automated review once the human-gated sequencer is stable.

- **Director review agent** — diff + summary + logs → merge / rework / escalate
- **Director decision policy** — explicit autonomy tier and escalation rules
- **Director rework orchestration** — automatic re-run after rework decision
- **Audit trail** — all director decisions logged and reviewable

**Exit:** Director can reliably review and route decisions with human override.

---

## Phase 4: Parallel DAG Scheduler, Merge Queue, Ship (Done)

`batty work <phase> --parallel N` — DAG-aware parallel agent execution.

- **Task dependency DAG** — topological sort with cycle detection from task frontmatter
- **Parallel agent spawner** — per-agent worktrees, tmux windows, slot management
- **DAG scheduler** — dispatches ready tasks to idle agents as dependencies complete
- **Merge serialization queue** — FIFO merge with rebase, conflict detection, retry
- **Parallel status bar** — multi-agent progress in tmux status line
- **Shell completions** — bash/zsh/fish via `batty completions`
- **`batty merge` command** — orchestrated worktree merge back into main
- **Board sync** — uncommitted kanban changes propagated into worktrees

**Exit:** Parallel execution works end-to-end with DAG scheduling and serialized merges. Phase 5 (polish) consolidated here — completions, README, and `cargo install batty-cli` are shipped.

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

All planned phases are complete. Phase 5 was consolidated into Phase 4.

Original cut priority (preserved for reference):

1. Phase 5 (polish) — shipped as part of Phase 4.
2. Phase 4 (parallel) — done.
3. Phase 3B (AI director) — done.
4. Phase 3A rework automation — done.
5. Phase 2 supervisor depth — done.
6. Phase 1 — **never cut.**
