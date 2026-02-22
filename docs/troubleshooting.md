# Troubleshooting

## `batty work` exits quickly

- Run project initialization first:

```sh
batty install
```

- Run `batty config` and confirm expected defaults
- Ensure the requested phase board exists under `kanban/<phase>/`
- Confirm `tmux` is installed and available

## `batty resume` cannot find a session

- List tmux sessions:

```sh
tmux list-sessions
```

- Retry with explicit tmux session name:

```sh
batty resume batty-phase-2-7
```

## Board path does not match expected run

Use:

```sh
batty board phase-2.7 --print-dir
```

This prints the resolved board directory Batty will use.

## Supervisor is not auto-answering prompts

- Check status bar for pause mode
- Resume supervision with `Prefix + Shift+R`
- Verify detector and policy defaults in `.batty/config.toml`
