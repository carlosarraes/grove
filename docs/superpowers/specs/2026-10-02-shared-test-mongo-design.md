# Shared Mongo for test runs

PM approved implementation on 2026-10-02 after review of the Grove pytest request.
Load admission ships first. This feature stays opt-in and awaits a separate release review.

## Approved contract

- `grove run --test-mongo -- <command>` owns a test run. Plain run retains its behavior.
- One dedicated Mongo replica set serves the machine's tests. Its published port binds to loopback.
- The repository declares the image pin. The Northstar pilot requires `mongo:8.0.20` and replica set `rs0`.
- Serialize creation and initialization. Validate ownership, image, server version, port mapping, and writable-primary readiness.
- Export the run ID, URI, primary database name, and allocation command only to the child.
- Preserve the development instance's database and environment settings.
- Preserve independent session, per-test, and no-index databases within one run-owned namespace.
- Record every allocated database before use. Cleanup reads that exact inventory and never enumerates by prefix.
- Per-run Mongo users are out of scope. Names and ownership records prevent collisions and cleanup of another run's data.
- Database names do not prevent arbitrary cross-database writes by trusted test code. Shared CPU, memory, and availability remain shared.
- Forward cancellation, stop the owned command group, and preserve its exit status.
- Keep failed cleanup records. Retry abandoned cleanup only when the run lease is free and its recorded process group has no live processes.
- Retain records with uncertain process ownership. Age alone never authorizes cleanup.
- Pilot Northstar currency first, then Deal Hub, then Actions and Northstar persistence. Measure each migration before and after.
- Fleetlock stays external. The order is load admission, fleetlock admission, then the command.

## Interfaces

A root `[test_mongo]` section declares `image`, optional `port`, and optional `name`.
Defaults are port 27018 and container `grove-test-mongo`. The image has no default.
An existing container with different settings fails explicitly. Grove never replaces it automatically.

Child variables are `GROVE_TEST_RUN_ID`, `GROVE_TEST_MONGODB_URI`, `GROVE_TEST_DB_NAME`,
and `GROVE_TEST_ALLOCATOR`. The allocator is the current Grove binary's absolute path.
`grove test-db allocate` requires an active run environment and prints one newly recorded database name.
Allocation checks the lease and run state. It rejects use outside the owning run's process group.

State lives under Grove's state directory, separately from development instances.
A resource lock protects provisioning. A run lease protects lifecycle ownership.
A separate short metadata lock protects concurrent allocations and atomic record updates.
Records include the run ID, container identity, port, child process group, phase, and exact database inventory.
A crash during child registration leaves an uncertain record for explicit inspection instead of unsafe cleanup.

The first migration uses a shared Python helper for the URI and allocation operation.
Existing non-Grove fixture behavior stays available. Partial or broken Grove environment fails explicitly.
Northstar's existing URI variable remains supported for its existing standalone mode.

## Validation

Prove concurrent startup, distinct run and fixture names, exact recorded cleanup, and child exit propagation.
Exercise cancellation, a killed supervisor with a live child, failed cleanup retry, and uncertain ownership.
Verify foreign resources, wrong versions, and failed initialization produce errors before the test command starts.
Run the named Northstar tests against the existing provider and the shared provider with equal test counts and skips.
Measure resource startup, fixture and test time, cleanup, and concurrent run behavior on Mac and Linux.
