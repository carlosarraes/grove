# Shared test Mongo pilot

The Northstar pilot proves separate database ownership and cleanup. Its timing samples do not establish a reliable speedup.
The feature remains on `feature-shared-test-mongo`, with no release or installed-binary change.
The platform adapter is the first stage of MON-6493.

## Behavior

`run --test-mongo` verifies a dedicated loopback replica set before the child starts.
Each run gets a random namespace and records every database allocation before use.
Cleanup drops only those recorded databases after the command group exits.
Failed cleanup retains the record. A later run retries only after the lease is free and no process remains in the recorded group.
Grove retains the record when process ownership is uncertain.

Cancellation forwards to the command group. Interactive commands receive the foreground terminal, which Grove restores afterward.
The allocator rejects callers outside the owned group. Commands must not detach from that group.
Per-run Mongo users remain outside scope. These trusted tests still share server capacity and permissions.

The approved order with fleetlock is:

```sh
grove run --heavy -- fleetlock run pytest <lane> <card> -- grove run --test-mongo -- <cmd>
```

## Measurements

All resource probes used isolated `mongo:8.0.20` containers.

| Measurement | Result |
|---|---|
| Mac resource cold / reuse | 3.23 s / 0.525 s |
| Linux resource cold / reuse | 1.50 s / 0.282 s |
| Existing Northstar provider | 56 passed, zero skipped, 12.50 s total |
| New disposable provider and adapter checks | 62 passed, zero skipped, 8.29 s inside pytest |
| Grove cold pilot | 56 passed, zero skipped, 13.33 s total |
| Grove warm pilot | 56 passed, zero skipped, 16.77 s total |
| Grove warm pilot with phase output | 56 passed, zero skipped, 10.41 s total |
| Two simultaneous Grove pilots | 56 passed each, exit 0 each, 15.43 s combined |

The phase-timed run spent 0.406 s on the resource, 9.219 s on the child, and 0.514 s on cleanup.
The two simultaneous runs recorded 22 database names each. Their inventories were disjoint.
A Mongo query after completion found none of those 44 databases. Grove removed both run records.
The total durations above exclude admission waits.

The baseline resource startup cost was only 2.1 seconds. Most elapsed time belongs to Python startup and the tests.
Variable machine load limits comparisons between these individual samples. Measure the heavier suites before a broader performance claim.

## Validation and rollout

The resource regressions cover ownership, binding, version, initialization, and concurrent startup.
The lifecycle regressions cover concurrent allocation, exit codes, cancellation, terminal input, failed cleanup, live orphan children, and uncertain records.
Independent review found one terminal ownership bug. A real terminal regression failed before the fix and passed afterward.
The platform adapter received a separate review with no blocking findings.

The platform PR retains disposable containers in CI and adds missing currency persistence and cutover coverage.
The fixture also retains the explicit Northstar test URI. Partial Grove settings or allocation failure produce errors.
On this Colima host, testcontainers needed the Docker endpoint and `TESTCONTAINERS_DOCKER_SOCKET_OVERRIDE=/var/run/docker.sock`.

The Deal Hub trial is complete. Its results and the decision below supersede the proposed rollout.
Fleetlock settings and installed Grove binaries remain unchanged.

The Mac full suite passed 243 tests. The Linux full suite passed 242 tests.
Both machines passed formatting, Clippy with warnings denied, and packaging checks.
Earlier Mac runs hit existing timing-sensitive supervision tests and a restart port race.
Their focused reruns passed, followed by the successful full Mac run.

## Closing decision after the Deal Hub trial

PM parked this feature without a release. Keep the code and design on `feature-shared-test-mongo` for a future workload that justifies it.
The [Deal Hub results](2026-10-02-shared-test-mongo-results.md) show that parallel containers match shared Mongo for elapsed time.
Shared Mongo saved about 3% of combined process memory at concurrency two and three.

| Concurrent runs | Container batch | Shared batch |
|---:|---:|---:|
| 1 | 78.4 s | 74.9 s |
| 2 | 65.3 s | 66.3 s |
| 3 | 88.9 s | 89.3 s |

All 5,328 benchmark cases passed. The six shared runs had disjoint inventories, and cleanup left none of their 132 recorded database names.

Close platform PR #6198 because its optional Grove path has no released provider. Keep MON-6501 in Backlog with the evidence.
PR #6197 remains useful independently because its container fallback runs the Northstar currency suites locally instead of skipping them.
That PR still awaits Carlos's merge decision.

PM will pilot two heavy pytest slots with default core-count admission before fleetlock and measure queue waits.
Three slots and the shared Mongo release remain outside that pilot. This decision changes neither installed Grove binaries nor fleetlock capacity.
