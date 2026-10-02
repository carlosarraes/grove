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
  the instances, and `grove down --idle 2h` reclaims the forgotten ones. What `down` gives
  back is CPU; the checkout and the dependencies `setup` installed stay until
  `git worktree remove`, which grove never runs for you. A `[service.cache]` block
  makes worktrees on the same lockfile share one copy of their dependencies instead.

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

[service.cache]                            # worktrees on one lockfile share one copy
path = "node_modules"
key = ["package-lock.json"]
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
| `ls [--json]` | every instance on the machine, most neglected first, with the machine's load and what each holds on disk |
| `health [--json]` | everything on the machine that is costing someone — stray listeners, orphans, idle instances, disk — each with the command that ends it |
| `run -- <cmd>` | run a command with this instance's environment overlaid |
| `logs [service] [--since-restart] [-n N]` | what a service printed |
| `seed [--force]` | populate the datastore; markers follow the managed container incarnation, while `--force` rebuilds dirtied data |
| `prune` | stop and forget instances whose worktree is gone |
| `doctor` | check everything needed to start, and say what to fix |

## Admit heavy commands by load

`grove run --heavy -- <command>` waits until the one-minute load is below the available
core count. Plain `grove run` does not wait. This flag does not start services or MongoDB.
The child receives the normal instance environment and keeps its exit code.

The optional root-level config section sets the threshold and admission timeout:

```toml
[admission]
max_load = 10.0  # omit to use the available core count
timeout = "10m"
```

The threshold must be positive and finite. The timeout must be greater than zero.

Grove samples every two seconds and prints the load, threshold, and elapsed wait.
It prints another wait message every 15 seconds, then reports admission or timeout.
Timeout or an unreadable load fails without a child process. Ctrl-C cancels the wait.
The timeout applies to admission, not command execution.

Load averages lag behind new work and do not measure free memory. Keep fleetlock's
concurrency limit. Run admission before the lock so the load wait does not occupy a slot:

```sh
grove run --heavy -- fleetlock run pytest <lane> <card> -- <cmd>
```

The order is admission, then the fleetlock slot, then the command. The admission wait
stays outside fleetlock's hold cap. Load can change during the slot wait, so this remains
a heuristic. This flag provides neither FIFO ordering nor a slot reservation.

## Refresh dependencies

After a lockfile change, run `grove up`. Existing worktrees check their cache inputs
and reuse the matching shared install. Concurrent cache misses produce one install.
Warm links can run together. A refresh or prune waits for active readers of that entry.

Grove completes each cache link in a staging directory before it replaces dependencies. If setup or linking fails,
the previous install remains available and the command fails. A file inventory detects
truncated entries. Use `grove up --no-cache` to rebuild an entry that fails that check.
`--fresh` controls service restarts, not cache invalidation.
If the new install succeeds but old-backup cleanup fails, Grove warns with the backup
path and continues startup.

Retained backups live under the instance state directory,
in `dependency-backups/`, outside the worktree. Git cannot stage them with `git add -A`.
The install and state directory must share a filesystem for the atomic backup move.
Grove rejects a state directory inside the worktree for cached dependency replacement.

Before replacement, Grove stops the service that owns the cached install, if Grove
started that service. It stays stopped through replacement and synchronous cleanup.
Normal startup then runs prepare and readiness checks in service declaration order.
An unchanged setup marker leaves the service process alive.

A failed setup restores the previous install and attempts to restart the stopped service.
The command then returns the error.
If recovery also fails, Grove reports both failures. A startup failure after a successful
install leaves the new dependencies in place. Grove does not stop external watchers.
Concurrent `up` and `restart` calls for one instance wait through setup and startup.

Version 0.1.22 uses new cache keys so entries from earlier versions cannot bypass the
inventory check. Each key needs one initial install after the upgrade. Setup must leave
its declared cache key files unchanged. Include all installation inputs in `cache.key`.
Grove runs the configured setup command and its lifecycle scripts. Those scripts can
still modify other repository files.

For dependencies that must stay local, declare `setup_inputs` without a cache:

```toml
# In the existing backend service, whose cwd is "backend":
setup_inputs = ["uv.lock", "pyproject.toml"]
```

Paths are relative to the service's `cwd`. Grove reruns setup when the command,
file names or file contents change. A command-only marker reruns once when inputs
are first declared. Without `setup_inputs`, uncached setup still tracks only its command.

Missing or unreadable inputs fail setup. A failed setup, or one that changes its inputs,
does not receive a success marker. This check does not roll back an uncached install.
`setup_inputs` requires `setup` and a nonempty list. It cannot accompany `cache`.
Use `cache.key` for shared installs. Keep Python virtual environments local.

Setup emits `timing:` lines on stderr with a phase name, outcome and duration in seconds.
These separate lock waits, tree creation, inventory checks, replacement, cleanup, seeds
and service readiness. Dependency setup totals include their nested cache phases.
Do not add those totals to the individual phase durations.

Each `grove up` in a Git worktree saves its stdout, stderr and exit code under
`<instance-state>/up-logs/<timestamp>-<pid>.log`. The final stderr line names the file.
These files have mode `0600`. A closed downstream pipe does not discard the saved output.
The shell still decides the exit status of a pipeline. Read `grove up exit code:` in
the log when a pipeline hides Grove's status.

The log covers Grove's output. Services keep their separate logs. Invalid CLI arguments and worktree resolution failures occur
before a run log can start. An abrupt termination can leave a partial log without a footer.
At the start of each `up`, Grove keeps the newest 50 run logs and deletes older ones.
Active logs remain until a later run can prune them, so concurrent runs can exceed the cap temporarily.
Rotation errors warn without failing startup.

On a readiness timeout, Grove prints the service log path and makes a TCP check with
a one-second limit. An accepted connection means a listener accepted TCP, even though
HTTP readiness failed. Refusal or an inconclusive check does not identify the root cause.
The result describes the moment after the timeout, not the whole startup interval.

`grove doctor` warns when a setup command has neither `setup_inputs` nor `cache`.
A readiness probe without an explicit `timeout` remains a configuration error.

## Opt-in macOS clone trial

On macOS, `GROVE_MACOS_CLONE=1 grove up` clones a warm cache entry into the staging directory.
The flag is off by default. Linux ignores it and retains hardlinks.
Cold installs and cache publication retain the existing path.
A matching setup marker still skips setup, so the flag alone does not replace an existing install.

The trial uses APFS directory cloning. Apple discourages this API for directory trees:
see [clonefile](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2).
Unsupported volumes and clone errors fail setup and preserve the previous install.
Unset the flag to use the normal path. Grove does not silently retry with another strategy.

The clone must pass the existing inventory count before replacement. This count detects
missing entries, but does not verify file names or contents. Grove preserves symlinks without following them.
Cleanup of the old install stays synchronous. Cleanup errors warn and retain the backup.

Disk accounting has a known trial limitation. Grove detects shared hardlinks, but cannot
distinguish APFS clone blocks from private blocks through that check.
The private-dependency figures in `ls` and `health` can therefore overstate unique disk use
and space reclaimable from cloned installs. Those figures are not reliable for clone trials.
This change does not fix accounting for cloned blocks.

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
