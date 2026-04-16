# Environment Variables

Batty reads a handful of environment variables at runtime. Most are
optional knobs — defaults are sensible, but these hooks let you override
behavior without editing config files.

Variables are grouped by concern. Standard Unix variables (`HOME`,
`PATH`, `TMUX`, `TMUX_PANE`) are consumed where you'd expect and are not
listed here.

## Logging

| Name        | Purpose                                                                                | Default       | Example                    |
|-------------|----------------------------------------------------------------------------------------|---------------|----------------------------|
| `BATTY_LOG` | Log filter directive. Takes precedence over `RUST_LOG`. Accepts `tracing-subscriber` [EnvFilter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html). | *(unset)*     | `BATTY_LOG=debug`, `BATTY_LOG=batty=debug,hyper=warn` |
| `RUST_LOG`  | Fallback log filter if `BATTY_LOG` is unset. Standard Rust convention.                 | *(unset)*     | `RUST_LOG=info`            |

If neither is set, the `-v` / `-vv` / `-vvv` CLI flags control verbosity.

## Identity

| Name           | Purpose                                                                     | Default                            | Example                   |
|----------------|-----------------------------------------------------------------------------|------------------------------------|---------------------------|
| `BATTY_MEMBER` | Override the detected sender for `batty send`. Set automatically by the shim when a subprocess needs a stable identity. | detected from tmux pane `@batty_role` | `BATTY_MEMBER=eng-1-2`    |

## Messaging integrations

| Name                            | Purpose                                                                                          | Default                         | Example                              |
|---------------------------------|--------------------------------------------------------------------------------------------------|---------------------------------|--------------------------------------|
| `BATTY_TELEGRAM_BOT_TOKEN`      | Telegram bot token for human-bridge roles. Falls back if `channel_config.token` is not set.       | *(unset — Telegram disabled)*   | `BATTY_TELEGRAM_BOT_TOKEN=123:abc…`  |
| `BATTY_DISCORD_BOT_TOKEN`       | Discord bot token for Discord-bridge roles.                                                       | *(unset — Discord disabled)*    | `BATTY_DISCORD_BOT_TOKEN=…`          |

## Grafana alerting

| Name                               | Purpose                                                                                     | Default                                 | Example                                  |
|------------------------------------|---------------------------------------------------------------------------------------------|-----------------------------------------|------------------------------------------|
| `BATTY_GRAFANA_PROVISIONING_DIR`   | Override Grafana provisioning output directory.                                             | `~/.grafana/provisioning`               | `BATTY_GRAFANA_PROVISIONING_DIR=/etc/grafana/provisioning` |
| `BATTY_GRAFANA_ALERT_CHAT_ID`      | Telegram chat ID for Grafana alert delivery.                                                | falls back to `BATTY_TELEGRAM_ALERT_CHAT_ID` | `BATTY_GRAFANA_ALERT_CHAT_ID=-100123…`   |
| `BATTY_TELEGRAM_ALERT_CHAT_ID`     | Backwards-compatible fallback for the alert chat ID.                                        | *(unset)*                               | `BATTY_TELEGRAM_ALERT_CHAT_ID=-100123…`  |

See [Grafana alerting](../grafana-alerting.md) for full wiring.

## Paths and overrides

| Name                              | Purpose                                                                    | Default                                    | Example                                          |
|-----------------------------------|----------------------------------------------------------------------------|--------------------------------------------|--------------------------------------------------|
| `BATTY_BINARY_PATH`               | Override the `batty` binary the daemon spawns for shim subprocesses.       | `std::env::current_exe()`                  | `BATTY_BINARY_PATH=/opt/batty/bin/batty`         |
| `BATTY_PROJECT_REGISTRY_PATH`     | Override the JSON file that tracks known project roots.                    | `~/.batty/project-registry.json`           | `BATTY_PROJECT_REGISTRY_PATH=/tmp/registry.json` |
| `BATTY_PROJECT_ROUTING_STATE_PATH`| Override the JSON file that tracks per-project routing state.              | `~/.batty/project-routing-state.json`      | `BATTY_PROJECT_ROUTING_STATE_PATH=/tmp/state.json` |

## Shim runtime

| Name                                 | Purpose                                                                                      | Default   | Example                                          |
|--------------------------------------|----------------------------------------------------------------------------------------------|-----------|--------------------------------------------------|
| `BATTY_GRACEFUL_SHUTDOWN_TIMEOUT_SECS` | Seconds the shim waits for a graceful auto-commit before killing the child on restart.      | `30`      | `BATTY_GRACEFUL_SHUTDOWN_TIMEOUT_SECS=60`        |
| `BATTY_AUTO_COMMIT_ON_RESTART`       | Toggle the auto-commit-before-kill behavior. Accepts `0` / `false` / `FALSE` to disable.     | `true`    | `BATTY_AUTO_COMMIT_ON_RESTART=false`             |

## Daemon tuning

| Name                                          | Purpose                                                                                  | Default | Example                                          |
|-----------------------------------------------|------------------------------------------------------------------------------------------|---------|--------------------------------------------------|
| `BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS`   | How many times the automation loop will try to reconcile an orphan branch before parking. | `3`     | `BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS=5`    |

## Cleanroom / external tooling

| Name                          | Purpose                                                        | Default            | Example                                       |
|-------------------------------|----------------------------------------------------------------|--------------------|-----------------------------------------------|
| `BATTY_SKOOLKIT_SNA2SKOOL`    | Override the `sna2skool` binary used for Z80 disassembly.      | `sna2skool`        | `BATTY_SKOOLKIT_SNA2SKOOL=/opt/skoolkit/sna2skool.py` |
| `BATTY_GHIDRA_HEADLESS`       | Override the `analyzeHeadless` binary used for Ghidra analysis.| `analyzeHeadless`  | `BATTY_GHIDRA_HEADLESS=/opt/ghidra/support/analyzeHeadless` |

## `.env` file loading

Batty reads a `.env` file from the current working directory on startup
(via `src/env_file.rs`). Values already exported in the shell win over
`.env` entries. Lines matching `KEY=value` are parsed; comments and blank
lines are ignored.
