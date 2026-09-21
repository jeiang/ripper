# rip: design reference

`rip` moves files to the freedesktop.org Trash on Linux, handling btrfs subvolumes
and bind mounts (NixOS impermanence) correctly: it renames within a subvolume
whenever a mount shows both source and trash, and only copies when it must.

This document is a maintainer reference: the invariants every change must keep,
the behavior that goes beyond the freedesktop.org spec, the on-disk layout, and
the crash-consistency guarantees. It does not repeat the CLI `--help` text or
the freedesktop.org Trash spec itself.

## 0. Invariants

Every review and test checks a change against these:

1. **Never overwrite.** Every rename is `NOREPLACE`, or on `EINVAL` a plain
   rename after an explicit absent check. `cp -a -T` onto an existing directory
   merges into it and exits 0, so copies always land in a fresh box first.
2. **A complete, durable copy exists before any deletion.** Put: before rip
   removes any source byte. Restore: before rip removes any trash byte.
3. **Info order.** A `.trashinfo` is created with `O_EXCL` before its `files/`
   entry appears. rip removes it only after the `files/` entry has left
   `files/`, by restore or by rename to a tombstone.
4. **Removal stays in place.** Removal never follows a symlink, never enters
   another mount (checked by mount id), and uses only fd-relative `*at` calls.
   rip never calls `std::fs::remove_dir_all` or `std::fs::rename`.
5. **Trash dirs are checked.** rip opens trash directories with `O_NOFOLLOW`
   and checks type, owner, sticky bit (`.Trash`) and mount before use.
6. **Hostile infos are contained.** A topdir `.trashinfo` can only affect
   paths beneath its topdir, and it cannot hang rip.
7. **Consent.** An irreversible action needs a terminal answer or `-y`/`-f`.
   Without a terminal, nothing irreversible happens.
8. **Crash safety.** A crash at any point leaves the old state, the new state,
   or the new state plus a duplicate or garbage that `rip empty` removes.
9. **Locks.** A rip process that writes to a trash dir holds `LOCK_SH` on it.
   `empty` and `purge` hold `LOCK_EX` only while they rename and unlink entries.

## 1. Behavior beyond the freedesktop.org spec and the brief

Each item here is a deliberate choice, reversible if it causes trouble:

- **Extra refusals**, on top of "don't trash `/`, `.`, `..`, a mount point, or
  inside a trash":
  - A directory that is the source of a mount elsewhere (a bind source), e.g.
    `/persist/data/home/aidanp/Downloads`.
  - A directory that contains a trash directory.
  - `link/` where `link` is a symlink or a non-directory.
- **Sizes** (used by `--max-size` and copy-threshold prompts).
  - A size is the apparent size: `st_size` of non-directories.
  - A hard-linked inode counts once per item.
  - Nested btrfs subvolumes count (because `cp -a` copies them). Other mounts
    do not.
  - Units are binary (`500M` = 500 MiB = 524,288,000 bytes). `KB`, `MB`, `GB`
    are rejected with a hint to use the binary unit.
- **Orphans and malformed infos.**
  - Orphans count in `--max-size` and `--older-than`, dated by their ctime.
  - A malformed `.trashinfo` turns its `files/` entry into an orphan.
- **Machine output.** `rip list -0` gives NUL-terminated records.
- **Copy prompt with `-f`.** Root `-f` also skips the copy-threshold prompt,
  and rip copies.
- **Waiting.** A put waits while an `empty` holds its short exclusive lock,
  and an `empty` waits for running puts and restores. Both print `waiting for
  another rip`.

## 2. CLI parsing

### 2.1 The argv pre-scan

clap alone cannot give the brief's parsing rules. Flags and option values
reset clap's parse state, and the next word is checked against subcommand
names again (clap 4.6.7 `parser.rs`), so without a fix `rip foo -f empty`
would run the `empty` subcommand instead of trashing files named `foo` and
`empty`.

`rip` fixes this by classifying the first non-option word *before* invoking
clap (see `first_word` in `src/main.rs`):

- The values of `--config` and `--completions` are skipped when scanning.
- The first word after `--` is always a file; nothing after `--` is ever a
  subcommand.
- If the first non-option word matches a subcommand name, and no rm-style
  flag (anything starting with `-` other than `--config`/`--completions` and
  their values) appeared before it, it is the subcommand.
- If an rm-style flag appeared before a word that matches a subcommand name,
  parsing fails with a hint to write `rip -- NAME`.
- Otherwise the first word is a file: `Command::args_conflicts_with_subcommands(true)`
  is set, so no later word is ever treated as a subcommand.

