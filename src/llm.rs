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
CORS_ORIGINS = "http://{{ host.public }}:{{ port.frontend }}"
MONGODB_URI = "mongodb://localhost:27017/?directConnection=true&replicaSet=rs0"
MONGODB_DATABASE = "{{ db.name }}"
ENVIRONMENT = "development"
DEBUG = "True"
AUTH_REDIRECT_URI = "http://{{ host.public }}:{{ port.frontend }}/auth/callback"

[[secrets]]
from = "frontend/.env.local"
into = "frontend/.env.local"

# Both pointers must be set. VITE_API_URL is what the browser fetches; VITE_PROXY_TARGET
# is what the dev server proxies /auth and /api to. Each falls back to a hardcoded
# localhost:8000, so missing either one silently drives another instance's backend.
[secrets.set]
VITE_API_URL = "http://{{ host.public }}:{{ port.backend }}"
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
command = "uv run uvicorn src.main:app --reload --host {{ host.bind }} --port {{ port.backend }}"
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
  {{{{ host.public }}}}    localhost normally; the selected LAN host when exposed
  {{{{ host.bind }}}}      127.0.0.1 normally; 0.0.0.0 when exposed

### Schema

  version            required, currently 1

  [ports]
  names              list of port names this repo needs

  [[secrets]]        repeatable; one per env file
  from               path relative to the MAIN worktree -- where real secrets live
  into               path relative to THIS worktree -- where they are written
  [secrets.set]      keys to override after copying; values are templates.
                     These are also exported into the environment of every command Grove
                     runs -- services, seeds, setup, prepare, and `grove run` -- because
                     code that reads the environment before its settings library loads
                     the file would otherwise see the pre-instance value.
                     Overrides, not secrets: .grove.toml is committed, so a real
                     credential written here is already in git. Real secrets belong in
                     the gitignored file named by `from`, which Grove copies verbatim
                     and never puts in the environment.

Grove renders configured dotenv files and overlays per-instance variables on commands it
starts; it does not provide an empty settings environment. Tests that assert application
defaults must disable dotenv loading and clear the relevant process variables in the
repo's own fixture or settings constructor.

When a repository-specific test switch tells integration tests to use Grove's managed
dependency instead of starting their own container, declare it in the corresponding
[secrets.set]. Grove renders it into the dotenv target and `grove run` exports it to the
command, without needing to know the repository's variable names.

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
                     instance gets back to a known shape. Grove records each managed
                     container incarnation and re-runs seeds after resource recreation.
  name               identifier, also the marker and log file name
  cwd                working directory relative to the worktree root
  command            run once per instance, after dependencies, before services
  if_exists          skip unless this path exists, relative to cwd -- for a fixture
                     that may not have been fetched

  [[service]]        repeatable; a long-running process
  name               identifier, also the log file name
  cwd                working directory relative to the worktree root
  setup              run ONCE per worktree before first start (uv sync, npm install)
  prepare            run on EVERY up, before this service starts and after the services
                     declared above it are answering -- for generated code that must
                     track what it was generated from, e.g. a typed client built from
                     this worktree's own backend. It runs even when the service is
                     already up, which is the case where stale output survives unnoticed.
  command            the process to run; must accept its port, usually via a flag
  ready.http         URL polled until it answers
  ready.timeout      how long to wait, e.g. "180s"

Three fields run commands, and the difference between them is how often:
`setup` once per worktree, `[[seed]]` once per instance, `prepare` every `up`. Put a
dependency install in `setup`, fixture data in `[[seed]]`, and code generation in
`prepare` -- generated output is the one that goes stale when it is only made once.

### Two rules that prevent silent cross-talk

1. Bind the port explicitly and strictly. A dev server that falls back to "next free
   port" on collision will start fine and then fail to match the CORS origin the backend
   was configured with. Pass the equivalent of Vite's --strictPort.

2. Rewrite every pointer between services, not just the obvious one. A frontend often
   holds two independent addresses for its backend -- one the browser uses and one the
   dev-server proxy uses -- and each usually has a hardcoded fallback. Missing one is
   invisible until an instance answers with another instance's data.

### Exposing an instance to the local network

Exposure is opt-in in both the command and the repository config. Use
`grove up --expose` to select the default-route IPv4 address, or
`grove up --expose-host <IPv4-or-hostname>` to choose the browser-visible host explicitly
for a VPN, Tailscale, or multi-interface machine. The explicit host implies exposure.
A plain `grove up` returns the instance to localhost-only operation.

Grove supplies the values; the config decides where they belong. A remotely usable
stack normally needs all three:

1. The service command binds with `{{{{ host.bind }}}}` so exposure can switch it from
   127.0.0.1 to all interfaces.
2. Values consumed by a browser use `{{{{ host.public }}}}`, not localhost on the
   viewer's machine.
3. Server-side CORS and redirect allowlists use the same public frontend origin.

Changing exposure re-renders config and restarts the instance. The selected state is
shown by `grove status` and `grove ls`; sibling instances remain unchanged. Exposure is
not a firewall, TLS, authentication, or tunnelling feature. Development services and any
authentication bypass are reachable from other machines that can contact the host, so
use it only on a network whose reachability you understand.

### Browser failures

Grove owns application readiness; agent-browser owns its browser daemon and CDP channel.
Run `grove status` first: it probes each service's `ready.http` and separates a live
process from a served endpoint. A service reported `running` but `NOT ANSWERING` is
Grove's problem, not the browser's -- read `grove logs <service> --since-restart`, and
check `grove doctor` for a datastore that has died underneath it. Only when the service
is answering does a refused, closed, or unresponsive browser channel point at
agent-browser: start its own recovery with `agent-browser doctor --fix`, then reopen the
isolated session if needed.

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
