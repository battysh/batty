# Troubleshooting

## 1) `batty work` exits immediately

Symptoms:
- Process exits quickly with a board/tooling error.

Cause:
- Missing phase board directory or missing required tooling.

Fix:
```sh
batty install
batty config
batty board phase-2.7 --print-dir
```
- Confirm the phase board exists under `.batty/kanban/<phase>/` (legacy projects may still use `kanban/<phase>/`).
- Confirm `tmux` and `kanban-md` are installed and available on `PATH`.

## 2) tmux version/capability error on startup

Symptoms:
- Startup fails with a `pipe-pane`/capability error.

Cause:
- tmux is too old or missing required features.

Fix:
- Check version: `tmux -V`
- Recommended version: `>= 3.2` (3.1.x is fallback path; `< 3.1` is not supported).
- Reinstall/upgrade tmux, then retry `batty work <phase>` or `batty resume <phase>`.

## 3) `kanban-md` not found

Symptoms:
- Errors indicate `kanban-md` is missing when opening boards or running workflow steps.

Cause:
- `kanban-md` is not installed (or not on `PATH`).

Fix:
```sh
batty install
# fallback:
cargo install kanban-md --locked
```
- Ensure `~/.cargo/bin` (or your install path) is in `PATH`.

## 4) `batty resume` cannot find session

Symptoms:
- Resume reports missing phase/session.

Cause:
- The tmux session is gone, renamed, or phase target does not match.

Fix:
```sh
tmux list-sessions
batty resume batty-phase-2-7
```
- If no session exists, start a fresh run with `batty work phase-2.7`.

## 5) Board path is not the one you expect

Symptoms:
- Board operations seem to target the wrong run or wrong phase directory.

Cause:
- Batty resolves board paths from active session/worktree/fallback rules.

Fix:
```sh
batty board phase-2.7 --print-dir
```
- Use printed path to verify whether Batty selected active tmux run board, latest worktree board, or repo fallback.

## 6) Supervisor is not responding or responses are delayed

Symptoms:
- Tier 2 answers never arrive, or come too slowly.

Cause:
- Supervisor program/path mismatch, timeout too low, or paused supervision.

Fix:
- Check pause state in tmux status bar (`PAUSED` means no auto answers).
- Resume with `Prefix + Shift+R` (`C-b R` by default).
- Inspect and tune:
  - `supervisor.program`
  - `supervisor.args`
  - `supervisor.timeout_secs`
  - `supervisor.trace_io`
- Use `batty config --json` to inspect effective values.

## 7) Worktree confusion or stale run directories

Symptoms:
- `--worktree` resumes an unexpected run, or stale branches remain.

Cause:
- Existing `.batty/worktrees/<phase>-run-###` run directories are reused by default.

Fix:
```sh
batty work phase-2.7 --worktree --new
```
- This forces a fresh run worktree.
- For full project cleanup after uninstalling assets: `batty remove` then `rm -rf .batty` (destructive; verify first).

## 8) Dangerous mode behavior is unclear

Symptoms:
- Agent commands appear to run with fewer safety prompts than expected.

Cause:
- `dangerous_mode.enabled = true` wraps supported agent commands with dangerous flags.

Fix:
- Verify config:
```toml
[dangerous_mode]
enabled = false
```
- Keep disabled unless you explicitly need reduced approval/sandbox friction.
- Re-enable only in trusted environments with clear risk acceptance.

## 9) Need deeper Tier 2 debugging context

Symptoms:
- Hard to understand why supervisor answered/escalated a prompt.

Cause:
- Tier 2 decisions rely on composed runtime context that is easiest to inspect from snapshot files.

Fix:
- Inspect `.batty/logs/<run>/tier2-context-<n>.md`.
- Correlate snapshot content with orchestrator events to verify prompt text and context quality.
- Enable/confirm `supervisor.trace_io = true` for richer orchestration logs.

## 10) Detector appears stuck in a loop or repeated nudges/escalations

Symptoms:
- Repeated nudge/escalate behavior without forward progress.

Cause:
- Prompt detector or stuck detector sees repeated stale output with no meaningful progress.

Fix:
- Check detector settings:
  - `detector.silence_timeout_secs`
  - `detector.answer_cooldown_millis`
  - `detector.unknown_request_fallback`
  - `detector.idle_input_fallback`
- Verify session is not paused (`C-b R` to resume).
- If loop persists, attach (`batty attach <phase>`), provide manual guidance in executor pane, and continue.
