# Resource Incarnation and Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make seed completion follow Grove-managed resource incarnations, make `doctor` expose managed-container launch failures and limits, and document why settings-default tests must disable dotenv themselves.

**Architecture:** Deepen `resource` into the single interface for port reachability, Docker observation, launch, and diagnostic log capture. `Instance` persists versioned seed markers containing rendered commands and observed managed-container IDs, while `doctor` renders verdicts from the same resource observations. Docker remains best-effort for a reachable external resource and mandatory only when Grove must start its declared image.

**Tech Stack:** Rust 2024, `serde`/`serde_json`, `clap`, Docker CLI subprocesses, `assert_cmd`, shell-backed fake Docker integration fixtures.

## Global Constraints

- The managed container name remains exactly `grove-<resource.name>`.
- Every Grove-created `docker-shared` container uses `nofile=64000:64000`.
- Port reachability remains the reuse decision for external resources.
- Docker observation failures must not reject a resource already answering on its port.
- Doctor is read-only and never restarts, removes, or mutates containers.
- Existing containers are never recreated automatically.
- Full container IDs are persisted; display output may abbreviate them.
- Existing plain-text seed markers remain readable and migrate without a global reseed.
- No seed verify command, Mondrio change, status JSON change, URL change, or load-warning change.

---

## File map

- `src/resource.rs`: expected limit constant, Docker command adapter, managed-container observation, ensure result, and diagnostic logs.
- `src/instance.rs`: resource snapshot flow, versioned seed markers, marker comparison/migration, and seed outcomes.
- `src/doctor.rs`: resource-state-to-verdict policy and user-facing diagnostic text.
- `src/main.rs`: pass started resource state into seeding and clarify `run` help wording.
- `tests/resource.rs`: launch policy and public resource-observation behavior.
- `tests/cli.rs`: end-to-end seed invalidation, legacy migration, and doctor diagnostics with a fake Docker executable.
- `tests/config.rs`: compiled LLM reference documentation assertion.
- `README.md`, `src/llm.rs`, `skills/grove/SKILL.md`: settings-test limitation.

---

### Task 1: Managed resource observation

**Files:**
- Modify: `src/resource.rs`
- Modify: `tests/resource.rs`

**Interfaces:**
- Produces: `pub const NOFILE_LIMIT: u64 = 64_000`.
- Produces: `ContainerObservation { id, running, exit_code, nofile }` and `Observation { reachable, container, docker_error }`.
- Produces: `pub fn observe(resource: &Resource) -> Observation`.
- Produces: `EnsureResult { started: bool, observation: Observation }` from `ensure`.
- Produces: `pub fn logs(resource: &Resource, lines: usize) -> Result<String>`.
- Internal test seam: `GROVE_DOCKER` selects the Docker executable for child-process tests; it remains undocumented.

- [ ] **Step 1: Add failing launch-policy and inspect-parser tests**

Add unit tests inside `src/resource.rs` for Docker inspect JSON so private parsing remains private:

```rust
#[test]
fn inspect_reads_identity_state_exit_and_nofile() {
    let json = r#"[{"Id":"abcdef0123456789","State":{"Running":false,"ExitCode":133},"HostConfig":{"Ulimits":[{"Name":"nofile","Soft":64000,"Hard":64000}]}}]"#;
    let found = parse_inspect(json).expect("inspect").expect("container");
    assert_eq!(found.id, "abcdef0123456789");
    assert!(!found.running);
    assert_eq!(found.exit_code, 133);
    assert_eq!(found.nofile, Some((64_000, 64_000)));
}

#[test]
fn inspect_accepts_a_container_without_an_explicit_nofile() {
    let json = r#"[{"Id":"old","State":{"Running":true,"ExitCode":0},"HostConfig":{"Ulimits":[]}}]"#;
    assert_eq!(parse_inspect(json).unwrap().unwrap().nofile, None);
}
```

Keep the integration assertion in `tests/resource.rs` tied to the shared constant:

```rust
assert_eq!(argv[ulimit + 1], format!("nofile={0}:{0}", resource::NOFILE_LIMIT));
```

