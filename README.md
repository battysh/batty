# Batty

**A workflow model for building with agents.**

Batty reads your project board, launches coding agents in tmux, supervises their work, auto-answers routine prompts, escalates real questions, gates on tests, and merges results.

You design phases. Batty executes them.

## Quick Start

### Install + Run (Local Dev)

From this repository root:

```sh
# 1) Run without installing globally
cargo run -- config
cargo run -- work phase-2.4
```

`batty config` has two output modes:

```sh
batty config          # concise human-readable sections
batty config --json   # machine-readable JSON for scripts
```

By default, `work` starts tmux detached and backgrounds Batty supervision.
Use `--attach` to immediately enter the tmux session in the same terminal.
By default, Batty runs directly in your current branch/worktree.
Use `--worktree` to opt into isolated phase worktrees.
Use `--worktree --new` to force a fresh isolated run branch like `phase-2-4-run-005`.

Optional global install:

```sh
# 2) Install the batty binary to ~/.cargo/bin
cargo install --path .

# 3) Verify and run
batty config
batty work phase-2.4
```

If `batty` is not found after install, add Cargo bin to your PATH:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

If you want to force Codex for executor on a run:

```sh
batty work phase-2.4 --agent codex
```

If you want immediate interactive mode:

```sh
batty work phase-2.4 --attach
```

If you want to force a new run worktree instead of resuming:

```sh
batty work phase-2.4 --worktree --new
```

If you want to inspect the exact composed launch context without starting the executor:

```sh
batty work phase-2.4 --dry-run
```

Example `.batty/config.toml`:

```toml
[defaults]
agent = "codex"
policy = "act"

[supervisor]
enabled = true
program = "claude"
args = ["-p", "--output-format", "text"]
timeout_secs = 60
trace_io = true

[detector]
silence_timeout_secs = 3
answer_cooldown_millis = 1000
unknown_request_fallback = true
idle_input_fallback = true

[dangerous_mode]
enabled = false

[policy.auto_answer]
"Continue? [y/n]" = "y"
```

To force dangerous-mode flags for both executor and supervisor commands, set:

```toml
[dangerous_mode]
enabled = true
```

When enabled, Batty prepends:
- `--dangerously-skip-permissions` for `claude`
- `--dangerously-bypass-approvals-and-sandbox` for `codex`

Batty logs effective executor/supervisor launch commands in the orchestrator log pane so you can verify the final program/args used at runtime.

Reconnect to an existing session:

```sh
batty attach phase-2.4
```

Resume Batty supervision after a crash/restart (reuses the active tmux run):

```sh
batty resume phase-2.4
# or
batty resume batty-phase-2-4
```

Open `kanban-md` TUI for the active run board:

```sh
batty board phase-2.4
```

Print the resolved board directory (for scripts):

```sh
batty board phase-2.4 --print-dir
```

List all phase boards with status and task counts:

```sh
batty board-list
```

Initialize Batty in the current project (checks/installs required tools and writes steering assets):

```sh
batty install                       # both agents (default)
batty install --target claude      # Claude only
batty install --target codex       # Codex only
batty install --dir /tmp/demo      # explicit destination
```

`batty install` bootstraps external dependencies:

- checks `tmux` and `kanban-md` on `PATH`
- attempts automatic install if missing (best effort):
  - `tmux`: Homebrew or `sudo` package manager install (`apt-get`/`dnf`/`pacman`)
  - `kanban-md`: `cargo install kanban-md --locked` (or Homebrew when available)

Then it writes:

- `.claude/rules/batty-workflow.md` (Batty workflow rules — auto-loaded by Claude Code alongside your existing CLAUDE.md)
- `.agents/rules/batty-workflow.md` (same content — auto-loaded by Codex alongside your existing AGENTS.md)
- `.batty/skills/claude/SKILL.md`
- `.batty/skills/codex/SKILL.md`

And installs kanban-md skills (`kanban-md` and `kanban-based-development`) for the selected agent(s).

Kanban boards default to `.batty/kanban/` for new projects. Existing projects with boards at `kanban/` continue to work — Batty resolves the active location automatically.

Batty never touches your existing `CLAUDE.md` or `AGENTS.md` — workflow instructions are installed as rules files that agents load automatically alongside your project's own steering docs.

Remove installed Batty assets from a project:

```sh
batty remove                       # remove both agents' assets (default)
batty remove --target claude      # Claude only
batty remove --target codex       # Codex only
batty remove --dir /tmp/demo      # explicit target directory
```

`batty remove` deletes the workflow rules, skill files, and kanban-md skills that `batty install` created. It prints each removed/not-found file and a reminder to `rm -rf .batty` for full cleanup (worktrees under `.batty/worktrees/` may contain local branches).

Note: "phase" is Batty's prescribed unit of work, but it maps to whatever your team calls a sprint, story, milestone, or iteration.

Lint workflow (stable command names):

```sh
make lint      # fmt check + strict clippy
make lint-fix  # fmt + clippy --fix for local cleanup
```

