# grove

Give every git worktree its own running dev stack — its own ports, its own env files, its
own database — so you can work several tickets in parallel without them colliding.

Built for the case where you have four agents on four tickets and all four want port 8080.

```
$ grove up
instance  mon-2695
frontend  http://localhost:24310
backend   http://localhost:24311
database  app_mon_2695
```

## The problem

Two things break the moment you run more than one worktree at a time:

- **A worktree arrives without your secrets.** `git worktree add` gives you tracked files.
  Your `.env.local` is gitignored, so it doesn't come along — and whoever is working in
  that worktree spends their first ten minutes rediscovering that.
- **Everything wants the same port.** Two worktrees, and the second one loses. Or worse,
  half-wins: a frontend that can't reach its own backend quietly talks to the *other*
  instance's backend instead.

grove reads secrets from your main checkout, rewrites the handful of values that must
differ, assigns each worktree a port block it keeps across restarts, and gives each its
own database on a shared server.

It is **not** a Docker wrapper. Your services run as ordinary processes on your machine.
Docker appears only for an optional shared datastore, and only if one isn't already
running.

## Install

```sh
curl -fsSL https://github.com/carlosarraes/grove/releases/latest/download/install.sh | sh
```

Linux x86_64 and macOS arm64. Then, to teach coding agents about it:

```sh
grove skill install
```

## Use

Commit a `.grove.toml` at the repo root, then from any worktree:

```sh
grove up                        # render env, start services, wait until they answer
grove run -- pytest tests/ -v   # run anything with this instance's ports exported
grove logs backend --since-restart
grove down
```

## Configure

```toml
version = 1

[ports]
names = ["frontend", "backend"]

# Copied from the MAIN checkout — a worktree never inherits gitignored files,
# which is the whole reason this exists.
[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

[secrets.set]
CORS_ORIGINS = "http://localhost:{{ port.frontend }}"
DATABASE_NAME = "{{ db.name }}"

[[resource]]
name = "mongo"
kind = "docker-shared"
image = "mongo:8"
port = 27017
db_name = "app_{{ slug }}"

[[seed]]
name = "org"
cwd = "backend"
command = "uv run python -m tests.seed"

[[service]]
name = "backend"
cwd = "backend"
setup = "uv sync"
command = "uv run uvicorn src.main:app --reload --port {{ port.backend }}"
ready = { http = "http://localhost:{{ port.backend }}/health", timeout = "180s" }
```

`grove --llm` prints the full schema and a worked example — that's what an agent reads to
write one of these.

## Commands

| | |
|---|---|
| `up` | render config, start services, wait until ready |
| `down [--purge]` | stop services; `--purge` also drops the database |
| `restart [service]` | replace one service without touching the others |
| `status [--json]` | ports, pids, health — and a warning if a service predates your last edit |
| `ls` | every instance on the machine |
| `run -- <cmd>` | run a command with this instance's environment |
| `logs [service] [--since-restart] [-n N]` | what a service printed |
| `seed [--force]` | populate the datastore; `--force` rebuilds a dirtied instance |
| `prune` | stop and forget instances whose worktree is gone |
| `doctor` | check everything needed to start, and say what to fix |

## What it doesn't do

Create worktrees (it attaches to whatever it finds), sandbox anything (a service can read
your home directory, same as if you'd started it yourself), or containerise your app.
Repos whose ports are baked into a compose file aren't supported yet, nor is per-instance
Postgres database creation.

## Status

Alpha, in daily use on one large monorepo. Expect the config format to move.

## Develop

```sh
just build     # release binary into ~/.local/bin
just check     # fmt, clippy, tests, packaging
just release 0.1.7
```
