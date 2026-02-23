---
id: 6
title: Shell completions and crate publishing
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T01:18:49.842054087-05:00
started: 2026-02-23T01:11:32.641513487-05:00
completed: 2026-02-23T01:18:49.82018276-05:00
tags:
    - ship
    - polish
class: standard
---

Ship quality-of-life polish and make Batty installable via `cargo install`.

## Requirements

### Shell Completions
- Generate completions for zsh, bash, and fish via `clap_complete`
- New subcommand: `batty completions <shell>` that prints the completion script to stdout
- Add install instructions to README

### Crate Publishing
- Ensure `Cargo.toml` has correct metadata: name (`batty-cli`), description, license, repository, keywords
- `cargo publish --dry-run` passes
- Verify `cargo install batty-cli` works from a clean environment

## Implementation Notes

- `clap_complete` is a single dependency addition
- Completions subcommand is ~20 lines of code
- Crate metadata is just Cargo.toml fields — no code changes

[[2026-02-23]] Mon 01:18
Implemented shell completion support with new CLI subcommand `batty completions <shell>` (bash/zsh/fish) and script generation in src/shell_completion.rs. Updated CLI parsing/tests, main command dispatch, README command table, and README installation/completion instructions; updated docs/reference/cli.md to include completions command usage. Updated package metadata in Cargo.toml for publishing: package name batty-cli, explicit batty binary mapping, repository/homepage/readme/keywords/categories. Validation run: cargo test (all passing), cargo install --path . --force --locked (successful local install, executables batty/docsgen), cargo publish --dry-run --allow-dirty (packaged + verified successfully; upload aborted due dry-run).