| Input | Result |
|---|---|
| `rip foo`, `rip foo empty` | Files. |
| `rip foo -f empty` | Files `[foo, empty]` with force. |
| `rip foo --config c empty` | Files `[foo, empty]`. |
| `rip foo --config c empty -y` | Exit 2 (`-y` is not a root flag). Nothing is trashed and the trash is never emptied. |
| `rip empty`, `rip --config c empty`, `rip --config=c list`, `rip empty --config c` | The subcommand (`--config` is `global`). |
| `rip -- empty`, `rip -f -- -f`, `rip -- -- x` | Files `[empty]`, `[-f]`, `[--, x]`. |
| `rip -rf empty`, `rip -v list`, `rip --config c -v empty`, `rip -f empty` | Exit 2 with the `rip -- NAME` hint. This happens before any filesystem access. |
| `rip help`, `rip -` | Files (`help` is not a registered subcommand: `disable_help_subcommand` is set). |
| `rip --completions fish`; `rip --completions fish foo` | Prints the script; exit 2 (`exclusive`). |
| `rip` / `rip -f` | Exit 2 "missing operand" / exit 0. `main` checks this after parsing. |

### 2.2 Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. Also a "no" answer to any prompt, a cancelled picker or no fzf match, `rip -f` with no operand, and `list` or `empty` with nothing to do. |
| 1 | Operational failure: a refusal, a missing file without `-f`, a prompt with no terminal, fzf missing, nothing to undo or restore, `--max-size` with the newest item too large, a partial failure (other items still run), or a kept entry after deletion. |
| 2 | Usage, config or environment error: clap errors, rm flags before a subcommand, missing operand, missing `--config` file, invalid TOML, unknown key or value, `HOME` not absolute. Nothing was done. |

Output rules:

- Fatal errors print `rip: MSG`.
- Per-item errors print `rip: cannot trash|restore|delete 'X': REASON`, and the
  batch continues.
- `-v` and restored paths go to stdout.
- `BrokenPipe` on stdout in `list` counts as success.

`src/main.rs` builds the shared `Cx` (config, mountinfo, uid, cwd, home) after
argv parsing and config loading succeed, then dispatches to `put`, `restore`
or `empty`. A command function returning `Err(String)` at that point is always
an operational failure (exit 1): every usage/config/environment problem is
caught earlier and exits 2 before any command function runs.

## 3. Placement for put

### 3.1 Choosing the trash

Given the source's `st_dev` (one per btrfs subvolume) and mountinfo `FsId`
(one per filesystem, shared by every subvolume of it):

| Source vs. home trash | Result |
|---|---|
| Same `st_dev` | Home trash (`$XDG_DATA_HOME/Trash`), renamed through whatever mount shows both paths (e.g. `/persist` on artemis). |
| Same `FsId`, different `st_dev` | Config `fallback` (copy or refuse). rip never creates `$topdir/.Trash-$uid` on the home trash's filesystem. |
| Different `FsId` | A topdir trash at the mount point through which rip reached the file: `$topdir/.Trash/$uid` if an admin `.Trash` is a valid sticky directory, else `$topdir/.Trash-$uid` (created on demand). |

A rename between two mounts of the *same* subvolume still returns `EXDEV`
(verified on artemis: `~/Downloads/x -> ~/Documents/x` fails, the same move
through `/persist` succeeds), so rip finds one mount that shows both the
source and the trash's `files/` directory, opens both through it, and
verifies each opened fd's mount id and `(dev, ino)` before renaming.

When no trash is usable on the file's own filesystem (ephemeral root
subvolume, a root-owned topdir, a read-only filesystem), rip copies into the
home trash by default (`fallback = "copy"`), asking first when the item is
larger than `copy_threshold` (default 500 MiB); `fallback = "refuse"` leaves
the item untouched with a clear warning instead.

### 3.2 Results on artemis (verified)

| Source | Result |
|---|---|
| `~/Downloads/x` | Home trash, renamed through `/persist`. |
| `/persist/x` | Home trash through the same mount; the parent is root-owned so the rename fails with `EACCES` and the item fails (no copy). |
| `~/x`, `/tmp/x`, `/mnt/root/rootfs/x` | Same filesystem, other subvolume: fallback (copy by default). |
| `/mnt/Mumei/sub/x` | Existing `/mnt/Mumei/.Trash-1000`, `Path=sub/x`. |
| `/run/user/1000/x` | Creates `/run/user/1000/.Trash-1000`. |
| `/nix/store/...` | `mkdirat` gives `EROFS`, so the fallback runs; the preflight then finds the source is not removable and refuses it before any copy. |

## 4. On-disk names

- **Collision names inside `files/`.** `name`, `name~1`, `name~2`, ... (a
  suffix appended after any extension), truncated as needed so
  `name~N.trashinfo` fits `NAME_MAX` (255 bytes). A valid UTF-8 name is cut on
  a char boundary.
