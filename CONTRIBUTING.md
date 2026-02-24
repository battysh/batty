# Contributing to Batty

Thanks for your interest in contributing to Batty! This document covers everything you need to get started.

## Prerequisites

- **Rust** (stable toolchain)
- **tmux** 3.2+
- **kanban-md** (`cargo install kanban-md`)
- An agent CLI: [Claude Code](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/overview) or [Codex](https://github.com/openai/codex)

## Development Setup

```sh
git clone https://github.com/battysh/batty.git
cd batty
cargo build
cargo test
```

All tests must pass before submitting a PR. The CI runs on both Ubuntu and macOS.

## Making Changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Add or update tests for any new functionality
4. Run `cargo test` and `make lint` locally
5. Write a clear commit message (see below)
6. Open a PR against `main`

## Commit Messages

Follow this format:

```
<scope>: <short summary>

What: <what was implemented/changed>
Why: <why this approach was chosen>
How: <key implementation details>
```

Keep the first line under 72 characters. The body should give enough context that `git log` tells the full story.

## Testing

Every module must have unit tests. Use `#[cfg(test)]` modules in each source file. Focus on:

- Happy paths and error conditions
- Prompt detection and supervisor logic (the hard part)
- Edge cases in task/board parsing

```sh
cargo test              # run all tests
cargo test <module>     # run tests for a specific module
make lint               # clippy + rustfmt check
```

## Code Style

- **Minimal code.** Build the smallest thing that works.
- **No premature abstraction.** Three similar lines > one clever abstraction.
- **Compose, don't monolith.** Use existing CLI tools where possible.
- **Markdown as backend.** All state in human-readable, git-versioned files.

See `planning/dev-philosophy.md` for the full philosophy.

## AI-Assisted PRs Welcome

If you used an AI tool to help write your contribution, that's fine! Just:

- Mark the PR as AI-assisted in the description
- Confirm you've reviewed and tested the generated code
- Make sure tests pass and the code follows project conventions

## Reporting Bugs

Use the [bug report template](https://github.com/battysh/batty/issues/new?template=bug_report.yml). Include:

- What you expected vs. what happened
- Steps to reproduce
- Your OS, tmux version, and agent CLI version
- Relevant log output (check `.batty/logs/`)

## Requesting Features

Use the [feature request template](https://github.com/battysh/batty/issues/new?template=feature_request.yml). Describe the problem you're trying to solve, not just the solution you want.

## Project Structure

```
src/               # Rust source
docs/              # User documentation (MkDocs)
planning/          # Architecture, roadmap, philosophy
assets/            # Static assets
scripts/           # Utility scripts
.batty/kanban/     # Phase boards
```

## Questions?

Open a [discussion](https://github.com/battysh/batty/discussions) or file an issue. We're happy to help.
