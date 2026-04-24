# Brazil Workspace-Compatible Engineer Worktrees

Task `#693` investigates whether Batty can keep engineer isolation while still
allowing Brazil-heavy builds to resolve cross-package dependencies.

## Current Batty Behavior

Batty currently detects a "multi-repo" workspace by scanning the project root
for immediate child git repositories. When that mode is active, it prepares one
plain git worktree per sub-repo under:

```text
.batty/worktrees/<engineer>/<repo>
```

The relevant code paths are:

- `TeamDaemon::new()` in `src/team/daemon.rs`: infers `is_multi_repo` from
  child git repos, not from an explicit workspace type.
- `member_work_dir()` in `src/team/launcher.rs`: assigns engineers to
  `.batty/worktrees/<engineer>`.
- `setup_multi_repo_worktree()` and
  `prepare_multi_repo_assignment_worktree()` in `src/team/task_loop.rs`: create
  nested git worktrees at `.batty/worktrees/<engineer>/<repo>`.
- `ensure_engineer_worktree_links()` and
  `ensure_shared_cargo_target_config()` in `src/team/task_loop.rs`: add Batty's
  `.batty/team_config` symlink and Cargo target config, but no Brazil metadata
  or workspace registration.

That layout is enough for git isolation and Rust build caching, but it does not
make the engineer directory a Brazil workspace peer. In this environment,
`brazil`, `brazil-build`, and `packageInfo` are not installed, so I could not
run a live Brazil registration test here.

## Recommendation

Do not try to "teach" the existing `.batty/worktrees/<engineer>/<repo>` layout
to behave like a Brazil workspace. Treat Brazil as a distinct workspace type.

Recommended layout:

```text
<workspace-parent>/
  src/                                 # original Brazil workspace
  .batty-brazil/
    <engineer>/
      src/
        <pkg-a>/                       # git worktree for repo A
        <pkg-b>/                       # git worktree for repo B
```

Why this layout:

- The engineer gets a real workspace root containing its own `src/` tree.
- The workspace root is a sibling of the original `src/`, which matches the
  intent of the issue and keeps Brazil-specific metadata out of Batty's normal
  `.batty/worktrees` tree.
- Batty can still keep isolation by giving each engineer a separate workspace
  root.
- Cleanup and health checks can target one explicit Brazil workspace root per
  engineer instead of guessing from nested sub-repos.

## Spike Command Sequence

The following is the spike sequence I would validate on a machine with Brazil
tooling installed. The exact `brazil ws use` flags may need adjustment to local
workspace conventions, but the shape of the experiment should stay the same.

```bash
# starting point
export WS_PARENT=~/workplace/InscopeDataAgent
export ENG_WS="$WS_PARENT/.batty-brazil/eng-1-2"

mkdir -p "$ENG_WS/src"

# create one git worktree per package/repo inside the engineer workspace
git -C "$WS_PARENT/src/pkg-a" worktree add -b eng-1-2/693 "$ENG_WS/src/pkg-a" main
git -C "$WS_PARENT/src/pkg-b" worktree add -b eng-1-2/693 "$ENG_WS/src/pkg-b" main

# register the engineer workspace as a Brazil workspace rooted at ENG_WS/src
cd "$ENG_WS/src"
brazil ws use ./pkg-a
brazil ws use ./pkg-b

# verify that package metadata now points at the worktree paths, not the
# original workspace
packageInfo pkg-a
packageInfo pkg-b

# run the heavy build from the engineer workspace
cd "$ENG_WS/src/pkg-a"
brazil-build
```

Success criteria for the spike:

1. `packageInfo` resolves packages inside `"$ENG_WS/src"` rather than
   `"$WS_PARENT/src"`.
2. `brazil-build` from `"$ENG_WS/src/pkg-a"` resolves cross-package deps from
   sibling worktrees under `"$ENG_WS/src"`.
3. Deleting or resetting the engineer workspace does not mutate the original
   workspace registration.

## Failure Modes To Expect

- `brazil ws use` may record canonical source paths from the original workspace
  instead of the worktree paths, which would defeat isolation.
- Brazil workspace metadata may be global to the workspace root, so engineer
  teardown must unregister or delete generated metadata before reuse.
- Batty's current worktree health logic assumes either one git repo or a
  generic nested multi-repo tree; a Brazil workspace root needs its own
  `workspace_type` and discovery rules.
- Merge/completion code currently reads branch state from nested git repos under
  `.batty/worktrees/<engineer>/<repo>`; that logic will need a Brazil-aware
  root resolver.
- If Brazil requires `packageInfo` or workspace registration files outside the
  git worktree itself, Batty must preserve those files across engineer refreshes
  without checking them into the repo.
- If Brazil tooling is unavailable on the local host, Batty must degrade
  cleanly to the current generic multi-repo layout instead of half-creating a
  broken engineer workspace.

## Conclusion

This looks feasible, but only as an explicit Brazil workspace mode. The
recommended next step is to add a dedicated Batty follow-up task that:

- introduces `workspace_type: brazil`,
- creates engineer workspace roots outside `.batty/worktrees`,
- registers and unregisters packages through Brazil workspace tooling, and
- updates health/merge/status flows to resolve repos from the Brazil workspace
  root instead of generic nested sub-repo discovery.

Follow-up: board task `#706` tracks the implementation.
