---
name: treeish
description: Run this repo's dev servers inside a git worktree - each worktree gets its own ports, env files, and database. Use when starting or restarting a dev server, when a port is already in use, when a service fails to start on a missing .env or config error, or when a test needs a live server.
---

# treeish

One worktree, one **instance**: its own ports, its own rendered env files, its own
database. Several agents work in parallel without colliding and without anyone copying
configuration by hand.

## Starting

From anywhere inside the worktree:

```
treeish up
```

It starts the services in the background and returns once each answers, printing a URL
per service. Use those URLs — the repo's documented ports belong to the main checkout,
not to this instance.

Re-running `up` is safe, and is the cheapest way to recover from a service that died. The
first `up` in a worktree also installs the repo's dependencies, so it can take minutes
where later ones take seconds.

## Working

Commands that need to reach the instance get its ports from `treeish run`:

```
treeish run -- pytest tests/integration -v
treeish run -- sh -c 'curl localhost:$TREEISH_PORT_BACKEND/health'
```

`run` exports `TREEISH_PORT_<NAME>` for every declared port — the config's `web` becomes
`TREEISH_PORT_WEB` — plus `TREEISH_SLUG`, `TREEISH_DB_NAME`, and `TREEISH_WORKTREE`.

Commands that read the worktree's own env files — the ordinary unit-test loop — work
unwrapped, because `up` wrote those files to disk.

`treeish status` shows what is running now, and `--json` makes it parseable. Stop the
instance with `treeish down` when the work is finished.

## When something is wrong

A service that never started is a precondition problem:

```
treeish doctor
```

It checks each precondition separately and states the fix for whichever failed. Two of
its results are worth knowing in advance:

**"this is the main worktree"** — treeish reads secrets *from* the main checkout, so it
declines to write over them. Run from a linked worktree.

**"<file> is missing from the main checkout"** — the fix belongs in the main checkout, at
the path the message names. A worktree never inherits gitignored files, so that copy is
the only one treeish can read; a file created inside the worktree gets overwritten on the
next `up`.

A service that started and then crashed is a different problem, and `doctor` will not see
it. Read what the service itself said:

```
treeish logs <service>
```

## When the repo has no .treeish.toml

`up` will say so and point here. Run:

```
treeish --llm
```

That prints the schema and a worked example. Write `.treeish.toml` at the worktree root
and commit it — it is checked in, so every later agent in every worktree only runs
`treeish up`.
