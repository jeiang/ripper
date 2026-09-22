# rip

`rip` moves files to the freedesktop.org Trash on Linux instead of deleting them.
It exists because plain `rm` is permanent, and because a straightforward trash
implementation gets NixOS impermanence setups wrong: a bind-mounted home
directory and btrfs subvolumes mean the file being trashed and `~/.local/share/Trash`
often are not on the same filesystem in the way a simple rename assumes.

`rip` handles that correctly:

- **Bind mounts.** It finds a mount that shows both the file and the trash's
  `files/` directory (there is almost always one on an impermanence layout,
  e.g. `/persist`) and renames through that, instead of assuming `~/Downloads`
  and `~/.local/share/Trash` share a mount just because they look like they do.
- **btrfs subvolumes.** Each subvolume has its own device number even on the
  same filesystem, so a plain rename across two subvolumes fails with `EXDEV`.
  `rip` detects this and only copies when a rename is genuinely impossible.
- **freedesktop.org Trash spec compatibility.** Items land in the standard
  home trash or a topdir trash (`$topdir/.Trash/$uid` or `$topdir/.Trash-$uid`)
  as the spec describes, readable by any other spec-compliant trash tool.

See `docs/design.md` for the full design reference (invariants, on-disk
layout, crash-consistency guarantees).

## Install

`rip` is packaged as a Nix flake, exposing `packages.x86_64-linux.default`
(also `aarch64-linux`). From a checkout of this repository:

```
nix profile install .#default
```

or, as an input to another flake, this repository's own flake reference. The
package wraps `rip`'s `PATH` with `coreutils` (for `cp`, used by the
cross-filesystem copy fallback) and `fzf` (used by the interactive picker),
so neither needs to be installed separately. It also installs the bash, zsh
and fish completions under `completions/`, all static, hand-written files,
so this works the same on a native or a cross build.

There is no non-Nix install path today; building from source needs the
dependencies listed in `Cargo.toml` and a Linux target.

## Usage

```
rip FILE...                    trash files (recursive; there is no non-recursive mode)
rip undo [-y]                  restore every item from the most recent rip invocation
rip list [-a|--all] [-0]       list trashed items; without -a, only ones under the cwd
rip restore [-a|--all|PATH...] [--rename] [-y]   restore chosen items, or pick with fzf
rip empty [--older-than DUR] [--max-size SIZE] [-y]   permanently delete (no filter: everything)
rip purge [-a|--all|PATH...] [-y]                permanently delete chosen items, or pick with fzf
rip --config PATH ...          use PATH instead of the default config file
rip --completions bash|zsh|fish   print a shell completion script
```

Examples:

```
rip foo.txt bar/                       # trash foo.txt and the directory bar
rip -v foo.txt                         # print what happened
rip list                               # trashed items whose original path is under here
rip restore                            # pick items to restore with fzf
rip restore ~/Downloads/report.pdf     # restore that specific item
rip undo                               # undo the last rip invocation
rip empty --older-than 30d             # delete everything trashed more than 30 days ago
rip empty --max-size 2G -y             # delete oldest items first until the rest fits in 2 GiB
rip purge                              # pick items to permanently delete with fzf
```

A subcommand is recognized only as the very first word (after an optional
`--config PATH`). This matters because `rip` also accepts a few `rm`-style
flags for muscle-memory compatibility -- `-r`/`-R` (ignored: trashing is
always recursive) and `-d` (ignored: rip has no "only if empty" mode) --
and a flag before a word that looks like a subcommand name is ambiguous:

- `rip foo empty` trashes two files, `foo` and `empty`: `empty` isn't the
  first word, so it can't be the subcommand.
- `rip empty` runs the `empty` subcommand: `empty` is the first word.
- `rip -f empty` and `rip -rf empty` are usage errors with a hint, not a
  silent choice either way: an `rm`-style flag before a word that matches a
  subcommand name is refused, with the message telling you to write
  `rip -- empty` to trash a file actually named `empty`.
- `rip -- FILE...` always trashes files: nothing after `--` is ever treated
  as a subcommand, even if it matches one by name.

## Prompts, `-y` and `-f`

An irreversible action always needs either an answer at a terminal or an
explicit flag; without a terminal, `rip` refuses rather than guessing.