- [ ] **Step 2: Run the focused tests and observe RED**

Run: `cargo test --test resource docker_flags_precede_the_image -- --exact && cargo test resource::tests::inspect --lib`

Expected: compilation fails because `NOFILE_LIMIT` and `parse_inspect` do not exist.

- [ ] **Step 3: Implement the observation types and Docker parsing**

Add the public data types and a private serde representation:

```rust
pub const NOFILE_LIMIT: u64 = 64_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerObservation {
    pub id: String,
    pub running: bool,
    pub exit_code: i64,
    pub nofile: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub reachable: bool,
    pub container: Option<ContainerObservation>,
    pub docker_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EnsureResult {
    pub started: bool,
    pub observation: Observation,
}
```

Use `docker inspect grove-<name>` and deserialize the first array element. Map an exact Docker “No such object/container” failure to `container: None`; preserve every other stderr message as `docker_error`. Read the Docker executable with:

```rust
fn docker_program() -> OsString {
    std::env::var_os("GROVE_DOCKER").unwrap_or_else(|| "docker".into())
}
```

Build the launch limit from the constant:

```rust
format!("nofile={NOFILE_LIMIT}:{NOFILE_LIMIT}")
```

After a successful start and reachability wait, call `observe` and return `EnsureResult { started: true, observation }`. A reused resource returns `started: false` plus its observation. Implement `logs` as `docker logs --tail <lines> grove-<name>`.

- [ ] **Step 4: Run resource tests and observe GREEN**

Run: `cargo test --test resource && cargo test resource::tests --lib`

Expected: all resource integration and unit tests pass.

- [ ] **Step 5: Commit the resource module**

```bash
git add src/resource.rs tests/resource.rs
git commit -m "feat: observe managed resource containers"
```

---

### Task 2: Incarnation-aware seed markers

**Files:**
- Modify: `src/instance.rs`
- Modify: `src/main.rs`
- Modify: `tests/cli.rs`

**Interfaces:**
- Consumes: `resource::EnsureResult` and `resource::observe` from Task 1.
- Produces: `Instance::resources(&mut self) -> Result<Vec<String>>`, retaining started resource names for the current command.
- Produces: versioned JSON `SeedMarker { version, command, resources }`.
- Produces: seed output reasons for command changes and resource recreation.

- [ ] **Step 1: Add a fake-Docker CLI test harness**

Extend `Cli` with an optional executable passed as `GROVE_DOCKER` to every spawned Grove command. The fake executable reads a JSON fixture from a test-owned path and implements `inspect`, `run`, and `logs`. Its inspect output has Docker's real field names:

```json
[{"Id":"mongo-generation-a","State":{"Running":true,"ExitCode":0},"HostConfig":{"Ulimits":[{"Name":"nofile","Soft":64000,"Hard":64000}]}}]
```

Keep each fake and fixture inside the test's temp directory so parallel tests share no environment or state.

- [ ] **Step 2: Add failing seed lifecycle tests**

Add a config containing a reachable fake resource and a seed that appends to `seeded.log`. Cover these observable behaviors:

```rust
cli.run(&wt, &["up"]).success();
cli.run(&wt, &["up"]).success();
assert_eq!(read(&wt.join("seeded.log")), "feat_search\n");

docker.set_id("mongo-generation-b");
let output = cli.run(&wt, &["up"]).success();
assert_eq!(read(&wt.join("seeded.log")), "feat_search\nfeat_search\n");
assert!(stdout_and_stderr(output).contains("resource mongo was recreated"));
```

Also add focused cases proving:

- a matching legacy plain-text marker migrates to JSON without appending again;
- a legacy marker reruns when the fake `run` creates a newly reachable container in the current `up`;
- changing the configured seed command reruns it and reports `command changed`;
- malformed JSON beginning with `{` is stale and reruns;
- an inspect failure while the resource port answers does not invalidate a valid marker;
- `seed --force` still appends regardless of a valid structured marker.

- [ ] **Step 3: Run focused lifecycle tests and observe RED**

Run: `cargo test --test cli seed_ -- --nocapture`

