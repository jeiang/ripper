# Agent instructions

ripper builds `rip`, a Linux trash CLI that follows the freedesktop.org Trash spec and handles
btrfs subvolumes and bind mounts (NixOS impermanence). Design: docs/design.md.

## Commands
- `just build`, `just test` (Linux: unit and bwrap sandbox tests), `just check` (clippy -D warnings, rustfmt), `just fmt`
- From macOS: `just artemis check` and `just artemis test`.

## Rules
- Linux only. clippy and tests count only when they ran on Linux (artemis or CI).
- Every test that runs `rip` runs it in the bwrap sandbox (tests/common). Never run put,
  restore, empty or purge against a real home, real trash or /mnt/Mumei while developing.
- Keep the invariants in docs/design.md §0. Never use std::fs::remove_dir_all or
  std::fs::rename in trash code. A change to a destructive path needs a test of its failure path.
- A behavior change updates docs/design.md in the same commit.
- A change to the command line (a subcommand, flag, value or parse rule) updates all three
  hand-written completions (completions/rip.fish, completions/rip.bash, completions/_rip) and
  tests/completions.rs in the same commit.
- Never run doas or sudo on artemis.
- Conventional Commits. Commit each working checkpoint; push main after `just check` and
  `just test` pass on Linux.

## Release
- Every user-facing change (behavior, command line, completions, package, config) adds one
  line under `## [Unreleased]` in CHANGELOG.md, in the same commit.
- From 1.0.0, every push to main that changes what users get is a release: patch for fixes,
  minor for features, major for breaking changes. Make it with `just release VERSION`, push
  main, wait for CI to go green, then `git push origin vX.Y.Z` (the tag triggers
  .github/workflows/release.yml).
- A push that touches only docs, tests or CI keeps its line(s) under `## [Unreleased]` and
  makes no release.
