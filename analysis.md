# Batty: Research and Feasibility Analysis

Date: 2026-02-21

## Executive Summary

The idea is viable. Most individual building blocks already exist in separate tools, but no project cleanly combines all of them into one fast, agent-native terminal platform.

The strongest strategic insight: **do not start by building a terminal renderer from scratch**. Start with a control plane for agent orchestration, pane/session control, and rich content surfaces; build on existing PTY/terminal tech first, then optimize rendering later if needed.

## What You Want to Build (Condensed)

- A very fast terminal superset (normal terminal by default, advanced when needed).
- First-class orchestration of foreground/interactive processes.
- Programmable panes/layouts via commands and shortcuts.
- Agent runner that can:
  - detect/handle agent questions,
  - execute tests as definition-of-done gates,
  - auto-complete workflow actions (commit/merge) under policy.
- Rich rendering: Markdown-first UX, Mermaid diagrams, and board/card-like interfaces.
- Cross-platform app: macOS, Linux, Windows.
- Plugin/extension ecosystem.

## Research Findings: Similar Projects

### 1) AI-native and agent-adjacent terminals

### Wave Terminal
- Positioned as an open-source, AI-native terminal with workspace/block UX.
- Supports command blocks, sessions, history, and richer surfaces than plain VT text.
- Directly adjacent to your vision of terminal + workflow + AI.
- Gap vs Batty: less emphasis on deterministic "agent governance" (DoD gating, policy engine for autonomous answers/merges).

### Superset Terminal
- Multi-agent terminal concept with parallel coding agents and worktree-based flows.
- Repo: `superset-sh/superset` ("The command center for coding agents").
- TypeScript-heavy implementation focused on coordinating multiple external agents.
- Useful proof that "agent control plane over terminal sessions" is practical.
- Gap vs Batty: stronger on agent management notifications, weaker on minimal terminal baseline + deterministic orchestration + extensible pane-app platform model.

### Warp
- Built around Rust + GPU rendering + custom UI toolkit.
- Demonstrates demand for a modern terminal with productized UX and speed focus.
- Gap vs Batty: product orientation differs from "operator-programmable agent OS in terminal."

### 2) High-performance/scriptable terminal foundations

### WezTerm
- Strong programmable/multiplexer model and pane control (`wezterm cli split-pane ...`).
- Lua event/hooks API supports automation patterns.
- Good evidence that command-driven pane orchestration is already feasible.
- Limitation for Batty goals: still fundamentally a terminal/mux model, so UX and control semantics inherit tmux-like constraints rather than an agent-orchestration-first model.

### kitty
- Has remote-control mode and a command API (`kitten @ ...`) for external orchestration.
- Confirms that external process control over terminal state is practical.

### Zellij
- Built-in multiplexer with CLI actions and plugin model (WASM/WASI-based plugins).
- Shows a credible plugin architecture path for terminal-native extensions.

### tmux (control mode)
- Supports machine-readable control mode, i.e., programmatic interaction with sessions/panes.
- Important counterpoint: some "impossible in current terminal" claims are already partially solved, but UX is fragmented and developer-hostile.
- Practical issue: modal copy/scroll behavior is a known pain point for many users; this is exactly the kind of baseline friction Batty should remove with a minimal, non-modal default UX.

### Ghostty
- High-performance native terminal focus (Zig + platform-native UI integration).
- Roadmap currently indicates no plugin/config-language direction.
- Useful as performance benchmark philosophy, less useful as plugin ecosystem foundation today.

### 3) Markdown/Mermaid and rich rendering

### Markdown in terminal
- Tools like `glow` prove good Markdown rendering in terminal contexts is solved.

### Mermaid
- `mermaid-cli` demonstrates reliable diagram rendering from markdown-adjacent workflows.

### Constraint that remains
- Rich inline graphics are still fragmented across terminal protocols (kitty/iTerm2/sixel ecosystems), so cross-terminal portability is weak.
- For Batty, owning your app surface (not depending on external terminal support) is the safer route.

## Gap Analysis: What Is Still Missing in Market

No single project currently combines all of this well:

- Fast terminal core.
- Strong pane/process orchestration UX for humans + agents.
- Policy-driven agent supervision (question answering, tests, merge gates).
- Rich markdown/diagram/card surfaces in the same product.
- Extensible plugin runtime with stable APIs.

This gap is real and commercially/technically meaningful.

## Feasibility by Subsystem

1. Terminal + PTY + pane orchestration: **High feasibility**
2. Agent supervisor and workflow automation: **Medium-high feasibility**
3. Safe autonomous prompt answering and merge actions: **Medium feasibility** (policy + guardrails are hard)
4. Markdown/Mermaid rich rendering: **High feasibility** in owned UI surface
5. In-terminal board/cards interaction: **Medium-high feasibility**
6. Cross-platform consistency incl. Windows PTY specifics: **Medium feasibility**
7. Plugin ecosystem with stability/sandboxing: **Medium feasibility**