Expected: changed container IDs and commands are skipped because markers are existence-only.

- [ ] **Step 4: Implement structured markers and comparison**

Add private marker types:

```rust
const SEED_MARKER_VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
struct SeedMarker {
    version: u8,
    command: String,
    #[serde(default)]
    resources: BTreeMap<String, String>,
}

enum StoredSeedMarker {
    Structured(SeedMarker),
    Legacy(String),
    Invalid,
}
```

Add `recently_started_resources: BTreeSet<String>` to `Instance`. Populate it from `EnsureResult.started`. Observe resource IDs once at the beginning of `seed`, then compare:

```rust
fn invalidation_reason(
    stored: &StoredSeedMarker,
    command: &str,
    current: &BTreeMap<String, String>,
    recently_started: &BTreeSet<String>,
) -> Option<String>
```

Rules, in order:

1. `--force` runs without comparison.
2. A changed command returns `command changed`.
3. A structured marker reruns for the first current ID that is absent or different.
4. A matching legacy marker reruns only when that resource was started in this command.
5. A malformed JSON-looking marker reruns as `seed marker was invalid`.
6. A missing marker runs normally.
7. Unobservable resources do not invalidate a structured marker.

When writing the successful marker, start with the prior structured resource map, overlay positively observed current IDs, and serialize pretty JSON. When a legacy marker migrates without running, rewrite it immediately with the current snapshot. Change `SeedOutcome::Ran` to carry an optional reason and render `seed org ... ok (resource mongo was recreated)` when present.

Make the `Up` command's `instance` mutable before `resources()`. `restart` has no resource-start event; its seed passively observes current IDs.

- [ ] **Step 5: Run seed tests and observe GREEN**

Run: `cargo test --test cli seed_ -- --nocapture`

Expected: all seed lifecycle tests pass, including legacy migration and forced seeding.

- [ ] **Step 6: Run all CLI tests for regressions**

Run: `cargo test --test cli -- --test-threads=1`

Expected: all CLI tests pass.

- [ ] **Step 7: Commit seed lifecycle behavior**

```bash
git add src/instance.rs src/main.rs tests/cli.rs
git commit -m "fix: invalidate seeds after resource recreation"
```

---

### Task 3: Resource-aware doctor diagnostics

**Files:**
- Modify: `src/doctor.rs`
- Modify: `tests/cli.rs`

**Interfaces:**
- Consumes: `resource::observe`, `resource::logs`, and `resource::NOFILE_LIMIT` from Task 1.
- Produces: doctor verdicts that separate healthy managed, old-limit managed, external, stopped, and Docker-unavailable states.

- [ ] **Step 1: Add failing doctor integration tests**

Using the same per-test fake Docker adapter, add cases asserting:

```rust
assert!(healthy.contains("mongo"));
assert!(healthy.contains("mongo-gen")); // displayed short ID
assert!(healthy.contains("nofile=64000:64000"));

assert!(old_limit.contains("warn"));
assert!(old_limit.contains("expected 64000:64000"));
assert!(old_limit.contains("observed 1024:1024"));

assert!(stopped.contains("FAIL"));
assert!(stopped.contains("exit 133"));
assert!(stopped.contains("Too many open files"));
assert!(stopped.contains("preserve"));
assert!(stopped.contains("recreate"));
```

Also prove that an answering external resource passes when fake Docker exits with a daemon error, while a non-answering image-backed resource fails because Docker cannot start it.

- [ ] **Step 2: Run doctor tests and observe RED**

Run: `cargo test --test cli doctor_ -- --nocapture`

Expected: doctor only reports port reachability and lacks container diagnostics.

- [ ] **Step 3: Implement doctor's resource verdict policy**

Replace the current port-only branch with one observation per resource. Use a private helper:

```rust
fn check_resource(declared: &Resource, observed: Observation) -> Verdict
```

Healthy managed output includes the short ID and `nofile=<soft>:<hard>`. A reachable managed container with any other limit is `Warn` and includes safe data-preserving recreation guidance. A reachable resource with no observable exact-name container is `Ok` and described as external/unobserved.

