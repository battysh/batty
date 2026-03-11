# Getting Started

Use this guide to install Batty, create a team config, launch a tmux session, send the first directive, and stop or resume the team.
## Prerequisites
- Rust 1.85+
- `tmux`
- `kanban-md`
- At least one agent CLI on your `PATH` (`claude`, `codex`, or similar)
```sh
cargo install kanban-md --locked
```
## Install
Install Batty from crates.io:
```sh
cargo install batty-cli
```
Or build from source:
```sh
git clone https://github.com/battysh/batty.git
cd batty
cargo install --path .
```
## Initialize
Run `batty init` from the repository you want Batty to manage.
```sh
cd my-project
batty init
```
Example output:
```text
Initialized team config (4 files):
  /path/to/my-project/.batty/team_config/team.yaml
  /path/to/my-project/.batty/team_config/architect.md
  /path/to/my-project/.batty/team_config/manager.md
  /path/to/my-project/.batty/team_config/engineer.md

Edit .batty/team_config/team.yaml to configure your team.
Then run: batty start
```
If you want a different scaffold, use `batty init --template solo|pair|simple|squad|large|research|software|batty`.
## Configure
Edit `.batty/team_config/team.yaml`. Start with `name`, `layout`, `roles`, and `use_worktrees`.

```yaml
name: my-project
layout:
  zones:
    - name: architect
      width_pct: 30
    - name: engineers
      width_pct: 70
      split: { horizontal: 3 }
roles:
  - name: architect
    role_type: architect
    agent: claude
    prompt: architect.md
  - name: manager
    role_type: manager
    agent: claude
    prompt: manager.md
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    prompt: engineer.md
    use_worktrees: true
```
Validate before you start:
```sh
batty validate
```
Example output:
```text
Config: /path/to/my-project/.batty/team_config/team.yaml
Team: my-project
Roles: 3
Total members: 5
Valid.
```
## Launch
Start the daemon and attach to tmux immediately:
```sh
batty start --attach
```
`batty start --attach` opens tmux instead of printing a summary. Expect something like:

```text
┌ architect ─────────────┬ manager ───────────────┬ eng-1-1 ───────────────┐
│ role prompt loaded     │ role prompt loaded     │ codex/claude starting  │
│ waiting for directive  │ waiting for architect  │ waiting for assignment  │
├────────────────────────┼────────────────────────┼ eng-1-2 ───────────────┤
│                        │                        │ waiting for assignment  │
├────────────────────────┼────────────────────────┼ eng-1-3 ───────────────┤
│                        │                        │ waiting for assignment  │
└────────────────────────┴────────────────────────┴─────────────────────────┘
```
## Send A Directive
From another shell, send the architect the first goal:
```sh
batty send architect "Implement a small JSON API with auth and tests."
```
Example output:
```text
Message queued for architect.
```
## Monitor
Check the team without attaching:
```sh
batty status
```
Example output:
```text
Team: my-project
Session: batty-my-project (running)

MEMBER               ROLE         AGENT      REPORTS TO
--------------------------------------------------------------
architect            architect    claude     -
manager              manager      claude     architect
eng-1-1              engineer     codex      manager
eng-1-2              engineer     codex      manager
eng-1-3              engineer     codex      manager
```
Use these while the team runs:

```sh
batty attach
batty inbox architect
batty board
```
If a member has queued messages, `batty inbox architect` looks like:

```text
STATUS   FROM         TYPE         ID       BODY
------------------------------------------------------------------------
pending  human        send         a1b2c3d4 Implement a small JSON API with auth...
```
## Stop And Resume
Stop the daemon and tmux session:
```sh
batty stop
```
Example output:
```text
Team session stopped.
```
The next `batty start` resumes agent sessions from the last stop:

```sh
batty start
```
Example output:
```text
Team session started: batty-my-project
Run `batty attach` to connect.
```
## Telegram
If you want a human endpoint over Telegram, add a `user` role with `channel: telegram`, then run:
```sh
batty telegram
```
## Next Steps
- [Configuration Reference](reference/config.md)
- [CLI Reference](reference/cli.md)
