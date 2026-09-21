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
- Keep the invariants in docs/design.md §0.3. Never use std::fs::remove_dir_all or
  std::fs::rename in trash code. A change to a destructive path needs a test of its failure path.
- A behavior change updates docs/design.md in the same commit.
- Never run doas or sudo on artemis.
- Conventional Commits. Commit each working checkpoint; push main after `just check` and
  `just test` pass on Linux.