## Architecture Options (Speed Emphasis)

### Option A: Native core + native renderer (Rust/Zig end-to-end)

- Pros:
  - Best long-term latency and resource efficiency.
  - Full control over rendering pipeline.
- Cons:
  - Highest engineering risk/time-to-market.
  - Hardest cross-platform UI parity.

When to pick: only if you can fund a longer platform build before product validation.

### Option B: High-performance core + webview UI shell (recommended)

- Core daemon in Rust for:
  - PTY/session orchestration,
  - agent supervision/state machine,
  - policy engine and test gates,
  - plugin host boundaries.
- UI in Tauri/Electron-class shell for:
  - fast product iteration,
  - board/cards/markdown/mermaid surfaces,
  - multi-pane interaction model.

- Pros:
  - Fastest path to prove product value.
  - Keeps performance-critical path native.
  - Easier rich UI than pure VT rendering.
- Cons:
  - Some UI overhead vs fully native rendering.

When to pick: now.

### Option C: Fork/extend existing terminal (WezTerm/Zellij-like base)

- Pros:
  - Reuse mature terminal/mux foundation.
  - Lower early infra cost.
- Cons:
  - Architectural constraints from host project.
  - Harder to realize product-specific UX vision.

When to pick: if team size is small and short runway is critical.

## Recommended Build Strategy (Pragmatic)

Phase 1 (6-10 weeks): control-plane MVP
- Multi-pane/session orchestration commands.
- Agent runner wrapper with:
  - question detection,
  - policy-based auto-response (limited scope),
  - test-gated completion.
- Markdown-first viewer + Mermaid rendering panel.
- Basic card board linked to task execution.

Phase 2 (8-12 weeks): plugin + policy hardening
- Stable plugin API (versioned).
- Plugin sandbox model and permission prompts.
- Improved automation safety for commit/merge operations.
- Telemetry for latency, crash rates, false-positive auto-replies.

Phase 3: performance and renderer optimization
- Profile hotspots before rewriting rendering stack.
- Decide whether to keep webview shell or migrate selected surfaces to native.

## Key Technical Risks and Mitigations

1. Unsafe automation behavior
- Mitigation: explicit policy tiers (`observe`, `suggest`, `act-with-approval`, `fully-auto`) and auditable action logs.

2. Cross-platform PTY edge cases (especially Windows)
- Mitigation: isolate PTY adapter layer early; run CI on all three OSes from week 1.

3. Plugin API instability
- Mitigation: versioned contracts and capability-based permissions from first release.

4. Performance regressions from rich UI
- Mitigation: strict budgets (startup, input latency, frame time), continuous profiling, and defer native rewrite until measured need.

## Go/No-Go Assessment

Go.

Reason: the concept addresses a real fragmentation gap and is technically feasible with an incremental architecture. The success determinant is not renderer novelty; it is the quality of the **agent control plane + safe automation model + extensibility**.

## Refined Product Direction (After Tool Evaluation)

Batty should be:

- Minimal by default (terminal first, zero UI clutter).
- Progressive by demand (panes, sidebars, apps appear only when needed).
- Orchestration-first (run control, policies, DoD gates, recovery).
- Platform-oriented (pane-native apps and extensions can integrate deeply).

This keeps the core fast while still enabling Wave/Superset-class capabilities as optional layers.

## GitHub Stars (as of 2026-02-21, via `gh repo view`)

- `wavetermdev/waveterm`: 17,463
- `superset-sh/superset`: 1,876
- `wezterm/wezterm`: 24,374
- `kovidgoyal/kitty`: 31,421
- `zellij-org/zellij`: 29,283
- `tmux/tmux`: 42,040
- `ghostty-org/ghostty`: 44,244
- `charmbracelet/glow`: 22,939
- `mermaid-js/mermaid-cli`: 4,171
- `warp` project: no public GitHub repository in this report

## Source Links

- Wave Terminal repo: https://github.com/wavetermdev/waveterm
- Superset Terminal repo: https://github.com/superset-sh/superset
- WezTerm CLI split-pane docs: https://wezterm.org/cli/cli/split-pane.html
- WezTerm Lua API reference: https://wezterm.org/config/lua/wezterm/
- kitty remote control docs: https://sw.kovidgoyal.net/kitty/remote-control/
- Zellij features/docs: https://zellij.dev/documentation/features
- Zellij CLI actions docs: https://zellij.dev/documentation/cli-actions
- tmux control mode notes: https://github.com/tmux/tmux/wiki/Control-Mode
- Ghostty roadmap: https://github.com/ghostty-org/ghostty/blob/main/docs/ROADMAP.md
- Warp architecture blog (Rust + GPU + toolkit): https://www.warp.dev/blog/how-warp-works
- Glow (Markdown rendering): https://github.com/charmbracelet/glow
- Mermaid CLI: https://github.com/mermaid-js/mermaid-cli
