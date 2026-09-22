#!/usr/bin/env bash
# Verifies a release tag matches Cargo.toml's version and that CHANGELOG.md
# has a matching, non-empty `## [X.Y.Z]` section, then prints that section
# on stdout as the release notes. Used by .github/workflows/release.yml's
# verify job, and runnable locally the same way.
#
# Usage:
#   scripts/release-notes.sh          # CI: reads the tag from GITHUB_REF_NAME (vX.Y.Z)
#   scripts/release-notes.sh X.Y.Z    # local: check/extract for this version directly
set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ge 1 ]; then
  version=$1
else
  tag=${GITHUB_REF_NAME:-}
  if [ -z "$tag" ]; then
    echo "release-notes: no VERSION given and GITHUB_REF_NAME is unset" >&2
    exit 64
  fi
  case "$tag" in
    v*) version=${tag#v} ;;
    *)
      echo "release-notes: tag '$tag' does not start with v" >&2
      exit 1
      ;;
  esac

  # Only the [package] section's own version starts a line with "version = "
  # (a dependency's version is always an inline table value, never at the
  # start of a line).
  cargo_version=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
  if [ "$version" != "$cargo_version" ]; then
    echo "release-notes: tag v$version does not match Cargo.toml's version $cargo_version" >&2
    exit 1
  fi
fi

ver_re=$(printf '%s' "$version" | sed 's/\./\\./g')

set +e
notes=$(awk -v ver_re="$ver_re" '
  /^## \[/ {
    if (insection) exit
    if ($0 ~ ("^## \\[" ver_re "\\]")) { insection = 1; found = 1 }
    next
  }
  insection { buf[++n] = $0 }
  END {
    if (!found) exit 3
    start = 1; end = n
    while (start <= end && buf[start] ~ /^[ \t]*$/) start++
    while (end >= start && buf[end] ~ /^[ \t]*$/) end--
    if (start > end) exit 4
    for (i = start; i <= end; i++) print buf[i]
  }
' CHANGELOG.md)
status=$?
set -e

case "$status" in
  0) ;;
  3)
    echo "release-notes: no '## [$version]' section in CHANGELOG.md" >&2
    exit 1
    ;;
  4)
    echo "release-notes: '## [$version]' section in CHANGELOG.md is empty" >&2
    exit 1
    ;;
  *)
    echo "release-notes: failed to extract '## [$version]' section (awk exit $status)" >&2
    exit 1
    ;;
esac

printf '%s\n' "$notes"