- **`.rip-staging/`.** One per trash root. Holds in-progress copy boxes
  (`put.<pid>.<n>`) during a cross-filesystem copy, and holds tombstones
  (`del.<pid>.<n>`) that `empty`/`purge` rename doomed entries to before the
  slow recursive delete, so the exclusive lock is held only for the rename.
- **`.rip-restore.<pid>.<n>`.** A box created next to a restore target while
  copying a trash item back, so a half-copied restore is never mistaken for
  the real file.
- **Orphans.** `files/N` with no paired info (or an info that fails to parse)
  becomes an orphan, dated by its own ctime. Plain `empty` removes orphaned
  `files/` entries with no `.trashinfo`; every `empty` removes `.trashinfo`
  files whose `files/` entry is missing (dangling infos), with no grace
  period.

## 5. Crash consistency

### 5.1 Put: rename path

| Step | On failure | If killed after it |
|---|---|---|
| Refusal, stat or prompt | Nothing was created. | n/a |
| Reservation (`O_EXCL` info file) | The guard unlinks the info file. | A dangling info file. `list` hides it, and the next `empty` removes it (under `LOCK_EX`, never during a put). |
| Rename `EEXIST` | Release the name and claim the next one. | Same. |
| Rename `EXDEV` | Guard unlinks the info, then the copy fallback runs. | Same. |
| Rename, other errors | Guard unlinks the info. The source is untouched. | n/a |
| Rename done | n/a | A complete item. |

`RENAME_NOREPLACE` giving `EINVAL` (seen on the `/mnt/Mumei` ntfs-3g fuseblk
mount, which has no RENAME2 support) is handled by checking the target is
absent, then doing a plain rename; this applies to every NOREPLACE rename
(put, publish, restore, tombstone) and leaves a small, accepted race window
between the check and the rename on such filesystems.

### 5.2 Put: copy-fallback path

| Failure or crash at | State | Recovery |
|---|---|---|
| Preflight problem, declined prompt | Nothing written. | Refused or declined. |
| During `cp` (error, `ENOSPC`, a signal) | Partial box in `.rip-staging`. Source untouched. No info. | Rollback removes the box. After a crash, the next `empty` removes it (under `LOCK_EX`, so never while a put runs). |
| After reservation, before publish | Complete info and complete box. Source untouched. | Rollback removes both. After a crash: a dangling info and a stale box, which `empty` removes. |
| After publish, before or during `syncfs` | Complete item and an intact source (a duplicate). | Nothing is lost. |
| During source removal | Complete, durable item. The source is partly removed. | The trash holds everything; rip reports the kept entries. |
| Removal removed nothing | Trash copy discarded. Source untouched. | Error: "it changed while it was copied". |

The copy path is: copy into a fresh box in `.rip-staging`, reserve the
`.trashinfo`, publish with `NOREPLACE`, `syncfs`, then remove the source
using a preflight manifest (`ino -> (size, mtime)`, mtime rather than ctime
because unlinking one name of a hard-linked inode changes ctime but not
mtime) so a source entry that changed during the copy is left in place
instead of being deleted out from under a stale assumption.

### 5.3 Restore

| Killed after | State | Result |
|---|---|---|
| Parents created | Extra empty directories. | Harmless. |
| Rename back | Restored, with a dangling info. | `list` hides it, and `empty` removes it. |
| During the copy | A partial `.rip-restore.*` next to the destination. The item is intact and listed. | No loss. |
| Publish, before `syncfs` or tombstone | Destination complete. The item is still listed (a duplicate). | A second restore is refused because the path exists. |
| Tombstone | Destination complete, with a dangling info. | Same as the rename-back row. |
| Tombstone removal | Leftover in `.rip-staging`. | Removed by the next `empty`. |

### 5.4 Empty and purge (`delete_batch`)

`LOCK_EX` is held only while entries are renamed to tombstones (in
`.rip-staging`) or unlinked; the slow recursive delete of each tombstone runs
after the lock is released, so a timer-run `empty` never blocks an
interactive put or restore, and a restore can never receive a directory that
is mid-deletion through an open fd.

- Every doomed item is rechecked under the lock: same `files/NAME` identity,
  same info identity and bytes (a restore plus a new put can reuse a name
  between load time and delete time).
- A mount found inside an item (`entry_conflict`) stops that item before any
  change.
- An item is either complete, or already a tombstone holding only garbage.
  A dangling info is harmless. A crash cannot delete anything that was not
  selected.
- Two concurrent empties may both remove the same tombstone; that is
  harmless because deletion is idempotent.

## 6. Data-loss paths and their tests