When the port is closed and a managed container exists, fetch the last ten log lines and return `Fail`; include running/stopped state, exit code, ID, log tail, and the fact that the existing name blocks `grove up`. When Docker itself is unavailable and Grove would need it to start an image, return `Fail`. Preserve the existing absent-image failure.

- [ ] **Step 4: Run doctor tests and observe GREEN**

Run: `cargo test --test cli doctor_ -- --nocapture`

Expected: all doctor state cases pass.

- [ ] **Step 5: Commit doctor diagnostics**

```bash
git add src/doctor.rs tests/cli.rs
git commit -m "feat: diagnose managed resource containers"
```

---

### Task 4: Settings-isolation documentation

**Files:**
- Modify: `src/main.rs`
- Modify: `src/llm.rs`
- Modify: `README.md`
- Modify: `skills/grove/SKILL.md`
- Modify: `tests/cli.rs`
- Modify: `tests/config.rs`

**Interfaces:**
- Produces: consistent user and agent guidance; no runtime behavior change.

- [ ] **Step 1: Add failing documentation contract tests**

Extend the CLI help test to assert the `run` summary contains `overlay`. Extend the LLM reference test to require `dotenv`, `disable`, and `process variables`. Extend the installed-skill test to require the same limitation in the compiled skill.

- [ ] **Step 2: Run documentation tests and observe RED**

Run: `cargo test --test cli help_names -- --nocapture && cargo test --test config llm_reference -- --nocapture`

Expected: help and reference assertions fail because the limitation is not yet stated.

- [ ] **Step 3: Write concise consistent documentation**

Change the help summary to:

```rust
/// Run a command with this instance's environment overlaid
```

Add this application-responsibility rule to `src/llm.rs`, `README.md`, and `skills/grove/SKILL.md`:

```text
Grove renders configured dotenv files and overlays per-instance variables on commands it
starts; it does not provide an empty settings environment. Tests that assert application
defaults must disable dotenv loading and clear the relevant process variables in the
repo's own fixture or settings constructor.
```

Keep the wording close enough for the documentation contract tests without claiming a
language- or framework-specific mechanism.

- [ ] **Step 4: Run documentation tests and observe GREEN**

Run: `cargo test --test cli help_names -- --nocapture && cargo test --test cli skill_install -- --nocapture && cargo test --test config llm_reference -- --nocapture`

Expected: all focused documentation tests pass.

- [ ] **Step 5: Commit documentation**

```bash
git add src/main.rs src/llm.rs README.md skills/grove/SKILL.md tests/cli.rs tests/config.rs
git commit -m "docs: explain settings test isolation"
```

---

### Task 5: Final simplification and verification

**Files:**
- Modify only files already touched if simplification exposes duplication or unclear names.

**Interfaces:**
- Consumes all earlier tasks.
- Produces a clean, release-ready Grove tree without publishing a version.

- [ ] **Step 1: Format and inspect the focused diff**

Run: `cargo fmt && git diff --check && git diff --stat && git status --short`

Expected: no whitespace errors; only planned files are modified.

- [ ] **Step 2: Run focused tests serially**

Run: `cargo test --test resource && cargo test --test cli seed_ -- --test-threads=1 && cargo test --test cli doctor_ -- --test-threads=1 && cargo test --test config`

Expected: all focused tests pass.

- [ ] **Step 3: Run the full repository gate**

Run: `just check`

Expected: formatting, clippy with warnings denied, all tests, and packaging checks pass.

- [ ] **Step 4: Inspect actual installed-style output from the built binary**

Run: `cargo run --quiet -- --help | rg 'overlay|nofile' && cargo run --quiet -- --llm | rg 'dotenv|process variables|nofile'`

Expected: help and LLM reference expose both the existing resource limit and the new settings limitation.

- [ ] **Step 5: Commit any final mechanical cleanup if needed**

If formatting or a behavior-preserving simplification changed tracked files:

```bash
git add src tests README.md skills/grove/SKILL.md
git commit -m "refactor: simplify resource lifecycle handling"
```

If no files changed, do not create an empty commit.
