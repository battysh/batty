# Task 396: Nether Earth game loop wiring

## Cross-repo deliverable

The implementation lives in the **nether_earth** repository:

- **Repo:** `~/nether_earth`
- **Worktree:** `~/nether_earth/.batty/worktrees/eng-1-2`
- **Branch:** `eng-1-2/396`
- **Commit:** `de9e8c0`
- **Changed file:** `runtime/main.c` (+141 lines, -9 lines)

## What was wired

1. `game_tick()` — per-frame update calling robot AI, combat, resources, clock, base capture
2. Live HUD — `draw_world()` reads from `player_resources`, `game_clock`, `robot_count_active()`
3. Robot movement — `robot_update_tick()` called for all 48 slots each tick
4. Combat — `combat_ai_try_fire()` + `combat_update_bullets()` + `combat_update_destruction()`
5. Base capture — robots at enemy warbase coords trigger `enemy_bases--`/`player_bases++`

## Build

```
cd ~/nether_earth/runtime && make   # clean with -Wall -Wextra -Werror
```
