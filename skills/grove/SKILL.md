---
name: grove
description: Use when starting or restarting this repo's dev servers in a git worktree, a port or env file is missing, tests need a live server or fail for unexplained reasons, an instance must be reached from another machine, a machine is short of disk, or work in a worktree is finished.
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

## Viewing from another machine

If the repo's `.grove.toml` opts in to network-aware templates, expose only this
instance with:

```
grove up --expose                       # default-route IPv4
grove up --expose-host dev-mac.local   # explicit VPN, Tailscale, or LAN host
```

The config must use `{{ host.bind }}` in each service's bind flag,
`{{ host.public }}` in browser-facing URLs, and the same public origin in CORS and
redirect allowlists. This coordination prevents a laptop's browser from calling its own
localhost. Grove supplies the values but cannot infer each framework's bind or CORS
settings; run `grove --llm` for a complete example.

Exposure is persisted for this instance, re-renders its config, and restarts its
services. `grove status` and `grove ls` show the public host; sibling instances keep
their own mode. Plain `grove up` switches this instance back to localhost-only.

Exposed services bind all interfaces. Grove does not add a firewall, TLS,
authentication, or a tunnel, so development services and authentication bypasses may be
reachable by other machines on the network.

## Working

Commands that need to reach the instance get its ports from `grove run`:

```
grove run -- pytest tests/integration -v
grove run -- sh -c 'curl localhost:$GROVE_PORT_BACKEND/health'
```

`run` exports `GROVE_PORT_<NAME>` for every declared port — the config's `web` becomes
`GROVE_PORT_WEB` — plus `GROVE_SLUG`, `GROVE_DB_NAME`, `GROVE_WORKTREE`, and every
per-instance override the config declares.

It also sets `AGENT_BROWSER_SESSION`, which is what keeps parallel browser automation from
sharing one session — see *Browser testing* below.

Commands that read the worktree's own env files — the ordinary unit-test loop — work
unwrapped, because `up` wrote those files to disk.

Grove renders configured dotenv files and overlays per-instance variables on commands it
starts; it does not provide an empty settings environment. Tests that assert application
defaults must disable dotenv loading and clear the relevant process variables in the
repo's own fixture or settings constructor.

`grove status` shows what is running and, for every service with a `ready.http`, whether
that endpoint actually answers. `--json` makes it parseable.

After editing a service that does not reload itself, replace just that one:

```
grove restart backend
```

`status` flags a service that started before your newest edit, because it goes on serving
the code you already changed and nothing else says so.

## Finishing

```
grove down
```

Starting an instance is cheap and leaving one running is invisible, so a machine
accumulates them until they start costing someone else — see *When tests fail* below.
Stopping one when the ticket is done is what keeps that from happening.

`down` stops the services, which returns the CPU and memory a crowded machine is short
of. It keeps three things on purpose: the port reservation, so a URL written down while
the instance ran still works when it comes back; the dependency trees `setup` installed,
often a gigabyte per worktree, which nothing removes until you ask; and the database.

When the branch is done and the machine is short of disk, the checkout is what holds it.
grove never deletes a worktree, so that step is yours:

```
git worktree remove <path>       # frees the checkout and everything setup put in it
grove prune                      # then forgets the instance and frees its ports
```

The database stays until `grove prune --purge`, and `prune` names every one it leaves.

When several have already piled up, stop the ones nobody is working in:

```
grove down --idle 2h --dry-run   # name them first
grove down --idle 2h             # then stop them
grove down --all-but-this        # everything except the worktree you are in
```

These keep each instance's port reservation, so a URL written down while one was running
still works when it comes back. (`grove prune` is the other case: instances whose worktree
has been deleted.)

**Read the list before sweeping when other agents share the machine.** grove counts an
instance as busy while its services are still writing to their logs, which covers an agent
driving a browser through a long QA pass. A box that is quiet for some other reason — a
review paused mid-read, a session waiting on a human — looks exactly like an abandoned
one. `--dry-run` names every casualty, generous windows cost little, and where you cannot
tell whose instance is whose, `grove down` in each worktree you own is the certain move.

## When a service needs generated code

