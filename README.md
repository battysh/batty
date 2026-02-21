# Batty

**Use the best agents. Batty controls execution.**

Batty is a terminal-native agent orchestration platform. It works as a normal terminal — just faster — and adds policy-based supervision, test-gated completion, and rich rendering on top.

## What is Batty?

- **Terminal superset** — works as a normal terminal by default, zero UI clutter. Panes, sidebars, and rich surfaces appear only when you need them.
- **Agent orchestration** — run any CLI agent (Claude Code, Codex CLI, Aider, others) with policy-based supervision and lifecycle control.
- **Test-gated completion** — tasks aren't "done" until tests pass. Completion is measured, not guessed.
- **Rich rendering** — Markdown documents and Mermaid diagrams rendered in panes, not pseudo-formatted with escape codes.
- **Extensible** — pane-native apps and WASM plugins via stable APIs.

## Quick Start

```sh
curl -fsSL https://batty.sh/install | sh
```

Launch a terminal:

```sh
batty
```

Split panes and run commands:

```sh
batty split --horizontal
batty split --vertical
batty focus <pane-id>
batty list
```

Run an agent with supervision:

```sh
batty run --agent "claude -p 'fix the bug in auth.py'"
```

View rendered markdown:

```sh
batty view README.md
cat doc.md | batty view
```

## Agent Runner

This is the core differentiator. No existing tool combines a fast terminal with deterministic agent governance.

### Policy tiers

Every autonomous action flows through an explicit policy:

| Tier | Behavior |
|---|---|
| `observe` | Log only — Batty watches, you drive |
| `suggest` | Show suggestion in supervisor pane, you confirm |
| `act-with-approval` | Auto-respond to routine prompts, escalate unknowns |
| `fully-auto` | Autonomous execution under defined constraints |

Policies are defined per-project in TOML:

```toml
[policy]
tier = "act-with-approval"

[policy.auto_answer]
"Continue? [y/n]" = "y"

[definition_of_done]
command = "pytest tests/"
pass_pattern = "passed"
```

### Test-gated completion

When an agent signals completion, Batty runs your definition-of-done command. If tests pass, the task is done. If tests fail, failure output is fed back to the agent for retry.

### Audit trail

Every automated action — prompt answers, test runs, commits — is logged to a structured event timeline. Inspect any autonomous decision after the run.

### Agent-agnostic

Batty doesn't ship its own AI. Swap Claude Code for Codex CLI without changing your workflow. Agents are first-class, never locked in.

## Pane Orchestration

Programmable multi-pane layouts via CLI commands or keyboard shortcuts. Non-modal by default — no tmux-style prefix key required.

```sh
batty split --horizontal    # Split current pane horizontally
batty split --vertical      # Split current pane vertically
batty close                 # Close current pane
batty focus <pane-id>       # Focus a specific pane
batty list                  # List all active panes
```

When `batty run --agent` launches, a companion supervisor pane opens automatically showing agent status, detected prompts, test results, and the audit log.

## Rich Rendering

```sh
batty view design.md        # Rendered markdown in a pane
batty view arch.md           # Mermaid diagrams rendered inline
cat notes.md | batty view    # Pipe support for agent-generated content
```

Markdown is rendered with syntax-highlighted code blocks and Mermaid diagrams — not approximated with terminal colors.

## Architecture

```
┌─────────────────────────────────────────┐
│  Frontend (WebView)                     │
│  TypeScript + Solid.js                  │
│  xterm.js · markdown-it · mermaid.js   │
├─────────────────────────────────────────┤
│  Tauri v2 IPC bridge                    │
├─────────────────────────────────────────┤
│  Rust Core                              │
│  portable-pty · tokio (async runtime)   │
│  wasmtime (WASM plugin host)            │
│  Agent supervisor state machine         │
│  Policy engine + test gate runner       │
│  Pane-native app SDK (JSON-RPC)         │
└─────────────────────────────────────────┘
```

- **Rust core** for PTY orchestration, agent supervision, policy engine, and plugin hosting. Performance-critical path stays native.
- **Tauri v2** for cross-platform app shell (macOS, Linux, Windows) without Electron's overhead.
- **xterm.js** for battle-tested terminal emulation.
- **WASM/WASI plugins** for sandboxed, language-agnostic extensions.

## Links

- Website: [batty.sh](https://batty.sh)
- GitHub: [github.com/battysh/batty](https://github.com/battysh/batty)
- Discord: [discord.gg/batty](https://discord.gg/batty)
- Twitter: [@battyterm](https://twitter.com/battyterm)
- Bluesky: [@batty.sh](https://bsky.app/profile/batty.sh)

## License

MIT
