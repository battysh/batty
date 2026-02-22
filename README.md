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

By default, `work` starts tmux detached and backgrounds Batty supervision.
Use `--attach` to immediately enter the tmux session in the same terminal.
Each run also gets an isolated git worktree branch like `phase-2-4-run-001`.

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

[policy.auto_answer]
"Continue? [y/n]" = "y"
```

Reconnect to an existing session:

```sh
batty attach phase-2.4
```

### Minimal Command

```sh
batty work phase-2.4
```

A tmux session opens. The executor (Claude Code, Codex, Aider) works through the phase board — picking tasks, implementing, testing, committing. Batty auto-answers routine prompts and shows everything in the orchestrator pane.
If the phase branch is merged back to the base branch, Batty cleans up the run worktree automatically; otherwise it keeps the worktree for inspection/rework.

```sh
batty work all                  # chain phases sequentially
batty work all --parallel 3     # three phases in parallel
batty attach phase-2.4          # reconnect to a running session
```

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

Phase 1 complete. Phase 2 complete. Phase 2.4 (supervision harness validation) is next. Phase 2.5 (runtime hardening + dogfood) follows.

## Links

- Website: [batty.sh](https://batty.sh)
- GitHub: [github.com/battysh/batty](https://github.com/battysh/batty)
- Discord: [discord.gg/battyterm](https://discord.gg/battyterm)
- Twitter: [@battyterm](https://twitter.com/battyterm)
- Bluesky: [@battyterm.bsky.social](https://bsky.app/profile/battyterm.bsky.social)

## License

MIT
