# Grafana Alerting

These provisioning files add four stall-detection alerts under `grafana/provisioning/alerting/`:

- `Zero Activity`: fires when `events` has no rows in the last 30 minutes.
- `Dispatch Stall`: fires when the latest `pipeline_starvation_detected` event reports `todo_tasks > 0` and no `task_assigned` event has happened for 20 minutes.
- `Agent Crash Spike`: fires when more than 3 `member_crashed` or `pane_death` events land within 10 minutes.
- `Review Queue Backup`: fires when a task has been completed for 30+ minutes without a later `task_auto_merged`, `task_manual_merged`, or `task_reworked` event.

## Thresholds

Default windows and thresholds:

| Alert | Lookback | Trigger | `for` |
| --- | --- | --- | --- |
| Zero Activity | 30m | no events | 5m |
| Dispatch Stall | 20m | todo work exists and no dispatches | 2m |
| Agent Crash Spike | 10m | count `> 3` | 2m |
| Review Queue Backup | 30m | count `> 0` | 5m |

## Customization

To tune an alert, edit both of these sections in [`stall-detection.yaml`](/Users/zedmor/batty/.batty/worktrees/eng-1-2/grafana/provisioning/alerting/stall-detection.yaml):

- `relativeTimeRange.from`: lookback window in seconds.
- `model.conditions[0].evaluator.params[0]`: numeric threshold.
- `for`: how long Grafana keeps the condition firing before notifying.

Notification routing lives in [`notification-channels.yaml`](/Users/zedmor/batty/.batty/worktrees/eng-1-2/grafana/provisioning/alerting/notification-channels.yaml). By default, alerts go to the local orchestrator log webhook at `http://127.0.0.1:8787/grafana-alerts`.

Telegram is intentionally left as an opt-in example. Uncomment the `batty-telegram` contact point and the `routes` block, then set:

- `BATTY_GRAFANA_TELEGRAM_BOT_TOKEN`
- `BATTY_GRAFANA_TELEGRAM_CHAT_ID`

## Notes

Two alerts use telemetry-backed proxies rather than live board state:

- `Dispatch Stall` depends on recent `pipeline_starvation_detected` events because the telemetry database does not persist current board status directly.
- `Review Queue Backup` treats `task_completed` as the start of review and clears the queue when merge or rework events arrive.
