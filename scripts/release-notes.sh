#!/usr/bin/env bash
# Group the commits between two tags by conventional-commit type.
set -uo pipefail
tag="$1"
prev="$(git describe --tags --abbrev=0 "${tag}^" 2>/dev/null || true)"
range="${prev:+$prev..}${tag}"

emit() {
  local body
  body="$(git log --no-merges --pretty=format:'%s' "$range" \
    | sed -n "s/^$1\(([^)]*)\)\?!\?: //p" \
    | sed 's/^/- /')"
  if [ -n "$body" ]; then
    printf '### %s\n%s\n\n' "$2" "$body"
  fi
}

emit feat     "Added"
emit fix      "Fixed"
emit perf     "Faster"
emit refactor "Changed"
emit docs     "Documentation"
emit test     "Tests"

# Catch-all, so a type nobody anticipated is never silently dropped from a changelog.
# Only the release commit itself is excluded, since it carries no information.
other="$(git log --no-merges --pretty=format:'%s' "$range" \
  | grep -vE '^(feat|fix|perf|refactor|docs|test)(\([^)]*\))?!?: ' \
  | grep -vE '^chore(\([^)]*\))?!?: release ' \
  | sed 's/^/- /')"
if [ -n "$other" ]; then
  printf '### Other\n%s\n\n' "$other"
fi

if [ -n "$prev" ]; then
  printf '**Full changelog:** %s...%s\n' "$prev" "$tag"
fi
