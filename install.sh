#!/usr/bin/env sh

set -eu

repository="${TREEISH_REPOSITORY:-carlosarraes/treeish}"
version="${TREEISH_VERSION:-latest}"
destination="${1:-${TREEISH_INSTALL_PATH:-${HOME}/.local/bin/treeish}}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  *)
    echo "treeish does not publish a binary for $(uname -s) $(uname -m) yet." >&2
    exit 1
    ;;
esac

asset="treeish-${target}.tar.gz"
if [ "$version" = "latest" ]; then
  base="https://github.com/${repository}/releases/latest/download"
else
  base="https://github.com/${repository}/releases/download/${version}"
fi

temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

echo "Downloading treeish ${version} for ${target}..."
curl -fsSL "${base}/${asset}" -o "${temporary}/${asset}"
curl -fsSL "${base}/${asset}.sha256" -o "${temporary}/${asset}.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$temporary" && sha256sum -c "${asset}.sha256")
elif command -v shasum >/dev/null 2>&1; then
  (cd "$temporary" && shasum -a 256 -c "${asset}.sha256")
else
  echo "treeish requires sha256sum or shasum to verify its download." >&2
  exit 1
fi

tar -xzf "${temporary}/${asset}" -C "$temporary" treeish
mkdir -p "$(dirname "$destination")"
install -m 0755 "${temporary}/treeish" "$destination"

echo "Installed $("$destination" --version) at $destination"
case ":${PATH}:" in
  *":$(dirname "$destination"):"*) ;;
  *)
    echo "Note: add $(dirname "$destination") to your PATH."
    ;;
esac

cat <<EOF

Next:
  1. Teach agents about treeish: $destination skill install
  2. In a repo without a config:  $destination --llm
  3. In a worktree:               $destination doctor && $destination up
EOF
