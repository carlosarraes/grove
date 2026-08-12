# Resource Incarnation and Diagnostics Design

**Date:** 2026-08-12
**Status:** Approved direction; awaiting written-spec review

## Problem

Grove currently treats a seed marker's existence as proof that the seed's effects are
still present. A Grove-managed shared container can be removed and recreated with an
empty datastore while every worktree retains its old seed markers. The next `grove up`
then skips seeding and starts an application against missing data.

The same missing resource identity makes `grove doctor` too shallow. It probes only the
declared port, so it cannot distinguish a healthy v0.1.11 container from an older
low-`nofile` container. When a named Grove container has stopped, doctor says `grove up`
will start the resource even though Docker will reject the occupied container name.

Separately, Grove's documentation does not state clearly enough that rendered dotenv
files remain application inputs. `grove run` overlays instance variables; it does not
provide a clean settings environment.

## Goals

1. Rerun a worktree's seeds when a Grove-managed resource it used has a different
   container incarnation.
2. Rerun a seed when its rendered command changes.
3. Make `doctor` report actionable state for the exact container name Grove manages:
   identity, state, exit code, and expected versus observed `nofile`.
4. Show a short resource log tail when a managed container is not usable.
5. Preserve Grove's port-first support for externally supplied resources.
6. Document that settings-default tests must disable dotenv loading and clear relevant
   process variables in the repository's own test setup.

## Non-goals

- No seed `verify` command in this change. Incarnation tracking fixes the observed
  lifecycle bug without adding an application-specific command to every `up`.
- No automatic container removal or recreation.
- No volume management, data backup, or datastore-specific health checks.
- No attempt to identify the incarnation of a resource Grove does not manage.
- No Mondrio repository changes.
- No status JSON or URL changes.
- No new load-warning behavior.

## Resource observation module

`resource` will own one observation interface that hides Docker subprocesses and Docker
JSON from callers. An observation contains:

- whether the declared TCP port answers;
- optionally, a managed container with its full ID, running state, exit code, and
  observed `nofile` soft and hard limits;
- an optional diagnostic error when Docker metadata or logs could not be read.

A container is managed for this purpose when Docker has a container with Grove's exact
existing name, `grove-<resource.name>`. This covers containers created before labels
exist and matches the name Grove already claims when starting a resource. The full ID is
stored as the stable incarnation; output abbreviates it for readability.

Docker observation is best effort. Port reachability remains the reuse decision and an
external resource answering on the port remains valid when Docker is absent, its daemon
is unavailable, or no matching container exists. Docker failures become diagnostics,
not a new dependency for external resources.

The expected open-file limit lives as one resource-module constant and is used by both
`docker run` construction and doctor comparison. This prevents launch policy and
diagnostics from drifting.

## Seed marker validity

Each successful seed writes a versioned JSON marker containing:

- the rendered seed command;
- the full container ID for every currently observable Grove-managed resource.

Until seeds can declare individual resource dependencies, every seed snapshot includes
all managed resources in the repository config. This is deliberately conservative: if
any datastore Grove owns is recreated, all repository seeds regain their invariants.

Before skipping a seed, Grove renders the current command and observes current managed
resource IDs. A marker is valid only when the command matches and every positively
identified current managed container has the same ID in the marker. A different ID, or
a newly managed resource missing from a structured marker, reruns the seed. The outcome
names the reason, such as `command changed` or `resource mongo was recreated`.

Externally supplied or temporarily unobservable resources do not invalidate a marker.
They retain today's marker behavior because a TCP connection provides no trustworthy
identity, and a Docker outage is not evidence that data disappeared. An existing
managed ID remains in the marker until a positively identified replacement supersedes
it. `grove seed --force` continues to rerun every eligible seed regardless of the marker.

### Legacy markers

Existing markers contain only the rendered command. Migration avoids a surprise global
reseed after upgrade:

- If the legacy command differs from the current rendered command, rerun the seed.
- If Grove started any managed resource during the current command, rerun the seed; the
  resource is known to be a new empty incarnation.
- Otherwise, preserve the skip, rewrite the marker in the structured format with the
  current resource snapshot, and report it as already seeded.

This means an old marker can be upgraded safely during ordinary use while a resource
recreated by that same `grove up` cannot be mistaken for populated data.
Recreation that happened before this Grove version is deliberately not inferred: doing
so would require automatically reseeding every existing worktree during migration.

Resource-start results must therefore flow through the instance for the duration of one
command. A standalone `grove seed` performs passive observation but never starts or
recreates a resource.

## Doctor behavior

Doctor combines port reachability with the managed-container observation:

| Port | Managed container | Verdict |
|---|---|---|
| answering | running, `nofile=64000:64000` | `ok`, with short ID and observed limit |
| answering | running, different or missing limit | `warn`, expected vs observed and safe recreation guidance |
| answering | absent/unobservable | `ok`, explicitly described as external or unobserved |
| not answering | absent, image configured | `warn`, `grove up` can start it |
| not answering | Docker itself unavailable, image configured | `FAIL`, because `grove up` cannot start it |
| not answering | stopped container present | `FAIL`, with ID, exit code, log tail, and preserve/remove/recreate guidance |
| not answering | running container present | `FAIL`, with ID and log tail because the managed container is unusable |
| not answering | absent, no image | existing `FAIL` telling the user to provide the resource |

A stopped container is a failure rather than a warning because its name prevents the
promised `docker run`. Resource logs are fetched only for unusable managed containers
and limited to the last ten lines. A log-read failure is mentioned briefly without
hiding the primary state.

Doctor remains read-only. It never restarts, removes, or mutates a container.

## Settings-test documentation

The README, compiled agent skill, and `grove --llm` reference will state:

- Grove writes configured dotenv targets to disk.
- Grove-launched commands receive per-instance variables overlaid on their inherited
  environment.
- Grove does not promise an empty or isolated settings environment.
- Tests asserting application defaults must disable dotenv loading and clear the
  relevant process variables in repository-owned fixtures or settings constructors.

The `run` help summary will use “overlay” or “layer” rather than wording that implies
isolation.

## Error handling and compatibility

- Marker files are written only after a successful seed, as today.
- Invalid structured marker data is treated as stale and rerun rather than trusted.
- A failed seed leaves no new valid marker.
- Full container IDs are persisted; abbreviated IDs are display-only.
- Existing plain-text markers remain readable under the migration rules above.
- Existing external-resource behavior remains port-first and Docker-optional.
- Existing CLI JSON formats are unchanged.

## Test strategy

Tests exercise the resource interface rather than requiring a real Docker daemon.
Docker inspect and log output parsing receive deterministic fixtures behind a private
test seam.

Regression coverage will prove:

1. An unchanged command and container ID skip a previously successful seed.
2. A changed container ID reruns the seed on each worktree's next `up`, even in a fresh
   Grove process that did not perform the recreation.
3. A resource started during the current `up` invalidates a legacy marker.
4. A matching legacy command with no current resource start migrates without rerunning.
5. A changed seed command reruns and replaces its marker.
6. `--force` still reruns unconditionally.
7. External or unobservable resources retain port-only seed behavior.
8. Doctor reports a healthy 64k container, warns about an old limit, and fails with the
   exit code and log tail for a stopped named container.
9. Doctor still accepts an external resource when Docker inspection is unavailable.
10. Help, LLM reference, README, and skill contain the settings-isolation explanation.

The final verification remains `just check`, covering formatting, clippy, all tests, and
packaging on the repository's supported build target.
