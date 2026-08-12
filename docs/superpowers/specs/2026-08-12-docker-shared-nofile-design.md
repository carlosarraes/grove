# Docker-shared open-file limit

## Problem

Grove starts a missing `docker-shared` resource with Docker's inherited open-file limit.
On the Mac, multiple Mongo containers created without an explicit limit exited with code
133 after integration suites repeatedly created collections and indexes. MongoDB
recommends a soft and hard `nofile` limit of 64,000.

Repository and live-machine diagnosis also confirmed that a proposed
`grove run --ports-only` flag would not solve the reported settings-isolation failure.
Rendered values reach a command through both its process environment and its on-disk env
file. Removing only the former leaves the latter active when the command runs from the
backend directory.

## Design

Whenever Grove starts a `docker-shared` container, `resource::decide` will include:

```text
--ulimit nofile=64000:64000
```

The flag is a fixed safety default for all containers Grove creates. It appears before the
image, with Grove's other Docker arguments. `resource.args` remain after the image because
they belong to the container command.

There is no new `.grove.toml` field and no Mongo image-name detection. A configurable
limit would add schema surface without a demonstrated second requirement; image-name
detection would make resource behavior implicit and brittle.

## Existing containers

The change affects only future `docker run` commands. Grove will continue to reuse
anything already answering on the declared port and will never delete or recreate a
container automatically. Existing Grove-created containers therefore need deliberate
recreation before receiving the new limit. User-facing guidance must warn that removing a
container can discard databases and that needed data should be preserved first.

## User-facing explanations

- `grove --help` will describe `docker-shared` resources as receiving Grove's fixed
  64,000 open-file safety limit when Grove starts them.
- `grove --llm` will briefly explain the fixed limit, that Docker flags precede the image
  while `args` follow it, and that an existing container must be deliberately recreated
  to adopt a changed launch configuration.
- The README will carry the same operational caveat for human readers.

## Testing

Development follows red-green-refactor:

1. Extend the direct `resource::decide` test to require the exact `--ulimit` pair before
   the image while retaining the existing assertion that container arguments follow it.
2. Add CLI/reference assertions that both `--help` and `--llm` mention the fixed limit
   and that the documented example still parses.
3. Implement the smallest argv and wording changes that make those tests pass.
4. Run the focused tests, then the complete `just check` verification.

## Non-goals

- No `grove run --ports-only` flag. The diagnosed settings tests must isolate both process
  variables and dotenv loading in their owning repository.
- No automatic container replacement or data migration.
- No `doctor` dependency on Docker inspection; Grove keeps its existing port-first model
  so externally supplied and forwarded resources remain valid.
- No change to the `grove ls` skill trigger in this work. The available reports predate
  v0.1.10, so that wording still needs a genuinely fresh-agent verification.
