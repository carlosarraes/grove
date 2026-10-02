# Deal Hub shared Mongo results

## Decision

PM parked shared Mongo without a release. PM will pilot two heavy slots with default core-count admission before fleetlock.
The measurements support parallel runs for this workload.
Shared Mongo did not improve elapsed time against parallel containers. It used about 3% less combined process memory at concurrency two and three.
Keep `feature-shared-test-mongo` unreleased with its design and measurements. Close #6198 and keep MON-6501 in Backlog.

## Results

Each run executed the same 444 cases. All 12 JUnit case lists match exactly.
All 5,328 cases passed with no failures, errors or skips. No flaky failure occurred in these samples.
Each run reported 73 existing warnings from Pydantic serializers, including the unchanged container baseline.

| Provider | Concurrent runs | Batch seconds | Peak load | Test RSS MiB | Mongo resident MiB | Peak combined MiB | Peak connections |
|---|---:|---:|---:|---:|---:|---:|---:|
| container | 1 | 78.4 | 16.90 | 487 | 408 | 878 | 20 |
| shared | 1 | 74.9 | 14.38 | 463 | 431 | 867 | 20 |
| container | 2 | 65.3 | 16.08 | 912 | 815 | 1668 | 40 |
| shared | 2 | 66.3 | 17.55 | 929 | 688 | 1617 | 37 |
| container | 3 | 88.9 | 17.75 | 1376 | 1223 | 2540 | 60 |
| shared | 3 | 89.3 | 18.71 | 1400 | 1127 | 2455 | 54 |

Combined memory is the largest sampled sum of test-process RSS and Mongo resident memory. It is not total host memory.
The separate component peaks can occur at different times. Mongo runs inside the Docker VM.
Samples are approximately one second apart. Connection counts include one monitoring client per server.

## Isolation and cleanup

Each shared run observed 21 fixture database names within its own namespace.
Its recorded inventory contained those 21 names plus the initial run database.

All concurrent inventories were disjoint. All 132 recorded names from the six shared runs were absent after cleanup.
Grove removed all completed run records. Each shared batch used one Mongo server. Container batches used one server per run.

The container descriptor guard passed. The shared provider cannot attribute server descriptors to individual runs, as with the existing local URI provider.

## Load and ordering

The first attempt waited ten minutes at the default threshold of 10, then exited before pytest started.
I canceled two pending regression commands before admission with exit 130.
PM approved a temporary threshold of 18 for both benchmark variants. Only this pilot worktree's ignored config changed.

Each batch used one admitted fleetlock slot. No fleetlock capacity or Grove default changed.
The batch order was container 1, shared 1, shared 2, container 2, container 3, shared 3.
The first pair included the implementation and regression interval. The two-run and three-run pairs ran back to back.

PM identified Spotlight, Rippling and system_profiler as background CPU users. These samples include that host load.
Admission checks the load before the slot wait. Load can rise during a batch, as the three-run shared sample shows.

## Interpretation and limits

Two shared runs completed in 66.3 seconds. Twice the single shared sample would take 149.8 seconds.
Two container runs completed in 65.3 seconds. Twice the single container sample would take 156.7 seconds.

Those sequential totals are estimates from single samples, not measured queue waits.
The two-run samples were faster than either single sample, which shows noise or cache effects in this sequence.

Three shared runs improved throughput by about 11% over two, but used about 52% more combined process memory.
Two slots are the smaller first trial. These samples do not establish safety for every heavy suite or sustained fleet traffic.

The shared server provides run-owned allocation and recorded cleanup. This test does not prove protection against arbitrary cross-database writes by trusted tests.

## Evidence

The raw result, sample, inventory, JUnit and per-run logs remain on the measurement Mac under `~/mondrio/pm-notes/mon-6501-benchmark/`.
That local directory also contains `method.md`, the runner, the observer and `implementation-plan.md`.
The source changes are on `feature/mon-6501-testdeal-hub-adopt-run-owned-shared-mongo-databases` in `wt-mon-6501`.

## Focused verification

The new namespace regression failed before fixture changes because its databases did not belong to the Grove run.
The provider regressions failed before the new selector existed. After the change, all nine focused tests passed with each provider.
A separate container regression attempt inherited `USE_LOCAL_MONGO_FOR_TESTS=1` and failed against the closed local port.
The corrected command cleared that flag and passed. The benchmark already cleared it, so its samples were unaffected.
Focused Ruff and Pyright checks passed. The full Deal Hub suite remains for CI.
The temporary admission override was removed after verification.

Parked PR: https://github.com/Mondrio-App/mondrio-platform/pull/6198. Head `9247eac7f0`. PM received this report.
