<!-- markdownlint-disable MD041 -->
<!--
  crates-io-readme-excerpt.md — canonical UTM-tagged subset of README.md
  ship target: crates.io page for batty-cli.

  STATUS: DORMANT. Cargo.toml still points `readme = "README.md"` so the
  full README ships. This file exists as a candidate tighter excerpt for
  a future switch (set `readme = "crates-io-readme-excerpt.md"` when we
  want a shorter crates.io page).

  All github.com/battysh/batty links here carry:
    ?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x

  UTMs are dormant pending batty.sh analytics — GitHub's traffic API
  attributes referrers by domain (e.g. "crates.io"), not by URL query
  string, so tags do not affect GitHub-side visibility. They become
  active when a downstream analytics layer (Plausible/GA on batty.sh, or
  a redirect we control) reads them.

  Keep this file in sync with README.md's Quick Start + links sections
  whenever the canonical README changes.
-->

<p align="center">
  <img src="https://raw.githubusercontent.com/battysh/batty/main/assets/batty-icon.png" alt="Batty" width="200">
  <h1 align="center">Batty</h1>
  <p align="center"><strong>Self-improving hierarchical agent teams for software development.</strong></p>
</p>

<p align="center">
  <a href="https://github.com/battysh/batty?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x">GitHub</a>
  &middot;
  <a href="https://battysh.github.io/batty/">Docs</a>
  &middot;
  <a href="https://github.com/battysh/batty/blob/main/LICENSE?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x">MIT License</a>
</p>

Batty is a control plane for agent software teams. Define roles such as
architect, manager, and engineers; Batty launches them through typed SDK
protocols or shim-backed PTYs, routes work between roles, tracks the
kanban board, isolates engineer work in git worktrees, and closes the
loop with verification and auto-merge.

## Quick Start

```sh
cargo install batty-cli
batty init
batty start
batty attach
batty status
```

`cargo install batty-cli` installs the `batty` binary. After `batty
init`, edit `.batty/team_config/team.yaml`, start the daemon, attach to
the live tmux session, and send the architect the first directive:

```sh
batty send architect "Build a small API with auth, tests, and CI."
```

Full setup flow and docs: <https://github.com/battysh/batty?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x>

## Links

- [GitHub repo](https://github.com/battysh/batty?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x)
- [Docs site](https://battysh.github.io/batty/)
- [Good First Issues](https://github.com/battysh/batty/labels/good%20first%20issue?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x)
- [CI status](https://github.com/battysh/batty/actions?utm_source=crates-io&utm_medium=readme&utm_campaign=0.11.x)

## License

MIT
