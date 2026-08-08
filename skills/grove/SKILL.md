---
name: grove
description: Run this repo's dev servers inside a git worktree - each worktree gets its own ports, env files, and database. Use when starting or restarting a dev server, when a port is already in use, when a service fails to start on a missing .env or config error, or when a test needs a live server.
---

# grove

One worktree, one **instance**: its own ports, its own rendered env files, its own
database. Several agents work in parallel without colliding and without anyone copying
configuration by hand.

## Starting

From anywhere inside the worktree:

```
grove up
```

It starts the services in the background and returns once each answers, printing a URL
per service. Use those URLs — the repo's documented ports belong to the main checkout,
not to this instance.

Re-running `up` is safe, and is the cheapest way to recover from a service that died. The
first `up` in a worktree also installs the repo's dependencies, so it can take minutes
where later ones take seconds.

## Working

Commands that need to reach the instance get its ports from `grove run`:

```
grove run -- pytest tests/integration -v
grove run -- sh -c 'curl localhost:$GROVE_PORT_BACKEND/health'
```

`run` exports `GROVE_PORT_<NAME>` for every declared port — the config's `web` becomes
`GROVE_PORT_WEB` — plus `GROVE_SLUG`, `GROVE_DB_NAME`, `GROVE_WORKTREE`, and every
per-instance override the config declares.

It also sets `AGENT_BROWSER_SESSION` to this instance's slug, so browser automation run
through `grove run` gets its own session. Driving a browser outside `grove run` shares
one session across every instance on the machine, and a sibling's navigation will steal
the tab — which then reads as an authentication bug in whichever instance was watching.

Commands that read the worktree's own env files — the ordinary unit-test loop — work
unwrapped, because `up` wrote those files to disk.

`grove status` shows what is running now, and `--json` makes it parseable. Stop the
instance with `grove down` when the work is finished.

## When something is wrong

A service that never started is a precondition problem:

```
grove doctor
```

It checks each precondition separately and states the fix for whichever failed. Two of
its results are worth knowing in advance:

**"this is the main worktree"** — grove reads secrets *from* the main checkout, so it
declines to write over them. Run from a linked worktree.

**"<file> is missing from the main checkout"** — the fix belongs in the main checkout, at
the path the message names. A worktree never inherits gitignored files, so that copy is
the only one grove can read; a file created inside the worktree gets overwritten on the
next `up`.

A service that started and then crashed is a different problem, and `doctor` will not see
it. Read what the service itself said:

```
grove logs <service>
```

## When the repo has no .grove.toml

`up` will say so and point here. Run:

```
grove --llm
```

That prints the schema and a worked example. Write `.grove.toml` at the worktree root
and commit it — it is checked in, so every later agent in every worktree only runs
`grove up`.
