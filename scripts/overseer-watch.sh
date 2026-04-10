#!/usr/bin/env bash
# overseer-watch.sh — lightweight external watchdog for batty.
#
# Runs from launchd every 15 minutes. Checks batty health and writes
# structured status to .batty/overseer.log. On anomalies, writes alerts
# to .batty/overseer-alerts.log so the next Claude session can pick up.
#
# Intentionally does NOT restart batty automatically — restarts require
# judgement (stop → fix → start) and should be driven by Claude or human.
# This script only observes and alerts.

set -u

PROJECT_DIR="/Users/zedmor/batty"
LOG_FILE="$PROJECT_DIR/.batty/overseer.log"
ALERT_FILE="$PROJECT_DIR/.batty/overseer-alerts.log"
BATTY_BIN="$HOME/.cargo/bin/batty"
SESSION_NAME="batty-batty-dev"

TS=$(date +"%Y-%m-%d %H:%M:%S")
cd "$PROJECT_DIR" || { echo "$TS FATAL cannot cd $PROJECT_DIR" >>"$LOG_FILE"; exit 1; }

alert() {
  echo "$TS $1" >>"$ALERT_FILE"
  echo "$TS ALERT $1" >>"$LOG_FILE"
}

note() {
  echo "$TS $1" >>"$LOG_FILE"
}

# 1. tmux session alive?
if ! tmux has-session -t "$SESSION_NAME" 2>/dev/null; then
  alert "tmux session '$SESSION_NAME' missing — batty not running"
  exit 0
fi

# 2. batty status parses?
STATUS=$("$BATTY_BIN" status 2>&1)
if [ $? -ne 0 ] || [ -z "$STATUS" ]; then
  alert "batty status failed or empty"
  exit 0
fi

# 3. Count engineers and their state from status output.
# Engineer rows look like: "eng-1-1   engineer  codex  working  ..."
ENG_LINES=$(printf '%s\n' "$STATUS" | grep -E "^eng-[0-9]+-[0-9]+ " || true)
ENG_COUNT=$(printf '%s\n' "$ENG_LINES" | grep -c "^eng-" || true)
WORKING=$(printf '%s\n' "$ENG_LINES" | grep -c "working" || true)
IDLE=$(printf '%s\n' "$ENG_LINES" | grep -c "idle" || true)
ERROR_STATES=$(printf '%s\n' "$ENG_LINES" | grep -cE "error|crashed|stalled" || true)

note "engineers=$ENG_COUNT working=$WORKING idle=$IDLE errors=$ERROR_STATES"

if [ "$ERROR_STATES" -gt 0 ]; then
  alert "$ERROR_STATES engineer(s) in error/crashed/stalled state"
fi

# 4. Worktree health: commits ahead and last commit age.
for wt in "$PROJECT_DIR"/.batty/worktrees/eng-*; do
  [ -d "$wt" ] || continue
  name=$(basename "$wt")
  ahead=$(git -C "$wt" rev-list --count main..HEAD 2>/dev/null || echo 0)
  dirty=$(git -C "$wt" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
  last_epoch=$(git -C "$wt" log -1 --format=%ct 2>/dev/null || echo 0)
  now_epoch=$(date +%s)
  if [ "$last_epoch" -gt 0 ]; then
    age_min=$(( (now_epoch - last_epoch) / 60 ))
  else
    age_min=-1
  fi
  note "wt $name ahead=$ahead dirty=$dirty last_commit_age_min=$age_min"

  # Alert on very stale worktrees that have uncommitted work (risk of loss).
  if [ "$dirty" -gt 50 ] && [ "$age_min" -gt 120 ]; then
    alert "$name has $dirty dirty files, last commit $age_min min ago — possible loss risk"
  fi
done

# 5. Disk check — build artifacts.
TARGET_GB=$(du -sk "$PROJECT_DIR/target" 2>/dev/null | awk '{printf "%.1f", $1/1024/1024}')
if [ -n "$TARGET_GB" ]; then
  note "target_dir_gb=$TARGET_GB"
  # Alert above 15GB.
  THRESH=$(echo "$TARGET_GB > 15" | bc -l 2>/dev/null || echo 0)
  if [ "$THRESH" = "1" ]; then
    alert "target/ directory is ${TARGET_GB}GB — consider cargo clean"
  fi
fi

# 6. Runaway processes. Normal: 1 watchdog + 1 daemon + 2/member (console-pane + shim).
# For a 6-member team that's ~14 procs. Alert only on clear runaways (>25).
BATTY_PROCS=$(pgrep -x batty 2>/dev/null | wc -l | tr -d ' ')
note "batty_procs=$BATTY_PROCS"
if [ "$BATTY_PROCS" -gt 25 ]; then
  alert "$BATTY_PROCS batty processes running — possible zombies"
fi

# Rotate log if it exceeds 1MB.
if [ -f "$LOG_FILE" ]; then
  SIZE=$(stat -f%z "$LOG_FILE" 2>/dev/null || echo 0)
  if [ "$SIZE" -gt 1048576 ]; then
    mv "$LOG_FILE" "${LOG_FILE}.1"
  fi
fi

exit 0
