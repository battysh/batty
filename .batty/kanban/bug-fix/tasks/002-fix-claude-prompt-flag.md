---
id: 2
title: Fix Claude Code adapter using invalid --prompt flag
status: backlog
priority: critical
tags: [bug, agent-adapter]
---

## Bug Description

The Claude Code adapter in interactive mode (`src/agent/claude.rs`, line 78) uses `--prompt` as a flag to pass the initial task description. However, Claude Code CLI does not have a `--prompt` flag. The actual error:

```
error: unknown option '--prompt'
(Did you mean one of --from-pr, --print?)
```

This means the agent never starts in interactive mode.

## Root Cause

`ClaudeCodeAdapter::spawn_config()` for `ClaudeMode::Interactive` does:
```rust
args.push("--prompt".to_string());
args.push(task_description.to_string());
```

But Claude Code's CLI expects:
- `-p` / `--print` for non-interactive print mode (with prompt as positional arg)
- For interactive mode, the prompt is passed as a positional argument (the last arg), or the user types it in

## Fix Approach

For interactive mode, the task description should be passed as the last positional argument to `claude`, not with `--prompt`. The Claude CLI accepts the prompt text as the final positional argument when launched interactively:

```
claude "Your task description here"
```

Change the interactive spawn config from:
```rust
args.push("--prompt".to_string());
args.push(task_description.to_string());
```
to:
```rust
args.push(task_description.to_string());
```

Also update the unit test `interactive_mode_uses_prompt_flag` which currently asserts on `--prompt`.

## Files to Modify

- `src/agent/claude.rs` — `spawn_config()` method (line 74-80) and unit tests

## How to Verify

1. Run `batty work <phase> --dry-run` and confirm the launch command no longer includes `--prompt`
2. Run an actual agent execution and confirm Claude starts successfully
3. Unit tests pass: `cargo test agent::claude`