| Data-loss path | Protection | Tests |
|---|---|---|
| A rename overwrites an entry in `files/` | NOREPLACE; `files/` lstat in `reserve` | `collisions_and_name_max`; `noreplace_with` table |
| NOREPLACE unsupported on FUSE (`EINVAL`) | `noreplace_with`: absent check, then plain rename | `noreplace_with` table; §15.2 item 1 on Mumei |
| A rename through a wrong or covered mount | fd verification (mount id and `(dev, ino)`) | `covered_persist_not_used`; mounts fixture tests |
| `.Trash-$uid` created on the home trash's filesystem | the `choose()` FsId rule | `side_bind_root_copies_no_topdir_trash`, `pside_bind_root_uses_home_trash`; `choose` table |
| A live bind source, a trash, or a dir holding mounts moved | `mount_conflict`, `inside_trash`, `contains_trash` | `refusals`; `mount_conflict` units |
| `rip dir dir/f` moves `f` out of the trashed item | Each operand resolved just before it moves | `parent_then_child` |
| Source deleted before the copy is complete and durable | Box, then reserve, publish, `syncfs`, then removal | `copy_rollback_unreadable_file`, `ephemeral_root_copies` |
| A source entry changed during the copy is deleted | Manifest `(ino, size, mtime)` | `remove_tree` manifest unit |
| A hard-linked tree is reported as changed | mtime, not ctime | `copy_hardlinked_tree`; hard-link unit |
| Copy complete but the source half-deleted | Full-tree preflight | `copy_refused_unwritable_subdir`, `copy_refused_sticky_foreign` |
| Removal crosses into another mount | Pre-check plus the mount-id guard | `never_crosses_mounts`, `refusals` |
| Removal follows a symlink | `O_NOFOLLOW` and `AT_SYMLINK_NOFOLLOW` everywhere | `never_follows_symlinks`; `remove_tree` unit |
| A `files` symlink makes empty delete home | Discovery opens with `O_NOFOLLOW` | `skips_invalid_dirs` |
| empty deletes an item restored concurrently, or a reused name | `LOCK_EX`, identity and byte recheck, tombstones | `delete_batch_skips_reused_name`, `waits_for_lock` |
| empty deletes an in-flight put's info or box | `LOCK_SH` across the whole put; reserve after the copy | `waits_for_lock`, `crash_states` |
| A deep tree fails midway | Explicit stack, `RLIMIT_NOFILE` raise, tombstone retry | 5000-level unit |
| Restore overwrites a file | lstat refusal plus NOREPLACE | `conflict_refused_and_rename` |
| Restore deletes the trash copy before the target is complete | Box, publish, `syncfs`, tombstone | `copy_back_enospc_keeps_item` |
| A hostile `Path=` writes outside its topdir | `RESOLVE_BENEATH` | `hostile_paths` |
| A FIFO info hangs rip | `O_NONBLOCK` and `S_ISREG` | `malformed_and_fifo_infos` |
| `--max-size` or `--older-than` deletes too much | Pure selection | `select_for_empty` table; the three `empty` scenarios |
| `rip -rf empty` or `rip foo --config c empty -y` empties the trash | argv pre-scan | parse table; `argv_rules` |
| An irreversible action without a terminal | `confirm` fails | `no_tty_refuses`, `copy_threshold`, `purge_no_tty` |
| Undo cannot restore `rip dir/f dir` | Parents first | `undo_parents_first` |

Power loss cannot be tested; the `syncfs` placement is a standing review item.

## 7. Module layout

```
src/main.rs     CLI types, argv pre-scan, config, prompts, display helpers, Cx, dispatch, exit codes
src/mounts.rs   [pure] mountinfo parser, FsId, path translation, mount refusals, route candidates
src/sys.rs      Dev/Ident/Meta, statx, open helpers, fd_path, rename_noreplace, lock, route, walk,
                remove_tree, cp_archive, make_box, syncfs, raise_nofile
src/info.rs     [pure] .trashinfo encode/parse, Path= codec, dates, Kind, original(), collision names
src/trash.rs    Trash, home trash, discovery, load, Reserved guard, staging, tombstones, delete_batch, discard
src/put.rs      rip FILE...: resolution, refusals, choose(), topdir trash, rename, copy fallback
src/restore.rs  list, selection, fzf picker, restore, undo, purge command
src/empty.rs    empty, select_for_empty()
```

`rip` is Linux-only (`compile_error!` on other targets): the trash spec,
`/proc/self/mountinfo`, `renameat2`, `statx` and `flock` semantics it depends
on are Linux-specific. Development happens on macOS inside the Nix dev shell
for editing and `cargo fmt`; every build, clippy run and test runs on Linux
(artemis over SSH, or GitHub Actions CI).
