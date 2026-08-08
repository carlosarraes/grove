#!/usr/bin/env sh

set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

fail() {
  echo "packaging test failed: $*" >&2
  exit 1
}

test -f "$root/justfile" || fail "justfile is missing"
grep -Eq '^build:' "$root/justfile" || fail "justfile has no build recipe"
grep -Eq '^check:' "$root/justfile" || fail "justfile has no check recipe"
grep -Eq '^release version:' "$root/justfile" || fail "justfile has no release recipe"

# A release that bumped the version but checked the old tree would ship untested code.
version_update_line="$(grep -n 'awk -v version=' "$root/justfile" | cut -d: -f1)"
release_check_line="$(grep -n '^[[:space:]]*just check$' "$root/justfile" | cut -d: -f1)"
test "$version_update_line" -lt "$release_check_line" ||
  fail "release checks must run after updating the package version"

test -x "$root/install.sh" || fail "root install.sh is not executable"
grep -Fq 'install.sh' "$root/.github/workflows/release.yml" ||
  fail "release workflow does not publish install.sh"

# Every target the installer claims to handle must be one the release workflow builds.
for target in x86_64-unknown-linux-gnu aarch64-apple-darwin; do
  grep -Fq "$target" "$root/install.sh" ||
    fail "install.sh does not handle $target"
  grep -Fq "$target" "$root/.github/workflows/release.yml" ||
    fail "release workflow does not build $target"
done

temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

mkdir -p "$temporary/archive" "$temporary/assets" "$temporary/bin"
cat >"$temporary/bin/uname" <<'UNAME'
#!/usr/bin/env sh
case "$1" in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
  *) exit 1 ;;
esac
UNAME
chmod +x "$temporary/bin/uname"

cat >"$temporary/archive/grove" <<'BINARY'
#!/usr/bin/env sh
if test "${1:-}" = '--version'; then echo 'grove 9.9.9'; exit 0; fi
echo "fixture grove"
BINARY
chmod +x "$temporary/archive/grove"

asset="grove-x86_64-unknown-linux-gnu.tar.gz"
tar -C "$temporary/archive" -czf "$temporary/assets/$asset" grove
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$temporary/assets" && sha256sum "$asset" >"$asset.sha256")
else
  (cd "$temporary/assets" && shasum -a 256 "$asset" >"$asset.sha256")
fi

cat >"$temporary/bin/curl" <<'CURL'
#!/usr/bin/env sh
set -eu

destination=""
url=""
while test "$#" -gt 0; do
  case "$1" in
    -o)
      destination="$2"
      shift 2
      ;;
    -* ) shift ;;
    *)
      url="$1"
      shift
      ;;
  esac
done

test -n "$destination"
cp "$FIXTURE_ASSETS/${url##*/}" "$destination"
CURL
chmod +x "$temporary/bin/curl"

destination="$temporary/install/grove"
output="$(
  PATH="$temporary/bin:$PATH" \
    FIXTURE_ASSETS="$temporary/assets" \
    GROVE_VERSION="v0.1.0" \
    GROVE_INSTALL_PATH="$destination" \
    "$root/install.sh"
)"

test -x "$destination" || fail "installer did not install an executable"

# The old binary must be noticed, and left alone when the answer is no.
former="$temporary/install/treeish"
printf '#!/usr/bin/env sh\necho old\n' >"$former"
chmod +x "$former"
legacy_output="$(
  PATH="$temporary/bin:$PATH" \
    FIXTURE_ASSETS="$temporary/assets" \
    GROVE_VERSION="v0.1.0" \
    GROVE_INSTALL_PATH="$destination" \
    "$root/install.sh" </dev/null
)"
case "$legacy_output" in
  *"$former"*) ;;
  *) fail "installer did not mention the old treeish binary" ;;
esac
test -x "$former" || fail "installer removed the old binary without being told to"
rm -f "$former"

test "$("$destination")" = "fixture grove" || fail "installed the wrong binary"
case "$output" in
  *"grove 9.9.9"*) ;;
  *) fail "installer did not echo the installed version" ;;
esac

echo "packaging checks passed"
