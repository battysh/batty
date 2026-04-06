# Task Aging Alerts

Batty's workflow daemon now computes task aging from task frontmatter timestamps instead of filesystem mtimes.

Thresholds come from `workflow_policy` in `.batty/team_config/team.yaml`:

- `stale_in_progress_hours` default `4`
- `aged_todo_hours` default `48`
- `stale_review_hours` default `1`

Alert behavior:

- `in-progress` tasks past threshold with no commits ahead of `main` emit `task_stale` and notify the manager.
- `todo` tasks past threshold emit `task_aged` and appear in standup/status aging summaries.
- `review` tasks past threshold emit `review_stale` and notify the review owner or manager.