- **`-f`** (root command only): never prompt, ignore a missing operand
  (matching `rm -f`, including one that turns out not to exist because a
  path component isn't a directory), and skip the copy-threshold prompt
  described below.
- **`-i`** (root command only): prompt before trashing each item.
- **`-I`** (root command only): prompt once before trashing more than three
  items, or any directory, in one invocation.
- **`-y`** (on `undo`/`restore`/`empty`/`purge`): skip that subcommand's own
  confirmation prompt.
- **The copy-threshold prompt.** When a `put` or `restore` falls back to
  copying (see below) and the item is larger than the configured
  `copy_threshold` (default 500 MiB), `rip` asks before copying and deleting
  the original, unless `-f`/`-y` is given.
- **The `empty` deletion prompt.** Shows how many items and orphans would be
  deleted, and a total size when every selected item's size is already
  known; skipped by `-y`.
- **The `purge` deletion prompt.** Lists the selected items (up to 20, then
  a count of the rest) and asks to confirm the count; skipped by `-y`.

## Where items go

| Source vs. the home trash's filesystem | Result |
|---|---|
| Same subvolume | Renamed into `$XDG_DATA_HOME/Trash` (typically `~/.local/share/Trash`), through whatever mount shows both paths. |
| Same filesystem, different subvolume | Copied into the home trash by default (`fallback = "copy"`); `fallback = "refuse"` leaves it untouched instead. Never creates a topdir trash on the home trash's own filesystem. |
| A different filesystem entirely | A topdir trash at the mount point rip reached the file through: `$topdir/.Trash/$uid` if a shared, sticky `.Trash` exists there, else `$topdir/.Trash-$uid` (created if needed). |

A mount that is itself read-only is refused outright, like `rm`: `rip` never
routes a rename around it through some other, writable view of the same
data.

## Configuration

`rip` reads `$XDG_CONFIG_HOME/ripper/config.toml` (or `~/.config/ripper/config.toml`
if `XDG_CONFIG_HOME` is unset), or the file given by `--config PATH`. A
missing default file is not an error (rip uses the defaults below); a
missing explicit `--config` file is.

```toml
# Defaults shown.
fallback = "copy"        # "copy" or "refuse": what to do when no trash can be
                          # reached by rename (see the placement table above)
copy_threshold = "500M"  # ask before copying+deleting an item larger than this
                          # (binary units: K/M/G/T or KiB/MiB/GiB/TiB; a plain
                          # number of bytes also works; KB/MB/GB are rejected
                          # with a hint, since they would be read as SI by mistake)
```

## On-disk names you may notice

- **`name`, `name~1`, `name~2`, ...** Collisions inside a trash's `files/`:
  the first free name wins, truncated as needed to fit `NAME_MAX` together
  with the matching `.trashinfo` file.
- **`.rip-staging/`** (one per trash root). Holds in-progress copies
  (`put.<pid>.<n>`) during a cross-filesystem copy, and holds tombstones
  (`del.<pid>.<n>`) that `empty`/`purge` rename doomed entries to right
  before the (slower) recursive delete. Ordinarily empty between commands;
  a leftover box or tombstone here after a crash is cleaned up by the next
  `empty`.
- **`.rip-restore.<pid>.<n>`.** A temporary box next to a restore
  destination while copying a trash item back, so a half-copied restore is
  never mistaken for the real file.

## Completions

The Nix package installs completions as described under Install above. To
get a completion script directly (e.g. for a non-Nix setup, or to inspect
one), run:

```
rip --completions bash > _rip_completion   # or zsh, or fish
```

All three (`completions/rip.bash`, `completions/_rip`, `completions/rip.fish`)
are hand-written rather than generated: clap's completion generators cannot
express a first word that is sometimes a subcommand and sometimes a file
(the first-word rule above), and none of them offer trashed original paths
for `restore`/`purge`.

## Limits

- **Linux only.** `rip` depends on `/proc/self/mountinfo`, `renameat2`,
  `statx` and `flock` semantics that are Linux-specific; it will not build
  on another OS.
- **A FUSE mount with no `RENAME2` support** (seen with `ntfs-3g`'s
  `fuseblk`) makes every no-overwrite rename fall back to an explicit
  absent-check followed by a plain rename, which leaves a small window
  between the two where a concurrent write could still be overwritten.
  Every other filesystem uses the atomic `RENAME_NOREPLACE` throughout.
- **A few narrow, accepted risks remain**, each documented with its reasoning
  in `docs/design.md`'s "Accepted residual risks" section: an editor's own
  atomic save landing in the microsecond window between a removal's identity
  check and the actual unlink; an orphaned copy box left behind if `rip` is
  `SIGKILL`ed mid-copy (harmless: the source is untouched until the copy is
  verified and published, and the next `empty` cleans up the box); a
  symlink planted under a shared filesystem between loading and restoring a
  home-trash item; and `restore`/`purge` matching a `PATH` argument as
  written, not through a bind-mount alias that resolves to the same file
  (the fzf picker is unaffected).

## Development

- `just build` -- `cargo build`.
- `just test` -- unit tests and the bwrap sandbox integration tests (Linux
  only; needs unprivileged user namespaces and a btrfs directory, either the
  system temp dir or `$RIP_TEST_BTRFS`).
- `just check` -- `cargo clippy --all-targets -- -D warnings`, then
  `cargo fmt --check`.
- `just fmt` -- `cargo fmt`.
- From macOS: `just artemis check` and `just artemis test` rsync the
  worktree to a configured Linux host and run the same recipe there over
  SSH, since `rip` cannot build or run on macOS.

CI (`.github/workflows/ci.yml`) runs `just check` on every push and pull
request, builds the Nix package (which runs the unit tests) and checks the
installed completions, and runs the full test suite on a real btrfs loop
mount.
