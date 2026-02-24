---
id: 3
title: batty install should scaffold default config.toml
status: backlog
priority: high
tags: [feature, install]
---

## Gap Description

`batty install` creates agent steering rules, skills, and gitignore entries but does NOT create `.batty/config.toml`. A user running `batty install` on a fresh project has no config file, which means batty falls back to compiled defaults. This is confusing because:

1. The user has no visible config to inspect or customize
2. `batty config` shows defaults but no source file path
3. Users must manually create the config file from scratch

## Fix Approach

Add a config scaffold step to `batty install` that creates `.batty/config.toml` with sensible documented defaults if it doesn't already exist. The scaffold should:

1. Create `.batty/config.toml` only if it doesn't exist (don't overwrite)
2. Include all config sections with their defaults
3. Have comments explaining each setting
4. Use `agent = "claude"` and `policy = "act"` as defaults

Example scaffold:

```toml
[defaults]
agent = "claude"        # Executor agent: claude, codex
policy = "act"          # Policy tier: observe, suggest, act
# dod = "cargo test"   # Definition-of-done command (uncomment and customize)
# max_retries = 3      # DoD retry count

[supervisor]
enabled = true
program = "claude"
args = ["-p", "--output-format", "text"]
timeout_secs = 60
# trace_io = true      # Log supervisor prompt/response pairs

[detector]
silence_timeout_secs = 3
answer_cooldown_millis = 1000
unknown_request_fallback = true
idle_input_fallback = true

[dangerous_mode]
enabled = false         # Skip agent safety prompts (use with caution)

[policy.auto_answer]
"Continue? [y/n]" = "y"
"Do you want to proceed?" = "yes"
```

## Files to Modify

- `src/install.rs` — add config scaffold function
- Add unit tests for scaffold behavior (creates if missing, skips if exists)

## How to Verify

1. `rm -rf /tmp/test-project && mkdir /tmp/test-project && cd /tmp/test-project && git init`
2. `batty install --dir .`
3. Confirm `.batty/config.toml` exists with documented defaults
4. Run `batty install --dir .` again — config should NOT be overwritten
5. `batty config` shows values from the scaffolded file
