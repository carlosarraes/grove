# treeish development and release tasks.

binary := "treeish"
install_dir := env_var("HOME") / ".local/bin"

default: build

# Build an optimized binary and install it for the current user.
build:
    cargo build --release --locked
    mkdir -p {{install_dir}}
    install -m 0755 target/release/{{binary}} {{install_dir}}/{{binary}}
    @echo "Installed {{install_dir}}/{{binary}}"

# Run the same checks expected by CI.
check:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets --all-features
    sh tests/packaging.sh

# Cut a release: update Cargo's version, commit, tag, and push.
# Usage: just release 0.1.0
release version:
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
