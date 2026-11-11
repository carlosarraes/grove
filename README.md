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
...
grove: ready checkout_redesign (31s)
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
  the instances, and `grove down --idle 2h` reclaims the forgotten ones (`up` and `run` also
  stop them on their own, see [Idle instances](#idle-instances)). What `down` gives
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

Commands act on the worktree you run them in. To look at another instance without
leaving yours, give `ls`, `status`, `doctor` or `down` its name as `grove ls` prints it, or
its worktree path. `ls`, `status` and `gate` also accept that target with `--worktree`:

```sh
grove status feature_branch     # read-only, from anywhere
grove status --worktree ../other-worktree
grove gate ../other-worktree
grove gate --worktree feature_branch
grove doctor ../other-worktree
grove down feature_branch
```

`status` reads an existing instance from the registry without reallocating its ports.

An unknown name fails and points at `grove ls`. `logs`, `restart`, `run` and `seed` already
take a service or a command as their argument, so run those inside the worktree.

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
| `gate [NAME] [--worktree NAME\|PATH] [--no-cache]` | whether `up` for this instance would link or build dependencies, and whether load admits it; exits 0 or 3 |
| `up [--heavy] [--gate] [--footprint] [--expose] [--expose-host HOST] [--setup-only] [--dry-run]` | render config, optionally measure dependency footprint, expose opted-in services, and start shared resources and services; `--setup-only` runs dependency setup without starting anything, while `--dry-run` reports cache decisions without changes |
| `down [--purge]` | stop services; `--purge` also drops the database |
| `down --idle 2h` \| `--all-but-this` | stop instances across the machine, keeping their ports; `--dry-run` names them first |
| `restart [service]` | replace one service without touching the others |
| `status [NAME] [--worktree NAME\|PATH] [--json]` | ports, pids, and whether each service's `ready.http` answers — plus a warning if a service predates your last edit |
| `ls [--json]` | every instance on the machine, most neglected first, with its owner, how long since its last grove command, the machine's load and what each holds on disk |
| `test-containers [--remove-stale]` | report stale Mongo testcontainers; recheck and remove only with the explicit flag |
| `health [--json]` | everything on the machine that is costing someone — stray listeners, orphans, idle instances, disk — each with the command that ends it |
| `run -- <cmd>` | run a command with this instance's environment overlaid |
| `logs [service] [--since-restart] [-n N]` | what a service printed |
| `seed [--force]` | populate the datastore; markers follow the managed container incarnation, while `--force` rebuilds dirtied data |
| `clean [NAME...] [--worktree PATH]... [--yes] [--dry-run]` | remove idle git worktrees recorded by grove; dry-run by default, delete only with `--yes` |
| `prune [NAME...] [--worktree PATH]... [--dry-run] [--purge]` | stop and forget all instances whose worktree is gone or no longer a git worktree, or only the selected orphaned instances by `grove ls` name or recorded path; selected prune skips the shared store sweep |
| `doctor` | check everything needed to start, and say what to fix |

`grove clean` removes existing grove-linked git worktrees; `grove prune` stops and forgets orphaned instances whose worktree is already gone or left behind as a non-git husk. Clean is machine-wide and defaults to a dry run (`--dry-run` is explicit); pass `--yes` to remove eligible trees without forcing git or dropping databases. Names and repeatable `--worktree PATH` select registry entries; unknown selectors reclaim nothing. Unregistered worktrees are never touched.

Clean reports each tree and skips primary checkouts, held instance locks, running `grove up` runs or stacks, open files, uncommitted changes, branches without upstreams, and unpushed commits. It requires `lsof` and refuses deletion when it is missing. Sizes count unique `nlink == 1` allocated blocks across the whole checkout using the same private-block method as `--footprint`, excluding virtualenv directories containing `pyvenv.cfg` and never following symlinks. These are reclaimable bytes, not `du` totals that include shared hardlinks. With `--yes`, safety checks run again before removal; a git removal failure stops further removals.

### Idle instances

An instance is idle when no grove command (`up`, `run`, `logs`, `restart`, `seed`) has run in its
worktree for a while. Service logs do not count: a backend that logs health checks every few
seconds would otherwise look busy forever. `status` and `ls` only read, so they do not reset the
clock either.

Every `grove up` and `grove run` stops the other instances of the same repo that have been idle
for longer than `[idle] stop_after`, which is `4h` when the section is absent. Stopping keeps ports,
worktree and database, so a stopped instance comes back on the same URLs with `grove up`. The
instance the command runs in is never stopped. Each stop prints one line on stderr:

```
grove: stopped idle t3_x (no grove command for 5h12m)
```

```toml
[idle]
stop_after = "8h"   # or "off" to never stop idle instances
```

Dependency footprint walks are opt-in: `grove up --footprint` measures after setup. Plain `grove up`, `grove ls` and `grove health` do not walk the worktree; `ls` and health read stored figures and label unmeasured instances with the flag to run next.

Do not write a separate `.grove.toml` to install dependencies. Use `grove up --setup-only` (optionally `--dry-run` first): a hand-written config hashes different cache inputs and misses the shared store. Dry-run reports per-service cache hit, cache miss, or `will run setup (no grove dependency cache)` without installing or linking anything. Setup-only never stops idle sibling instances. Seed and frontend file linking report progress on stderr about every five seconds. Setup-only uses the inherited project config and normal cache logic, linking a stored tree on a hit and installing and storing it on a miss, without starting shared containers, seeds, or services. Combine it with `--no-cache` to rebuild cached dependencies or `--footprint` to measure the resulting dependency footprint.

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
It prints another wait message every 5 seconds, then reports admission or timeout.
Timeout or an unreadable load fails without a child process. Ctrl-C cancels the wait.
The timeout applies to admission, not command execution.

Load averages lag behind new work and do not measure free memory. Use Ferry's shared
slot pool to limit concurrent work. Run admission before the lock so the load wait does
not occupy a slot:

```sh
grove run --heavy -- ferry lock pytest --lane <lane> --label <card> -- <cmd>
```

The order is admission, then the Ferry slot, then the command. The admission wait stays
outside the slot. Load can change during the slot wait, so this remains a heuristic.
This flag provides neither FIFO ordering nor a slot reservation.

## Gate installs by load

`grove gate [NAME]` (or `grove gate --worktree NAME|PATH`) decides, from the same cache
key `up` uses, whether `up` would only link dependency trees the store already holds or
would have to build one, and judges the 1- and 15-minute load against that kind's limits:

```
load gate: link-only (lockfile matches store), limits 60/60, load 48.0/52.0 ok
load gate: full install (frontend lockfile not in store, 1a2b…), limits 25/35, load 30.1/33.0 over, holding
```

It exits 0 when admitted and 3 when holding. `grove up --gate` prints the same line
and waits for admission (up to `[admission] timeout`) before installing anything,
repeating it with `: still waiting (Ns of TIMEOUT)` every 5 seconds while it holds.
Limits are `[admission] full_install = [25, 35]` and `link_only = [60, 60]` by default.
To apply the same cache decision without starting the stack, run `grove up --setup-only`.

## Limit concurrent startup

`grove up --heavy` opts into two startup slots shared across repositories using the
same user's Grove state root. Plain `grove up` does not join this gate. All cooperating
lanes must opt in for the limit to cover their startups.

Grove first takes the instance's existing up lock, then reserves a startup slot and
checks the same `[admission]` load threshold used by `run --heavy`. If load is at or
above the threshold, Grove releases the slot before waiting. One admission timeout
covers both slot and load waits. It does not include the preceding instance-lock wait.
The log reports `timing: startup queue` separately from setup, seeds and readiness.
Instance/log bookkeeping can happen before admission. Resources and services cannot.

The slot stays held through resource initialization, dependency setup, seeds and
service readiness. The gate does not change readiness timeouts. It polls every 200 ms,
reports waiting every 5 seconds, and has no FIFO guarantee. Load can rise after
admission. This gate does not limit the number of running stacks.

OS locks release when startup returns or the worker exits, including failure and
cancellation. Setup commands and service daemons do not inherit a slot. Existing
cancellation still signals only the Grove worker through the log supervisor.
An already-running setup subprocess can continue after Grove releases the slot.
Do not remove the `startup-slots/*.lock` files under the state root while Grove runs.

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
Backup and restore use atomic renames on the same filesystem. Across filesystems,
Grove copies the install, preserving symlinks and permissions, before removing the source.
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
file names or file contents change. Without `setup_inputs`, the command supplies
its identity. Missing or unreadable inputs fail setup. Setup that fails or changes
its inputs does not receive a completed receipt. When `uv` is available, Grove also
reruns setup when an existing `.venv` does not match `uv.lock`.

Uncached setup records whether it produced a local Python venv. Grove requires
`.venv/pyvenv.cfg`, an executable `.venv/bin/python`, and a matching directory in
`.venv/.grove-setup-path` before it reuses that setup receipt. If a worktree disappears
and returns at the same path, the external receipt cannot stand in for its missing venv.

Grove repairs missing, partial, relocated or unrecorded venvs through the configured
setup command. It stops its owned service during replacement and keeps the prior
install outside the worktree for rollback. Setup must restore the expected venv
before Grove marks it complete. An incomplete receipt preserves that requirement
across failed retries. Ordinary uncached setup has no general rollback transaction.

Old markers do not record expected output. The first `up` with versioned receipts
reruns each legacy uncached setup once, including setups that produce no venv.
Install this upgrade during a quiet window. Healthy venvs with valid path records
can reuse their files during that one-time setup run. Later unchanged runs skip setup.

Venv checks apply only to `.venv` in the service's setup directory. Grove refuses
symlinked venv directories. It does not inventory installed packages or check arbitrary
outputs from setups that produce no venv. Custom venv locations remain outside this check.

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

## macOS directory clones

On macOS, `grove up` restores warm cache entries with APFS directory cloning by default.
Clones have separate inodes and isolate writes from the store and other worktrees while
sharing blocks through copy-on-write. Set `GROVE_MACOS_CLONE=0` to opt out and use per-file
hardlinks. Only the exact value `0` disables cloning; unset, empty, and all other values enable it.
Linux ignores this variable and always uses hardlinks. Cold installs and cache publication
also retain hardlinks. A matching setup marker still skips setup, so changing the variable
alone does not replace an existing install.

Grove uses directory `clonefile` with `CLONE_NOFOLLOW`, preserving symlinks without following them.
Apple discourages this API for directory trees:
see [clonefile](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2).
If cloning fails and the destination was not created, Grove prints a warning and falls back
to per-file hardlinks. If a destination exists after failure, setup fails and preserves the
previous install. Hardlink restores share inodes and do not isolate writes.

After cloning, Grove recounts files and checks the stored inventory before replacing the
install. This detects missing entries, but does not verify file names or contents.
Cleanup of the old install stays synchronous. Cleanup errors warn and retain the backup.

Disk accounting has a known limitation: Grove detects shared hardlinks, but cannot
distinguish APFS clone extents from private blocks through that check. Unique-byte and
reclaimable-space figures in `ls`, `health`, and `clean` can therefore overstate cloned
installs. This change does not fix accounting for cloned blocks.

Grove measures private-dependency footprint only when `grove up --footprint` is passed.
`ls` and `health` read the stored figure; unmeasured instances point to this flag.

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

## Resource recovery and database names

If a datastore port does not answer, Grove inspects its named container before it creates one.
A stopped container restarts by ID only when its configured image and published ports match the resource.
A resource's configured args are satisfied when they appear contiguously in the container command; extra container flags are allowed.
A mismatch or inspection error stops `up` without replacing the container or deleting its data.
An existing running container gets the normal readiness wait. A reachable external datastore still needs no managed container.

Rendered database names longer than 63 bytes use a shortened prefix and a stable 16-digit hash of the full name.
Names at or below the limit stay unchanged. Grove uses the same bounded name in generated environment files, seeds and the registry.
The instance slug, service paths and port allocation stay unchanged.

## Mongo readiness before seeds

Before each seed that needs to run, Grove requires Mongo to answer `hello` with
`isWritablePrimary=true` through `127.0.0.1:<published port>` and `directConnection=true`.
This applies to resources named `mongo`, resources with the `mongo` image, and
resources with `db_name`, which Grove already treats as Mongo databases.

Grove uses `.venv/bin/python` and PyMongo in the seed's working directory when that
interpreter exists. Otherwise it uses host `mongosh`. A missing client or missing
PyMongo stops the seed with an error. The probe never uses `docker exec`.

The readiness deadline is 180 seconds per resource before each seed, with attempts
bounded to five seconds. The log reports readiness time separately from seed time.
Skipped seeds need no probe. Grove does not replay a failed seed automatically.
Seed logs are private (0600, directory 0700); the terminal shows status, heartbeat and the first error line only; read the log for the rest.
An OOM or disconnect after the probe can still fail a seed.

Each `[[seed]]` accepts a positive `timeout` duration, default `"20m"`, using the
same units as `ready.timeout`. This bounds the seed command after Mongo readiness.
When the deadline expires, Grove kills the seed's process group and fails `up`.
The error names the seed, elapsed time, log path, and recovery steps.

Ordinary `grove health` reports available disk space and recorded instance usage,
but labels the store size as unmeasured. It does not recursively scan the store
before it reports health findings.
Ports held by colima, Docker Desktop, or other recognized container forwards are reported as forwards; health never suggests killing the forwarder process.

## Resource initialization failures

Grove stops `up` when a managed resource's init command exits nonzero, before dependency setup, seeds, or services start.
It saves a pending-init record under its state directory, tied to the container ID, before it runs the command.
The next `up` retries that command even if the port already answers. Only a successful command clears the record.

A resource lock serializes startup and initialization across worktrees that share the resource name.
Existing reachable resources without a pending record keep their current behavior. Grove does not initialize external databases.

Init commands must be safe to retry after partial completion. For a Mongo replica set, this example accepts success or only `AlreadyInitialized`:

```toml
init = "try { const r = rs.initiate(); if (r.ok !== 1) throw r; } catch (e) { if (e.code !== 23) { printjson(e); quit(1); } }"
```

MongoDB defines [error 23 as AlreadyInitialized](https://www.mongodb.com/docs/manual/reference/error-codes/).
Other errors and unexpected results exit nonzero. Grove does not suppress them.
Init runs through `mongosh --eval`; a custom script must report failure with a nonzero exit, rather than print an error and exit successfully.
Grove leaves the container intact after failure so a retry can finish initialization without data loss.

## Stale Mongo testcontainers

`grove test-containers` reports candidates without removing them. It queries the
current Docker context and never restarts Docker or Colima. Use
`grove test-containers --remove-stale` only when you intend to remove idle test databases.

A candidate must have `org.testcontainers=true`, use a recognized official Mongo
image, and have run for more than 45 minutes since its latest start. Grove excludes
containers named `grove-*`, including the shared application database.

Grove checks both IPv4 and IPv6 socket tables inside the container for established
connections on Mongo port 27017. It also checks established host connections on
every published Mongo port through `lsof`. Missing ports, failed commands, malformed
metadata or incomplete socket evidence mean keep. Probes have a ten-second deadline.

Removal repeats the checks, confirms that the container identity and start time
have not changed, and removes that exact full ID. It never uses a broad prune.
A connection can appear after the last check. Idle does not prove that the owner
process is gone, so this remains an explicit operator action, not an automatic job.
