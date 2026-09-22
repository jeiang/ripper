#!/usr/bin/env bash
# Pushes store paths to garret with retries and a per-attempt watchdog.
# Adapted from cornn-flaek's .ci/garret-push.sh, but strict: unlike that
# version's continue-on-error default, exhausted attempts always fail this
# job. .github/workflows/release.yml's release job does not depend on the
# garret job, so a cache outage turns this job red without blocking the
# GitHub release.
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: $0 <store-path>..." >&2
  exit 64
fi

export PATH=$HOME/.nix-profile/bin:$PATH # release.yml installs the client via `nix profile install`

config=${GARRET_CONFIG:-.ci/garret.toml}
timeout=${GARRET_PUSH_TIMEOUT:-1800}
attempts=3

push_with_watchdog() {
  garret --config "$config" push "$@" &
  local push_pid=$!
  (
    sleep "$timeout"
    echo "garret push exceeded ${timeout}s; killing hung push" >&2
    kill -TERM "$push_pid" 2>/dev/null
  ) &
  local watchdog_pid=$!
  local status=0
  wait "$push_pid" || status=$?
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  return "$status"
}

for attempt in $(seq 1 "$attempts"); do
  if push_with_watchdog "$@"; then
    exit 0
  fi
  if [ "$attempt" -lt "$attempts" ]; then
    echo "garret push failed (attempt $attempt/$attempts); retrying in 15s..." >&2
    sleep 15
  fi
done

echo "garret push failed after $attempts attempts" >&2
exit 1