Some services cannot start correctly from a fresh checkout — a frontend that compiles
against types generated from its own backend, a client built from a schema. Generated
output has to track what it was generated from, so a worktree that regenerates only once
serves stale code the moment the thing it mirrors changes.

That is what a service's `prepare` is for. `up` runs it **every time**, before that
service starts and after the services declared above it are answering, so a generator can
read this instance's own backend:

```toml
[[service]]
name = "frontend"
prepare = "npm run contracts:generate"
```

Three config fields run commands and the difference is only how often: `setup` once per
worktree (dependency installs), `[[seed]]` once per instance (fixture data), `prepare` on
every `up` (generation). A failing `prepare` fails `up` and prints what it printed, rather
than starting a service on top of half a file.

## Seeding

`up` runs the config's `[[seed]]` blocks once per worktree, after dependencies and before
the services, and prints what each one did. A fresh instance has its own empty database,
so whatever guarded routes need — an organisation row, a fixture — comes from there.
Grove records the managed container incarnation with each seed marker, so recreating a
shared resource makes every worktree seed again on its next `up`.

### Putting the data back

Testing dirties an instance — records renamed, rows deleted, states flipped. To discard
all of that and rebuild the instance's data from scratch:

```
grove seed --force
```

This re-runs every `[[seed]]` block against the current database, which for a seed that
drops and rebuilds is a full reset. Reach for it whenever the data is in a shape you no
longer want, before a demo, or when a test needs a known starting point. Ordinary `up`
never re-seeds, so an instance keeps whatever you did to it until you ask for this.

A route answering 403 or "not found" in a fresh instance usually means missing seed data
rather than a broken login, because the error names authentication either way.

## When tests fail in ways your change does not explain

One dev stack per worktree means a machine can end up carrying a dozen of them. That cost
never arrives as a grove error. It arrives as a **noisy neighbour**: the box is
oversubscribed, and your tests time out on a branch that did not break them.

The signature — any one of these is enough to suspect it:

- Timeouts rather than assertion failures, especially where a test mounts a real component.
- Failures in files your change never touched.
- A failure count that drifts between runs — six, then four, then two.
- A clean checkout of the base branch fails the same tests.

Check the machine before you check the branch:

```
grove ls
```

The footer reports load against core count and how many instances are up. Load at or past
the core count, with a crowd of instances behind it, means the machine is the suspect and
the branch is probably innocent. A full disk is the other way a crowd costs someone, and
the load line says nothing about it: a machine can idle at two on ten cores with no room
left to write a log. `grove ls --json` carries the same numbers plus each
instance's idle age, for a decision made in code rather than by reading.

The fix is *Finishing* above: stop what nobody is using, then re-run the failing tests
before changing any code.

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
grove logs <service> --since-restart
```

The log keeps the whole history including the dependency install, so `--since-restart`
starts at the current run and `-n` limits it to the last lines.

### When requests fail but the service is up

A process can survive while the thing it serves is dead — most often because the shared
datastore underneath it went away mid-run. Nothing crashes, nothing appears in the process
list as wrong, and every request fails.

`grove status` separates the two:

```
services
  frontend  running  pid 41201  answering
  backend   running  pid 41202  NOT ANSWERING http://localhost:24311/health
            grove logs backend --since-restart
```

**`NOT ANSWERING` on a running service is grove's problem, not your test's and not the
browser's.** Read that log, then run `grove doctor`, which checks the datastores the
service depends on. Reach for browser or client-side recovery only once the service says
`answering` — otherwise you are fixing the wrong end of the request.

## Browser testing on an instance's port

Each instance serves on a port assigned at run time, which an external identity provider
will not have in its redirect allowlist — a login bounces with an invalid-redirect error
that looks like broken auth. Where the repo offers a bypass, prefer it; otherwise open a
static path first and seed the session there rather than starting at `/`.

`grove run` sets `AGENT_BROWSER_SESSION` to this instance's slug, so browser automation
invoked through it gets its own session. Driving a browser outside `grove run` shares one
session across every instance on the machine, and a sibling's navigation will take the
tab out from under you.

## When the repo has no .grove.toml

`up` will say so and point here. Run:

```
grove --llm
```

That prints the schema and a worked example. Write `.grove.toml` at the worktree root
and commit it — it is checked in, so every later agent in every worktree only runs
`grove up`.
