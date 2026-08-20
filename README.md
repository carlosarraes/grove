# grove

Give every git worktree its own running dev stack — its own ports, its own env files, its
own database — so you can work several tickets in parallel without them colliding.

Built for the case where you have four agents on four tickets and all four want port 8080.

```
$ grove up
instance  checkout_redesign
frontend  http://localhost:24310
backend   http://localhost:24311
database  app_checkout_redesign
```

## The problem

Two things break the moment you run more than one worktree at a time:

- **A worktree arrives without your secrets.** `git worktree add` gives you tracked files.
  Your `.env.local` is gitignored, so it doesn't come along — and whoever is working in
  that worktree spends their first ten minutes rediscovering that.
- **Everything wants the same port.** Two worktrees, and the second one loses. Or worse,
  half-wins: a frontend that can't reach its own backend quietly talks to the *other*
  instance's backend instead.
- **Nothing tells you when there are too many.** Starting an instance is cheap and leaving
  one running is invisible, so they accumulate — and the bill arrives disguised as flaky
  tests on somebody's unrelated branch. `grove ls` reports the machine's load alongside
  the instances, and `grove down --idle 2h` reclaims the forgotten ones.

grove reads secrets from your main checkout, rewrites the handful of values that must
differ, assigns each worktree a port block it keeps across restarts, and gives each its
own database on a shared server.

It is **not** a Docker wrapper. Your services run as ordinary processes on your machine.
Docker appears only for an optional shared datastore, and only if one isn't already
running. A container grove starts receives `nofile=64000:64000`; an existing container
keeps its original launch configuration. Preserve any needed data before deliberately
removing and recreating one to adopt the limit.

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

# Overrides, not secrets. `.grove.toml` is committed, so a real credential written
# here is already in git — put those in the gitignored file named by `from` above.
# These values are also exported to every command grove runs.
[secrets.set]
CORS_ORIGINS = "http://{{ host.public }}:{{ port.frontend }}"
DATABASE_NAME = "{{ db.name }}"

# Containers grove starts receive nofile=64000:64000.
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
setup = "uv sync"                 # once per worktree
command = "uv run uvicorn src.main:app --reload --host {{ host.bind }} --port {{ port.backend }}"
ready = { http = "http://localhost:{{ port.backend }}/health", timeout = "180s" }

[[service]]
name = "frontend"
prepare = "npm run contracts:generate"   # every `up`, once the backend answers
command = "npm run dev -- --strictPort --port {{ port.frontend }}"
```

`grove --llm` prints the full schema and a worked example — that's what an agent reads to
write one of these.

### View an instance from another machine

Repositories opt in by using `{{ host.bind }}` where a service chooses its bind address
and `{{ host.public }}` in values consumed by the browser, including CORS and redirect
allowlists. Then expose only the current instance:

```sh
grove up --expose                       # use the default-route IPv4
grove up --expose-host dev-mac.local   # explicit host for VPN or multi-NIC setups
grove up                                # return this instance to localhost-only
```

Changing exposure re-renders and restarts the instance; `status` and `ls` show the
selected host. Sibling instances are unaffected. Exposure binds opted-in services to all
interfaces—it does not add a firewall, TLS, authentication, or a tunnel. Development
auth bypasses may therefore be reachable by other machines on the network.

Grove renders configured dotenv files and overlays per-instance variables on commands it
starts; it does not provide an empty settings environment. Tests that assert application
defaults must disable dotenv loading and clear the relevant process variables in the
repo's own fixture or settings constructor.

## Commands

| | |
|---|---|
| `up [--expose] [--expose-host HOST]` | render config, optionally expose opted-in services to the local network, start shared resources and services |
| `down [--purge]` | stop services; `--purge` also drops the database |
| `down --idle 2h` \| `--all-but-this` | stop instances across the machine, keeping their ports; `--dry-run` names them first |
| `restart [service]` | replace one service without touching the others |
| `status [--json]` | ports, pids, and whether each service's `ready.http` answers — plus a warning if a service predates your last edit |
| `ls [--json]` | every instance on the machine, most neglected first, with the machine's load |
| `run -- <cmd>` | run a command with this instance's environment overlaid |
| `logs [service] [--since-restart] [-n N]` | what a service printed |
| `seed [--force]` | populate the datastore; markers follow the managed container incarnation, while `--force` rebuilds dirtied data |
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
