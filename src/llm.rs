//! Agent-facing schema reference, emitted by `grove --llm`.
//!
//! The examples here are parsed by the test suite against the same structs that read a
//! real `.grove.toml`, so this file cannot document a config grove would reject.

/// A worked `.grove.toml` for a Vite + FastAPI + MongoDB repo.
pub const EXAMPLE: &str = r#"version = 1

# Port names this repo needs. grove assigns the numbers and exposes them to every
# template below as {{ port.<name> }}.
[ports]
names = ["frontend", "backend"]

# Each [[secrets]] block copies one env file from the MAIN checkout into this worktree,
# then overrides the listed keys. The main checkout is the source because a worktree
# never inherits gitignored files -- that is the whole point.
[[secrets]]
from = "backend/.env.local"
into = "backend/.env.local"

# CORS_ORIGINS is an exact-match list, so it has to name THIS instance's frontend
# port. Any auth callback URL needs the same treatment, for the same reason.
[secrets.set]
CORS_ORIGINS = "http://localhost:{{ port.frontend }}"
MONGODB_URI = "mongodb://localhost:27017/?directConnection=true&replicaSet=rs0"
MONGODB_DATABASE = "{{ db.name }}"
ENVIRONMENT = "development"
DEBUG = "True"
AUTH_REDIRECT_URI = "http://localhost:{{ port.frontend }}/auth/callback"

[[secrets]]
from = "frontend/.env.local"
into = "frontend/.env.local"

# Both pointers must be set. VITE_API_URL is what the browser fetches; VITE_PROXY_TARGET
# is what the dev server proxies /auth and /api to. Each falls back to a hardcoded
# localhost:8000, so missing either one silently drives another instance's backend.
[secrets.set]
VITE_API_URL = "http://localhost:{{ port.backend }}"
VITE_PROXY_TARGET = "http://localhost:{{ port.backend }}"

# One container shared by every instance. grove probes the port first and reuses
# whatever already answers, so a Mongo from Docker, from a VM, or forwarded over SSH all
# work without changing this block. Instances are isolated by database name.
# Containers grove starts use --ulimit nofile=64000:64000. `args` below belong to
# mongod and follow the image; they are not Docker flags.
[[resource]]
name = "mongo"
kind = "docker-shared"
image = "mongo:8.0.23"
args = ["--replSet", "rs0"]
port = 27017
init = "rs.initiate()"
db_name = "app_{{ slug }}"

[[service]]
name = "backend"
cwd = "backend"
setup = "uv sync"
command = "uv run uvicorn src.main:app --reload --port {{ port.backend }}"
ready = { http = "http://localhost:{{ port.backend }}/openapi.json", timeout = "180s" }

# --strictPort matters: without it Vite drifts to the next free port on collision, and
# the backend's CORS_ORIGINS -- an exact-match list -- stops matching.
[[service]]
name = "frontend"
cwd = "frontend"
setup = "npm install"
command = "npm run dev -- --port {{ port.frontend }} --strictPort"
ready = { http = "http://localhost:{{ port.frontend }}/", timeout = "180s" }

# A fresh per-instance database is empty, so any route that looks up a tenant or
# account answers 403 or 404 -- and the error names authentication, not missing data,
# which sends you looking in the wrong place. Seed it here once, rather than in every
# agent's prompt. `grove seed --force` re-runs this to reset a dirtied instance.
[[seed]]
name = "account"
cwd = "backend"
command = "uv run python -m tests.seed"
"#;

pub fn reference() -> String {
    format!(
        r#"# grove

Each git worktree gets its own running instance of the repo: its own ports, its own env
files, its own database. Agents work in parallel without colliding and without copying
configuration by hand.

## Authoring .grove.toml

Write it at the worktree root and commit it. It is checked in, so every later agent in
every worktree just runs `grove up`.

### Template variables

Any string value in [secrets.set], [[resource]].db_name, [[service]].command, and
[[service]].ready.http is a template. Available:

  {{{{ slug }}}}           this instance's name, [a-z0-9_], from the worktree directory
  {{{{ port.<name> }}}}    a port from [ports].names, assigned by grove
  {{{{ db.name }}}}        this instance's database, from [[resource]].db_name
  {{{{ main_worktree }}}}  absolute path of the main checkout

### Schema

  version            required, currently 1

  [ports]
  names              list of port names this repo needs

  [[secrets]]        repeatable; one per env file
  from               path relative to the MAIN worktree -- where real secrets live
  into               path relative to THIS worktree -- where they are written
  [secrets.set]      keys to override after copying; values are templates.
                     These are also exported into every service's environment, because
                     code that reads the environment before its settings library loads
                     the file would otherwise see the pre-instance value.

  [[resource]]       repeatable; a datastore shared across instances
  name               identifier
  kind               "docker-shared"; grove-started containers use
                     --ulimit nofile=64000:64000
  image              container image, used only if nothing answers on `port` already
  args               extra arguments to the container command, after the image
  port               port to probe, and to publish if grove starts it
  init               one-time command against a freshly started resource
  db_name            per-instance database name; a template

Grove reuses anything already answering on a resource's port and does not alter its
launch configuration. To adopt the fixed limit in an existing container, preserve any
needed data, then deliberately remove and recreate that container.

  [[seed]]           repeatable; data the instance needs before it is useful.
                     `grove seed --force` re-runs them all, which is how a dirtied
                     instance gets back to a known shape.
  name               identifier, also the marker and log file name
  cwd                working directory relative to the worktree root
  command            run once per instance, after dependencies, before services
  if_exists          skip unless this path exists, relative to cwd -- for a fixture
                     that may not have been fetched

  [[service]]        repeatable; a long-running process
  name               identifier, also the log file name
  cwd                working directory relative to the worktree root
  setup              run once per worktree before first start (uv sync, npm install)
  command            the process to run; must accept its port, usually via a flag
  ready.http         URL polled until it answers
  ready.timeout      how long to wait, e.g. "180s"

### Two rules that prevent silent cross-talk

1. Bind the port explicitly and strictly. A dev server that falls back to "next free
   port" on collision will start fine and then fail to match the CORS origin the backend
   was configured with. Pass the equivalent of Vite's --strictPort.

2. Rewrite every pointer between services, not just the obvious one. A frontend often
   holds two independent addresses for its backend -- one the browser uses and one the
   dev-server proxy uses -- and each usually has a hardcoded fallback. Missing one is
   invisible until an instance answers with another instance's data.

## Worked example: Vite + FastAPI + MongoDB

{}

## Postgres and containerised stacks

Not yet supported. Repos whose ports are baked into a compose file rather than passed on
the command line, that need a database created per instance, or that run several
repositories as one unit, need a grove newer than this one. Run `grove --version` and
check the project for a release that lists them.
"#,
        EXAMPLE
    )
}
