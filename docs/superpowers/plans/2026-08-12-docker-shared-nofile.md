# Docker-shared Open-file Limit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every container Grove starts for a `docker-shared` resource a fixed `nofile=64000:64000` limit and explain that behavior briefly in `--help`, `--llm`, and the README.

**Architecture:** Keep the `.grove.toml` schema unchanged. Extend the Docker argv assembled by `resource::decide`, preserving the existing boundary between Docker flags before the image and container-command arguments after it; update generated and human-facing documentation without adding Docker inspection or automatic container replacement.

**Tech Stack:** Rust 2024, clap derive, Rust integration tests, Markdown, Just.

## Global Constraints

- Apply `--ulimit nofile=64000:64000` to every future `docker run` Grove builds for a `docker-shared` resource.
- Keep `resource.args` after the image; the new Docker flag must precede the image.
- Add no `.grove.toml` field and perform no image-name detection.
- Never delete, replace, or migrate an existing container automatically.
- Explain briefly in `grove --help` and `grove --llm` that the limit applies only when Grove starts the container.
- Warn in `grove --llm` and the README that existing containers require deliberate recreation after preserving needed data.
- Do not add `grove run --ports-only`, Docker inspection to `doctor`, or new `grove ls` skill wording.

---

### Task 1: Add the Docker open-file limit

**Files:**
- Modify: `tests/resource.rs:71-88`
- Modify: `src/resource.rs:32-45`

**Interfaces:**
- Consumes: `resource::decide(resource: &Resource) -> Decision`
- Produces: `Decision::Start(Vec<String>)` containing the adjacent arguments `--ulimit`, `nofile=64000:64000` before the image.

- [ ] **Step 1: Write the failing argv-order test**

Extend `container_arguments_come_after_the_image` in `tests/resource.rs` so it proves the Docker flag/value pair exists and precedes the image while the container argument remains after it:

```rust
#[test]
fn docker_flags_precede_the_image_and_container_arguments_follow_it() {
    let Decision::Start(argv) = resource::decide(&mongo(a_free_port())) else {
        panic!("should start");
    };

    let image = argv
        .iter()
        .position(|a| a == "mongo:8.0.23")
        .expect("image");
    let repl = argv.iter().position(|a| a == "--replSet").expect("replSet");
    let publish = argv.iter().position(|a| a == "-p").expect("publish");
    let ulimit = argv
        .iter()
        .position(|a| a == "--ulimit")
        .expect("ulimit");

    assert_eq!(
        argv.get(ulimit + 1).map(String::as_str),
        Some("nofile=64000:64000"),
        "{argv:?}"
    );
    assert!(publish < image, "{argv:?}");
    assert!(ulimit < image, "{argv:?}");
    assert!(image < repl, "{argv:?}");
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test --test resource docker_flags_precede_the_image_and_container_arguments_follow_it -- --exact
```

Expected: FAIL because `.expect("ulimit")` cannot find `--ulimit` in the argv.

- [ ] **Step 3: Add the minimal Docker arguments**

In `src/resource.rs`, add the fixed pair among the Docker-owned arguments in the initial vector:

```rust
let mut argv = vec![
    "run".to_string(),
    "-d".to_string(),
    "--ulimit".to_string(),
    "nofile=64000:64000".to_string(),
    "--name".to_string(),
    container_name(resource),
    "-p".to_string(),
    format!("{p}:{p}", p = resource.port),
];
```

Leave image insertion and `argv.extend(resource.args.iter().cloned())` unchanged.

- [ ] **Step 4: Run the resource tests and verify GREEN**

Run:

```bash
cargo test --test resource
```

Expected: all resource tests PASS with no warnings.

- [ ] **Step 5: Commit the runtime behavior**

```bash
git add src/resource.rs tests/resource.rs
git commit -m "fix: raise shared container open-file limit"
```

### Task 2: Explain the fixed limit and migration boundary

**Files:**
- Modify: `tests/cli.rs` near the first CLI behavior tests
- Modify: `tests/config.rs` after `the_documented_example_parses`
- Modify: `src/main.rs:105-115`
- Modify: `src/llm.rs:42-52,118-125`
- Modify: `README.md:35-37,80-85,100-107`

