# Batty: Development Philosophy

Date: 2026-02-21

## Core Principle: Compose, Don't Monolith

Batty is a thin orchestration layer that connects best-in-class CLI tools. It does not rebuild what already works. It makes existing tools work together better than they can alone.

This is the same pattern that made OpenClaw (Peter Steinberger) explode to 175k GitHub stars: don't build everything — build the glue that makes everything else more powerful.

## The Composability Pattern

```
┌──────────────────────────────────────────────┐
│  User                                        │
│  (keyboard, commands, shortcuts)             │
├──────────────────────────────────────────────┤
│  Batty (thin orchestration layer)            │
│  - tmux session orchestration                │
│  - agent supervision + policy engine         │
│  - lifecycle control (worktree, test, merge) │
│  - execution logging + audit                 │
├──────────────┬───────────┬───────────────────┤
│  kanban-md   │  claude   │  pytest / cargo   │
│  (tasks)     │  (agent)  │  (DoD gates)      │
├──────────────┼───────────┼───────────────────┤
│  git         │  codex    │  custom CLI tools  │
│  (worktrees) │  (agent)  │  (anything)        │
└──────────────┴───────────┴───────────────────┘
```

Each layer does one thing well. Batty coordinates them.

## Principles

### 1. Integrate First, Build Second

Before writing code, ask: does a good CLI tool already exist for this?

- Task management? Integrate kanban-md.
- Agent execution? Wrap Claude Code / Codex / Aider.
- Testing gates? Run the project's existing test commands.
- Version control? Use git worktrees natively.

Build custom only when no existing tool fits, or when the integration seam becomes a bottleneck.

### 2. CLI Tools Are the API

Every capability should be a CLI command that works independently. If `batty work 3` calls `kanban-md show 3` under the hood, then `kanban-md show 3` also works on its own outside of Batty. The user is never locked in.

This means:
- External tools remain first-class citizens, not wrapped-and-hidden dependencies.
- Users can mix Batty commands with any other CLI tool in scripts.
- Batty adds value by orchestrating, not by gatekeeping.

### 3. Own the Orchestration, Not the Tools

Batty's value is in the connections between tools, not in the tools themselves:

| What Batty owns | What Batty does NOT own |
|---|---|
| tmux session lifecycle | The terminal emulator |
| Agent supervision + policy | The AI model or agent |
| Task execution workflow | The task management data format |
| Test gating decisions | The test framework |
| Worktree lifecycle | Git itself |
| Audit trail | The tools being audited |

This keeps Batty small, fast, and replaceable at every layer except the orchestration layer — which is the product.

### 4. The Terminal Is the Platform

Batty is not a web app, not an IDE, not a dashboard. It is a terminal that can do more when you ask it to. Everything starts from the command line. Rich UI (panes, rendered markdown, boards) is opt-in, never mandatory.

This matches how power users actually work:
- Start minimal.
- Add complexity only when needed.
- Remove it when done.

### 5. Markdown as Backend

Markdown with YAML frontmatter is the universal data format for Batty. Not just for tasks — for everything: execution logs, policy configs, agent adapter definitions, session templates, run postmortems.

Why this works:

- **Agents already think in Markdown.** Every LLM outputs Markdown natively. No serialization layer, no format translation. The agent's natural output IS the data format.
- **Human-readable AND machine-readable.** YAML frontmatter is structured data any parser can consume. The Markdown body is natural language any agent can understand. Same file serves both purposes.
- **Git-native.** Every change is a diff. Every state transition is a commit. Versioning, branching, audit trail, and collaboration for free. No database migrations, no backup strategy — just files.
- **Zero infrastructure.** No database server, no API layer, no cloud dependency. Works offline, works on any machine, works in CI. `cat` is your query language, `grep` is your search engine.
- **Agents can create AND consume the format.** An agent can `kanban-md create "Fix the bug"` and another agent can read that task file and immediately understand it. The handoff between agents is a Markdown file with context in plain language — exactly what agents are best at processing.
- **Composable by default.** Any tool that reads files can participate. kanban-md doesn't need to know about Batty. Batty doesn't need to know about the next tool. The file is the API contract.
- **Progressive rendering.** The same Markdown file can be viewed as raw text (`cat`), styled terminal output (`glow`), or a rich rendered pane in Batty. The data doesn't change — only the presentation layer does.

This is a convergent pattern. OpenClaw stores everything as local Markdown. kanban-md chose Markdown. CLAUDE.md, AGENTS.md, and every agent config file is Markdown. The ecosystem is independently arriving at the same conclusion: Markdown is the native data format of the agent era.

Batty embraces this fully. Every piece of state in the system should be human-inspectable, agent-readable, git-versioned, and tool-agnostic. If it can't be expressed as a Markdown file, question whether it needs to exist.

### 6. Agents Are Processes, Not Features

Agents run in terminal panes as real interactive processes. The user can see what the agent is doing, type into the session, interrupt, or take over at any time. Batty supervises from the side — it doesn't replace the agent's native UX.

This is different from tools that hide agent execution behind a chat interface or progress bar. Transparency is non-negotiable.

### 7. Dogfood Everything

We use kanban-md to manage Batty development. We use Batty (as soon as it can run) to execute Batty tasks. Every feature we build is immediately tested by building the next feature with it.

If a workflow is painful for us, it will be painful for users. Fix it before shipping.

### 8. Fast Time-to-Market Over Purity

Shipping a working integration in hours beats building a perfect custom solution in weeks. We can always replace an integrated tool with a native implementation later — but only if real usage proves the need.

Premature ownership is waste. Premature abstraction is debt.

## Reference: The OpenClaw Pattern

OpenClaw (Peter Steinberger, 2025-2026) proved this philosophy at scale:

- **Skills system**: modular packages (Markdown/TypeScript) that define how to handle tasks — like kanban-md tasks + agent adapters for Batty.
- **Local gateway**: thin control plane that connects to external models and tools — like Batty's Rust core daemon.
- **BYO models**: users bring their own API keys for Claude, GPT, local models — like Batty's agent-agnostic adapter layer.
- **Composable CLI**: thin command layer that abstracts complexity while keeping everything accessible.

OpenClaw is a headless autonomous agent orchestrator (messaging-first).
Batty is a terminal-native supervised agent orchestrator (terminal-first).

Same philosophy, different surface. Both reject the monolith.

## Anti-Patterns (What We Avoid)

1. **Building our own AI** — We are not a model company. We orchestrate other people's models.
2. **Rebuilding existing CLI tools** — If `git worktree` works, use `git worktree`. Don't abstract it into a Batty-native concept.
3. **Feature gates behind Batty** — Everything Batty orchestrates should also work without Batty. The user chooses Batty for the orchestration, not because they're trapped.
4. **Dashboard-first thinking** — No always-on panels, no mandatory chrome beyond tmux status bar. Terminal first.
5. **Premature plugin API** — Don't design extension points until real usage patterns emerge. Ship the core, learn, then abstract.

## Decision Framework

When facing a build-vs-integrate decision:

```
Is there a good CLI tool for this?
  YES -> Integrate it. Move on.
    Does the integration become painful?
      YES -> Consider building a native replacement.
      NO  -> Keep the integration. It's working.
  NO  -> Build the minimum viable version. Ship it.
    Does usage reveal this needs to be richer?
      YES -> Invest more.
      NO  -> Leave it minimal.
```

## Bottom Line

Batty is not a product that does everything. Batty is the product that makes everything else work together, reliably, under control, with an audit trail.

The thinner the orchestration layer, the faster it ships, the easier it maintains, and the harder it is to compete with — because the value is in the connections, not the components.