Expanded commands used by the aliases:

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
cargo clippy --fix --allow-dirty --allow-staged
```

CI runs the strict lint checks on every pull request and blocks merges on failures.

### Minimal Command

```sh
batty work phase-2.4
```

A tmux session opens. The executor (Claude Code, Codex, Aider) works through the phase board — picking tasks, implementing, testing, committing. Batty auto-answers routine prompts and shows everything in the orchestrator pane.
By default, Batty uses your current branch/worktree. Pass `--worktree` to resume the latest phase worktree, or `--worktree --new` to force a fresh run.
If the phase branch is merged back to the base branch, Batty cleans up the run worktree automatically; otherwise it keeps the worktree for inspection/rework.

```sh
batty attach phase-2.4          # reconnect to a running session
batty resume phase-2.4          # resume supervision for an existing run
```

Planned (not implemented yet): `batty work all` and `batty work all --parallel <N>`.

## What You See

```
┌── tmux: batty-phase-1 ──────────────────────────┐
│                                                   │
│  Claude Code working on task #7...                │
│  Creating src/supervisor/mod.rs                   │
│  Running cargo test... 14 passed                  │
│                                                   │
├───────────────────────────────────────────────────┤
│  [batty] ✓ auto-answered: "Continue?" → y          │
│  [batty] ? supervisor thinking: "async or sync?"   │
│  [batty] ✓ supervisor answered → async             │
├───────────────────────────────────────────────────┤
│ [batty] phase-1 | task #8 | 7/11 done | ✓ active │
└───────────────────────────────────────────────────┘
```

Type into the executor pane anytime — human input always takes priority. Detach and re-attach. Session survives disconnect.

## Supervisor Hotkeys

During an active tmux-supervised run:
- Pause supervision: `Prefix + Shift+P` (`C-b P` with default tmux prefix)
- Resume supervision: `Prefix + Shift+R` (`C-b R` with default tmux prefix)

Expected status behavior:
- Pause sets status to `● PAUSED — manual input only`
- Resume returns status to `✓ supervising`

While paused:
- Batty does not inject Tier 1 auto-answers
- Batty does not call Tier 2 supervisor responses
- You can continue typing directly in the executor pane

Troubleshooting:
- If pause/resume appears ignored, confirm you are attached to the Batty tmux session (not another tmux server/session).
- If prompts are not being auto-answered, check whether status is still `PAUSED`; this is expected until resume.
- Repeated pause/resume presses are no-ops and logged (already paused / already supervising).

## Tier2 Context Snapshots

When Tier 2 supervisor intervention is invoked, Batty writes a per-call context snapshot under:

- `.batty/logs/<run>/tier2-context-<n>.md`

The orchestrator log includes a concise reference to each snapshot path.

Safety guardrail:
- Snapshot persistence uses deterministic keyword-based line redaction for likely secret-bearing lines (for example authorization/token/password markers) before writing context to disk.

## How It Works

Three layers:

- **Director** — reviews completed phases → merge / rework / escalate
- **Supervisor** — watches executor, answers questions using project context
- **Executor** — BYO agent (Claude Code, Codex, Aider) working through the board

Two-tier prompt handling:
- **Tier 1** — regex match → instant auto-answer (~70-80% of prompts)
- **Tier 2** — supervisor agent with project context → intelligent answer
- **Unknown fallback** — if output goes silent and no regex matches, Batty asks the supervisor anyway

Everything runs in tmux. Output captured via `pipe-pane`. Answers injected via `send-keys`. Status in tmux status bar. Events in orchestrator pane.

## tmux Compatibility

Batty probes tmux capabilities on each `work`/`resume` startup and logs:
- tmux version
- `pipe-pane` support (required)
- `pipe-pane -o` support (resume safety optimization)
- status style option support
- log-pane split mode (`-l` lines or `-p` percent fallback)

Compatibility matrix:

| tmux version | Status | Behavior |
| --- | --- | --- |
| `>= 3.2` | Known-good | Full feature path (`pipe-pane -o`, styled status bar, `split-window -l`) |
| `3.1.x` | Supported with fallbacks | Uses compatible split/status behavior where needed |
| `< 3.1` | Not supported | Batty fails fast when required `pipe-pane` capability is missing |

If required capabilities are missing, Batty exits with remediation guidance to install/upgrade tmux (recommended `>= 3.2`).

## Architecture

```
┌─────────────────────────────────────────┐
│  tmux                                   │
│  Panes · pipe-pane · send-keys          │
│  Status bar · Session persistence       │
├─────────────────────────────────────────┤
│  Batty (Rust)                           │
│  Event extraction · Prompt handling     │
│  Policy engine · Test gates · Logging   │
├─────────────────────────────────────────┤
│  kanban-md                              │
│  Markdown task boards — command center  │
└─────────────────────────────────────────┘
```

## Philosophy

- **Compose, don't monolith.** tmux + kanban-md + BYO agents. Build only the orchestration layer.
- **Markdown as backend.** All state is human-readable, git-versioned files.
- **Earn autonomy progressively.** observe → suggest → act. Trust is earned, not assumed.
- **Ship fast.** Use existing tools. Validate with real users.

## Project Status

- Phases 1, 2, 2.4, 2.5, and 2.6 are complete.
- Phase 2.7 is in progress.
- Phases 3A and later are planned.

## Links

- Website: TBD
- GitHub: [github.com/battysh/batty](https://github.com/battysh/batty)
- Discord: TBD
- Twitter/X: TBD
- Bluesky: TBD

## License

MIT