**Interfaces:**
- Consumes: clap's generated `grove --help` text and `llm::reference() -> String`.
- Produces: user-visible text containing `nofile=64000`, plus LLM guidance that `args` follow the image and existing containers require deliberate recreation after data preservation.

- [ ] **Step 1: Write the failing help test**

Add this test near the start of `tests/cli.rs`; it needs no repository fixture:

```rust
#[test]
fn help_names_the_shared_container_open_file_limit() {
    let out = Command::cargo_bin("grove")
        .expect("binary")
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&out);

    assert!(stdout.contains("nofile=64000"), "{stdout}");
}
```

- [ ] **Step 2: Write the failing LLM-reference test**

Add this test to `tests/config.rs` after `the_documented_example_parses`:

```rust
#[test]
fn llm_reference_explains_the_container_limit_and_existing_container_boundary() {
    let reference = llm::reference();

    assert!(reference.contains("nofile=64000:64000"), "{reference}");
    assert!(reference.contains("after the image"), "{reference}");
    assert!(reference.contains("preserve"), "{reference}");
    assert!(reference.contains("recreate"), "{reference}");
}
```

- [ ] **Step 3: Run both focused tests and verify RED**

Run:

```bash
cargo test --test cli help_names_the_shared_container_open_file_limit -- --exact
cargo test --test config llm_reference_explains_the_container_limit_and_existing_container_boundary -- --exact
```

Expected: both FAIL because the current output does not mention `nofile=64000` or the existing-container boundary.

- [ ] **Step 4: Update the clap help text**

Change the `Up` doc comment in `src/main.rs` to:

```rust
/// Render config, start missing shared containers with nofile=64000, start services
Up {
```

This line appears in both `grove --help` and `grove up --help` through clap derive.

- [ ] **Step 5: Update the generated LLM reference**

In the worked example's `[[resource]]` comment in `src/llm.rs`, add:

```text
# Containers grove starts use --ulimit nofile=64000:64000. `args` below belong to
# mongod and follow the image; they are not Docker flags.
```

In the schema section, replace the `kind` and `args` descriptions with:

```text
  kind               "docker-shared"; grove-started containers use
                     --ulimit nofile=64000:64000
  image              container image, used only if nothing answers on `port` already
  args               extra arguments to the container command, after the image
```

After the `[[resource]]` field list, add this short operational paragraph:

```text
Grove reuses anything already answering on a resource's port and does not alter its
launch configuration. To adopt the fixed limit in an existing container, preserve any
needed data, then deliberately remove and recreate that container.
```

- [ ] **Step 6: Update the README**

Expand the Docker paragraph in `README.md` with the same behavior and safety boundary:

```markdown
Docker appears only for an optional shared datastore, and only if one isn't already
running. A container grove starts receives `nofile=64000:64000`; an existing container
keeps its original launch configuration. Preserve any needed data before deliberately
removing and recreating one to adopt the limit.
```

Add a brief comment above the README's `[[resource]]` example:

```toml
# Containers grove starts receive nofile=64000:64000.
```

Change the `up` command-table description to:

```markdown
| `up` | render config, start missing shared containers with `nofile=64000`, start services |
```

- [ ] **Step 7: Run focused documentation tests and verify GREEN**

Run:

```bash
cargo test --test cli help_names_the_shared_container_open_file_limit -- --exact
cargo test --test config
```

Expected: both commands PASS; the config test also proves the edited worked example still parses.

- [ ] **Step 8: Inspect the actual generated output**

Run:

```bash
cargo run --quiet -- --help
cargo run --quiet -- --llm
```

Expected: `--help` names `nofile=64000`; `--llm` contains the exact limit, argument-order explanation, and preserve-before-recreate warning without introducing a new schema field.

- [ ] **Step 9: Run full verification**

Run:

```bash
just check
```

Expected: formatting, clippy with warnings denied, all tests, and packaging checks PASS.

- [ ] **Step 10: Commit the explanations**

```bash
git add src/main.rs src/llm.rs README.md tests/cli.rs tests/config.rs
git commit -m "docs: explain shared container open-file limit"
```
