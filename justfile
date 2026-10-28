# grove development and release tasks.

binary := "grove"
install_dir := env_var("HOME") / ".local/bin"

default: build

# Carlos's Mac is an install-only host for this tool.
[private]
assert-build-host:
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(hostname -s)" in
        Carloss-MacBook-Air|Carloss-MacBook-Air.*)
            echo "error: do not compile grove on mac; run just sync mac mact" >&2
            exit 1;;
    esac
    if [[ -e "$HOME/.config/grove/no-build" ]]; then
        echo "error: compilation disabled on this host; run just sync mac mact" >&2
        exit 1
    fi

# Build an optimized binary and install it for the current user.
build: assert-build-host
    cargo build --release --locked
    mkdir -p {{install_dir}}
    install -m 0755 target/release/{{binary}} {{install_dir}}/{{binary}}
    @echo "Installed {{install_dir}}/{{binary}}"

# Build on a separate host, then atomically install on the destination.
[positional-arguments]
sync host="mac" builder="mact":
    #!/usr/bin/env bash
    set -euo pipefail
    sync_host="$1"
    build_host="$2"
    for host in "$sync_host" "$build_host"; do
        if [[ ! "$host" =~ ^[a-zA-Z0-9][a-zA-Z0-9._@-]*$ ]]; then
            echo "error: use an SSH host alias, hostname, or user@host" >&2
            exit 1
        fi
    done
    if [[ "$build_host" == mac ]]; then
        echo "error: mac is an install-only host; use mact as the builder" >&2
        exit 1
    fi
    ssh_options=(-o BatchMode=yes -o ControlPath=none -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2)
    builder_name=$(ssh "${ssh_options[@]}" "$build_host" 'hostname -s')
    case "$builder_name" in
        Carloss-MacBook-Air|Carloss-MacBook-Air.*)
            echo "error: Carlos's Mac is an install-only host; use mact as the builder" >&2
            exit 1;;
    esac
    builder_platform=$(ssh "${ssh_options[@]}" "$build_host" 'uname -sm')
    destination_platform=$(ssh "${ssh_options[@]}" "$sync_host" 'uname -sm')
    if [[ "$builder_platform" != "$destination_platform" ]]; then
        echo "error: builder $build_host is $builder_platform, destination $sync_host is $destination_platform; choose a matching builder" >&2
        exit 1
    fi
    stage=$(mktemp -d "${TMPDIR:-/tmp}/grove-sync.XXXXXX")
    pending=""
    builder_stage=""
    cleanup() {
        rm -rf "$stage"
        if [[ -n "$builder_stage" ]]; then
            ssh "${ssh_options[@]}" "$build_host" "rm -rf -- '$builder_stage'" || true
        fi
        if [[ -n "$pending" ]]; then
            ssh "${ssh_options[@]}" "$sync_host" "rm -f -- '$pending'" || true
        fi
    }
    trap cleanup EXIT
    candidate=$(ssh "${ssh_options[@]}" "$build_host" 'test ! -e "$HOME/.config/grove/no-build" || { echo "error: compilation disabled on this host" >&2; exit 1; }; mkdir -p ~/.cache/grove-build; mktemp -d "$HOME/.cache/grove-build/source.XXXXXX"')
    if [[ ! "$candidate" =~ ^/[a-zA-Z0-9_./-]+/\.cache/grove-build/source\.[a-zA-Z0-9]+$ ]]; then
        echo "error: invalid builder staging path" >&2
        exit 1
    fi
    builder_stage="$candidate"
    echo "Building grove on $build_host ($builder_platform); installing on $sync_host"
    rsync -az \
        --include='/Cargo.toml' --include='/Cargo.lock' --include='/build.rs' \
        --include='/rust-toolchain.toml' --include='/src/***' --include='/skills/***' \
        --exclude='*' -e 'ssh -o BatchMode=yes -o ControlPath=none -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2' ./ "$build_host:$builder_stage/"
    ssh "${ssh_options[@]}" "$build_host" "bash -s -- '$builder_stage'" <<'BUILD'
    set -euo pipefail
    test ! -e "$HOME/.config/grove/no-build" || { echo "error: compilation disabled on this host" >&2; exit 1; }
    export PATH="$HOME/.cargo/bin:$PATH"
    python3 - "$1" <<'PYBUILD'
    import fcntl, os, pathlib, shutil, subprocess, sys
    source = pathlib.Path(sys.argv[1])
    cache = pathlib.Path.home() / ".cache/grove-build"
    with (cache / "build.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        env = dict(os.environ, CARGO_TARGET_DIR=str(cache / "target"))
        result = subprocess.run(["cargo", "build", "--locked", "--release"], cwd=source, env=env)
        if result.returncode:
            sys.exit(result.returncode)
        shutil.copy2(cache / "target/release/grove", source / "grove")
    PYBUILD
    BUILD
    scp -q "${ssh_options[@]}" "$build_host:$builder_stage/grove" "$stage/grove"
    candidate=$(ssh "${ssh_options[@]}" "$sync_host" 'mkdir -p ~/.local/bin; mktemp "$HOME/.local/bin/.grove-sync.XXXXXX"')
    if [[ ! "$candidate" =~ ^/[a-zA-Z0-9_./-]+$ ]]; then
        echo "error: invalid destination staging path" >&2
        exit 1
    fi
    pending="$candidate"
    scp -q "${ssh_options[@]}" "$stage/grove" "$sync_host:$pending"
    ssh "${ssh_options[@]}" "$sync_host" "bash -s -- '$pending'" <<'SH'
    set -euo pipefail
    pending="$1"
    trap 'rm -f "$pending"' EXIT
    chmod 755 "$pending"
    "$pending" --version
    mv -f "$pending" "$HOME/.local/bin/grove"
    echo "Installed $HOME/.local/bin/grove"
    SH
    pending=""

# Run the same checks expected by CI.
check: assert-build-host
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets --all-features
    sh tests/packaging.sh

# Cut a release: update Cargo's version, commit, tag, and push.
# Usage: just release 0.1.0
release version: assert-build-host
    #!/usr/bin/env bash
    set -euo pipefail

    version="{{version}}"
    version="${version#v}"
    if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      echo "error: version must be semver like 0.1.0 (got '{{version}}')" >&2
      exit 1
    fi
    if [[ -n "$(git status --porcelain)" ]]; then
      echo "error: working tree is dirty; commit or stash first" >&2
      exit 1
    fi
    if git rev-parse "v$version" >/dev/null 2>&1; then
      echo "error: tag v$version already exists" >&2
      exit 1
    fi

    temporary="$(mktemp)"
    trap 'rm -f "$temporary"' EXIT
    awk -v version="$version" '
      /^\[package\]$/ { in_package = 1 }
      in_package && !updated && /^version = / {
        print "version = \"" version "\""
        updated = 1
        next
      }
      { print }
      END { if (!updated) exit 1 }
    ' Cargo.toml >"$temporary"
    mv "$temporary" Cargo.toml
    trap - EXIT

    just check
    git add Cargo.toml Cargo.lock
    if git diff --cached --quiet; then
      echo "Cargo.toml is already at $version; tagging the current commit"
    else
      git commit -m "chore: release v$version"
    fi

    git tag -a "v$version" -m "v$version"

    remote="${RELEASE_REMOTE:-}"
    if [[ -z "$remote" ]]; then
      branch="$(git branch --show-current)"
      remote="$(git config --get "branch.$branch.remote" || true)"
    fi
    if [[ -z "$remote" || "$remote" == "." ]]; then
      if git remote get-url origin >/dev/null 2>&1; then
        remote="origin"
      elif git remote get-url upstream >/dev/null 2>&1; then
        remote="upstream"
      else
        echo "error: no push remote found; set RELEASE_REMOTE" >&2
        exit 1
      fi
    fi

    git push "$remote" HEAD
    git push "$remote" "v$version"
    echo "Pushed v$version; GitHub Actions will build and publish the release."
