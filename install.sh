#!/usr/bin/env sh

set -eu

repository="${GROVE_REPOSITORY:-carlosarraes/grove}"
version="${GROVE_VERSION:-latest}"
destination="${1:-${GROVE_INSTALL_PATH:-${HOME}/.local/bin/grove}}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  *)
    echo "grove does not publish a binary for $(uname -s) $(uname -m) yet." >&2
    exit 1
    ;;
esac

asset="grove-${target}.tar.gz"
if [ "$version" = "latest" ]; then
  base="https://github.com/${repository}/releases/latest/download"
else
  base="https://github.com/${repository}/releases/download/${version}"
fi

temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

echo "Downloading grove ${version} for ${target}..."
curl -fsSL "${base}/${asset}" -o "${temporary}/${asset}"
curl -fsSL "${base}/${asset}.sha256" -o "${temporary}/${asset}.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$temporary" && sha256sum -c "${asset}.sha256")
elif command -v shasum >/dev/null 2>&1; then
  (cd "$temporary" && shasum -a 256 -c "${asset}.sha256")
else
  echo "grove requires sha256sum or shasum to verify its download." >&2
  exit 1
fi

tar -xzf "${temporary}/${asset}" -C "$temporary" grove
mkdir -p "$(dirname "$destination")"
install -m 0755 "${temporary}/grove" "$destination"

echo "Installed $("$destination" --version) at $destination"

# grove was called treeish until 0.1.2. Leaving the old binary on PATH means agents and
# muscle memory keep reaching a version that no longer gets fixes.
former="$(dirname "$destination")/treeish"
if [ -e "$former" ]; then
  echo
  echo "Found the old treeish binary at $former."
  # `curl | sh` leaves stdin pointed at the pipe, so ask the terminal directly. Probe by
  # opening it: `[ -r /dev/tty ]` reports readable in sandboxes where the open still fails.
  # The probe runs in a subshell: dash exits the whole script on a failed redirection,
  # even inside an `if` condition, so a subshell is what keeps that contained.
  reply=""
  if ( exec < /dev/tty ) 2>/dev/null; then
    printf 'Remove it? [y/N]: '
    read -r reply < /dev/tty || reply=""
  fi

  case "$reply" in
    y|Y|yes|YES)
      rm -f "$former"
      echo "Removed $former"
      ;;
    "")
      echo "Not removed — no terminal to ask on. Remove it with: rm $former"
      ;;
    *)
      echo "Left in place. Remove it later with: rm $former"
      ;;
  esac
fi
case ":${PATH}:" in
  *":$(dirname "$destination"):"*) ;;
  *)
    echo "Note: add $(dirname "$destination") to your PATH."
    ;;
esac

cat <<EOF

Next:
  1. Teach agents about grove: $destination skill install
  2. In a repo without a config:  $destination --llm
  3. In a worktree:               $destination doctor && $destination up
EOF
