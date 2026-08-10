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

if [ -n "$prev" ]; then
  printf '**Full changelog:** %s...%s\n' "$prev" "$tag"
fi
