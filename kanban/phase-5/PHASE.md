# Phase 5: Polish + Ship

**Status:** Not Started
**Board:** `kanban/phase-5/`
**Depends on:** Phase 3A complete (Phase 3B and Phase 4 optional)

## Goal

Ship Batty to first users. Target: 10 users, 1 GitHub star. Focus on reliability, installation, documentation, and a demo that shows the workflow model.

## What Already Exists (from Phases 1-4)

- Full execution pipeline with tmux supervision
- Worktree isolation and runtime hardening
- `batty work <phase>` and `batty work all`
- Human review gate and optional AI director review
- Optional parallel execution
- Two agent adapters (Claude Code, Codex CLI)

## Tasks (4 total)

1. **Config and error handling** — `.batty/config.toml` with sensible defaults. Graceful error messages. Crash recovery (detect stale tmux sessions, clean up worktrees). Validate config on startup.
2. **CLI completions** — Shell completions for zsh, bash, fish via clap's built-in derive.
3. **README, demo GIF, and docs** — Rewrite README for new users. Record a demo GIF/asciinema showing `batty work <phase>`. Getting-started guide. `cargo install batty-cli` works. Homebrew tap.
4. **Phase 5 exit criteria** — `cargo install batty-cli` works. README is clear. Demo shows the workflow. At least 1 real user has tried it.

## Key Technical Notes

- Use `clap_complete` for shell completions
- `asciinema` for recording terminal demos
- Config should cover: default agent, policy tier, test command, tmux preferences
- Error messages should suggest fixes, not just report failures
- README should lead with the workflow model (see `planning/differentiation.md`), tool is secondary

## Exit Criteria

- Clean install path (`cargo install batty-cli` or Homebrew)
- README explains the workflow model and shows the tmux layout
- Demo GIF/recording shows a real phase execution
- Config file with good defaults and documentation
- No crash on common error paths (missing tmux, missing kanban-md, bad config)

## Kanban Commands

```bash
kanban-md board --compact --dir kanban/phase-5
kanban-md pick --claim <agent> --status backlog --move in-progress --dir kanban/phase-5
kanban-md show <ID> --dir kanban/phase-5
kanban-md move <ID> done --dir kanban/phase-5
```

## Reference Docs

- `planning/roadmap.md` — ship targets
- `planning/differentiation.md` — messaging and positioning
- `planning/peter-switch-strategy.md` — target user needs
- `CLAUDE.md` — agent instructions
