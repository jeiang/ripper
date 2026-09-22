#!/usr/bin/env bash
# `just release VERSION`: bump Cargo.toml/Cargo.lock, move CHANGELOG.md's
# Unreleased section into a dated release, commit `chore(release): vVERSION`,
# and tag it locally (AGENTS.md's release rules). Never pushes: pushing main
# and the tag afterwards is a separate, explicit step.
#
# Only POSIX text tools (awk, grep, git, date, mktemp) -- no cargo -- so this
# runs the same on macOS and Linux without needing the Rust toolchain.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ne 1 ]; then
  echo "usage: $0 X.Y.Z" >&2
  exit 64
fi
VERSION=$1

if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "release: VERSION must be X.Y.Z (got '$VERSION')" >&2
  exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "release: working tree is dirty; commit or stash first" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
  echo "release: tag v$VERSION already exists" >&2
  exit 1
fi

# The [package] section's own version is the only line starting with
# "version = ": every dependency version is an inline table value, e.g.
# `clap = { version = "4.6.7", ... }`, never at the start of a line.
current_version=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
IFS=. read -r v_major v_minor v_patch <<<"$VERSION"
IFS=. read -r c_major c_minor c_patch <<<"$current_version"

is_greater=0
if [ "$v_major" -gt "$c_major" ]; then
  is_greater=1
elif [ "$v_major" -eq "$c_major" ]; then
  if [ "$v_minor" -gt "$c_minor" ]; then
    is_greater=1
  elif [ "$v_minor" -eq "$c_minor" ] && [ "$v_patch" -gt "$c_patch" ]; then
    is_greater=1
  fi
fi
if [ "$is_greater" -ne 1 ]; then
  echo "release: $VERSION is not greater than the current version $current_version" >&2
  exit 1
fi

unreleased_body=$(awk '
  /^## \[Unreleased\]/ { state = 1; next }
  state == 1 && /^## \[/ { exit }
  state == 1 { print }
' CHANGELOG.md)
if [ -z "$(printf '%s' "$unreleased_body" | tr -d '[:space:]')" ]; then
  echo "release: CHANGELOG.md's Unreleased section is empty; nothing to release" >&2
  exit 1
fi

date=$(date -u +%Y-%m-%d)

tmp=$(mktemp)
awk -v new="$VERSION" '
  !done && /^version = "/ { print "version = \"" new "\""; done = 1; next }
  { print }
' Cargo.toml >"$tmp" && mv "$tmp" Cargo.toml

# Cargo.lock: only the ripper package's own [[package]] stanza (it has no
# "source"/"checksum" fields -- it is the workspace root, not a registry
# crate -- so a plain version-line replace cannot touch anything else).
tmp=$(mktemp)
awk -v new="$VERSION" '
  $0 == "name = \"ripper\"" { inripper = 1; print; next }
  inripper && /^version = "/ { print "version = \"" new "\""; inripper = 0; next }
  { print }
' Cargo.lock >"$tmp" && mv "$tmp" Cargo.lock

# CHANGELOG.md: move the Unreleased body into a new dated section right
# after it, leaving Unreleased's own heading with nothing under it.
tmp=$(mktemp)
awk -v ver="$VERSION" -v date="$date" '
  state == 0 {
    pre[++pn] = $0
    if ($0 ~ /^## \[Unreleased\]/) state = 1
    next
  }
  state == 1 && /^## \[/ { state = 2; post[++postn] = $0; next }
  state == 1 { body[++bn] = $0; next }
  { post[++postn] = $0 }
  END {
    bstart = 1; bend = bn
    while (bstart <= bend && body[bstart] ~ /^[ \t]*$/) bstart++
    while (bend >= bstart && body[bend] ~ /^[ \t]*$/) bend--
    for (i = 1; i <= pn; i++) print pre[i]
    print ""
    print "## [" ver "] - " date
    print ""
    for (i = bstart; i <= bend; i++) print body[i]
    if (postn > 0) {
      print ""
      for (i = 1; i <= postn; i++) print post[i]
    }
  }
' CHANGELOG.md >"$tmp" && mv "$tmp" CHANGELOG.md

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): v$VERSION

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
git tag -a "v$VERSION" -m "v$VERSION"

echo "release: committed and tagged v$VERSION locally."
echo "Next: push main, wait for CI to go green, then: git push origin v$VERSION"
