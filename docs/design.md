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
   paths beneath its topdir, it cannot hang rip, and reading it never
   allocates or reads past its size cap even if a concurrent writer grows the
   file after rip's own `fstat`.
7. **Consent.** An irreversible action needs a terminal answer or `-y`/`-f`.
   Without a terminal, nothing irreversible happens.
8. **Crash safety.** A crash at any point leaves the old state, the new state,
   or the new state plus a duplicate or garbage that `rip empty` removes.
9. **Locks.** A rip process that writes to a trash dir holds `LOCK_SH` on it.
   `empty` and `purge` hold `LOCK_EX` only while they rename and unlink entries.
10. **Human output never writes an untrusted name raw.** A path or name that
    came from outside rip (an operand, a `Path=` value, a `files/` entry)
    goes through `escape()` before reaching stdout, stderr or a terminal:
    `\n` and `\t` as `\n`/`\t`, every other control byte (C0, DEL, and C1 --
    `U+0080..=U+009F`, valid as UTF-8, a terminal in UTF-8 mode can act on
    the same as its C0 equivalent) as `\xNN`, invalid UTF-8 as `\xNN` -- so a
    hostile name cannot smuggle a terminal escape sequence through as "plain
    text".

## 1. Behavior beyond the freedesktop.org spec

Each item here is a deliberate choice, reversible if it causes trouble:

- **Extra refusals**, on top of "don't trash `/`, `.`, `..`, a mount point, or
  inside a trash":
  - A directory that is the source of a mount elsewhere (a bind source), e.g.
    `/persist/data/home/aidanp/Downloads`.
  - A directory that contains a trash directory.
  - `link/` where `link` is a symlink or a non-directory.
  - A filesystem mounted read-only: refused the way `rm` is, never routed
    around through some other, writable alias of the same subvolume. Picking
    a mount to rename through likewise skips a read-only candidate and tries
    the next one, rather than failing or silently defeating a read-only view
    someone set up on purpose.
  - An operand inside a trash directory that discovery skipped with a
    warning (e.g. a missing `info/`), not only one it could open: it is
    still that directory's trash, waiting to be repaired and reused.
  - An operand whose parent directory is immutable or append-only
    (`chattr +i`/`+a`), refused up front for the copy fallback: the kernel
    only enforces append-only at unlink/rmdir time, after rip would already
    have published a copy and started deleting the source.
- **Sizes** (used by `--max-size` and copy-threshold prompts).
  - A size is the apparent size: `st_size` of non-directories.
  - A hard-linked inode counts once per item.
  - Nested btrfs subvolumes count (because `cp -a` copies them). Other mounts
    do not.
  - Units are binary (`500M` = 500 MiB = 524,288,000 bytes). `KB`, `MB`, `GB`
    are rejected with a hint to use the binary unit.
  - **`--max-size` batches.** Items sharing one `DeletionDate` (one `rip`
    invocation) are one batch; an orphan (dated by ctime, an unrelated clock)
    is always its own single-item batch, even when that ctime happens to
    equal a neighboring item's `DeletionDate`. Batches are kept newest-first
    while the running total stays at most the limit; the first batch that
    pushes the total over, and every older batch, are deleted whole (a batch
    is never split by size). If the newest batch alone is already over the
    limit, nothing is deleted and rip errors. The confirmation prompt shows a
    size only when every selected entry's size is already known from
    selection's own lazy walk; otherwise the `(size)` clause is left off
    rather than walking the rest of the trash just to fill in the prompt.
- **Orphans and malformed infos.**
  - Orphans count in `--max-size` and `--older-than`, dated by their ctime.
  - A malformed `.trashinfo` turns its `files/` entry into an orphan.
  - A `.trashinfo` name that could never be a real `files/` entry (empty,
    `.`, `..`, or containing `/`) is garbage from the start: a dangling
    info, not a phantom item or orphan.
  - `files/NAME` present but its `.trashinfo` unopenable (a dangling
    symlink, unreadable) is an orphan with a warning, not a permanently
    stuck entry nothing can ever remove.
- **Machine output.** `rip list -0` gives NUL-terminated records.
- **Copy prompt with `-f`.** Root `-f` also skips the copy-threshold prompt,
  and rip copies. `-f` (like `rm -f`) also ignores an operand that resolves
  through a non-directory path component (`ENOTDIR`), the same as one that
  is simply missing.
- **Waiting.** A put waits while an `empty` holds its short exclusive lock,
  and an `empty` waits for running puts and restores. Both print `waiting for
  another rip`.
- **An item's own path shown as cwd.** An item whose original path is
  exactly the current directory displays as `.` (list, the fzf picker,
  restore/purge reports), never an empty string.
- **Home trash: missing `files/`/`info/`.** A home trash whose `files/` and
  `info/` are *both* absent is silent, like no home trash existing yet (an
  impermanence setup can bind-mount the home trash's parent into place
  before either is created). Exactly one of the two missing is corruption,
  not impermanence: it warns and is skipped, the same as a topdir trash
  missing one of them, instead of silently hiding real trashed data.
- **Home trash: relative `Path=` base.** A relative `Path=` in the home
  trash resolves against `$XDG_DATA_HOME` itself, not the canonicalized
  `Trash` directory's parent. This only differs when `Trash` is a symlink
  (impermanence's "symlink" method): resolving against the symlink's target
  instead would misplace `restore`/`undo`'s output.

## 2. CLI parsing

### 2.1 The argv pre-scan

clap alone cannot give rip's own parsing rules. Flags and option values
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
| `rip --completions fish` | Prints the script; exit 0. |
| `rip --completions fish foo` | Exit 2: `--completions` is `exclusive`, so combining it with another argument is a clap usage error and nothing is printed. |
| `rip` / `rip -f` | Exit 2 "missing operand" / exit 0. `main` checks this after parsing. |

All three completion scripts (`completions/rip.bash`, `completions/_rip`,
`completions/rip.fish`) are hand-written, not generated: each mirrors
`first_word`'s own file-vs-subcommand resolution at completion time (a
`_rip_state`/`__rip_state` function classifying the words before the cursor
into `first`, `files`, `--`, or a subcommand name, the same way `first_word`
classifies argv), which clap's static generators cannot do, and each offers
trashed original paths for `restore`/`purge` from `rip list -0`, which no
generated completion can do either. Two correctness fixes recur across them:
a trashed path's own embedded newline must never forge extra completion
candidates (each shell's NUL-delimited read keeps such a record's newline
part of it, and the record is then skipped whole rather than split); and a
candidate containing a space or another shell metacharacter (a file or a
trashed path) must be inserted correctly quoted (bash: `compopt -o
filenames`, collecting each candidate without word-splitting it; zsh:
`compadd`'s own quoting). Fish also needed a fix specific to it: fish 4's
qmark-noglob feature (default since fish 4.0) makes `?` a literal character
in a glob rather than a wildcard, so its rm-style-flag pattern must be
`'-*'`, not `'-?*'` (which stopped matching any real flag under fish 4).

bash needed two more fixes of its own. Real readline splits `COMP_WORDS` not
just on whitespace but at every run of a non-whitespace `COMP_WORDBREAKS`
character (default includes `=` and `:`), so `--config=PATH` and a
colon-bearing path (a trashed original path, or a file) arrive as several
words instead of one; `completions/rip.bash`'s `_rip_reassemble` glues such
runs back onto their neighbors before `_rip_state` and the file/trashed-path
matching run, and each match is then trimmed back down to only the part
readline still expects to insert (the bash-completion
`__ltrim_colon_completions` trick, generalized to both split characters).
Readline also leaves a still-typed prefix in its raw quoted form (an
inserted `My\ Doc`, or a still-open `'My Do`); `_rip_dequote` strips that
before matching a file or trashed path, since `compgen -f` has the same
problem matching a raw escaped prefix literally.

zsh's `_rip_trashed` shows the deletion date next to each trashed path with
`compadd -l -d displays -a paths` (display strings of the form
`PATH -- DATE`, the same approach `_describe` uses internally): `-d` without
`-l` makes the description array replace the match in the listing instead of
annotating it, which would show only dates and no paths.

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

`Cx::new` does not fail merely because `getcwd` does: if the invoking shell's
own working directory was just trashed (e.g. `rip ../x` run from inside `x`),
it falls back to `$PWD` (when set and absolute) or `/` instead of exiting 2,
printing a warning. Every subcommand still runs; a relative operand or PATH
against the stand-in cwd simply fails to find its target, the same as from
any other stale working directory. `dispatch` also raises `RLIMIT_NOFILE`'s
soft limit to the hard limit once, before any command runs
(`sys::raise_nofile`), so a tree only a few hundred levels deep does not hit
`EMFILE` under a common default soft limit of 1024.

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
verifies each opened fd's mount id and `(dev, ino)` before renaming --
skipping a read-only candidate mount and trying the next one, since renaming
through it would fail (or, worse, silently defeat a read-only view someone
set up on purpose).

When no trash is usable on the file's own filesystem (ephemeral root
subvolume, a root-owned topdir), rip copies into the home trash by default
(`fallback = "copy"`), asking first when the item is larger than
`copy_threshold` (default 500 MiB); `fallback = "refuse"` leaves the item
untouched with a clear warning instead. An operand whose *own* mount is
read-only is refused outright instead, the way `rm` is: it is never routed
around through the copy fallback, since the mount that would not let it be
overwritten will not let it be deleted either.

Opening or creating a topdir trash (`.Trash`, `.Trash-$uid`, its own
`files/`/`info/`) is entirely fd-relative and `O_NOFOLLOW` throughout, never
re-resolved by path after the mount/subvolume checks: a symlink a concurrent
writer swaps in for any of these partway through is refused, not followed.

### 3.2 Results on artemis (verified)

| Source | Result |
|---|---|
| `~/Downloads/x` | Home trash, renamed through `/persist`. |
| `/persist/x` | Home trash through the same mount; the parent is root-owned so the rename fails with `EACCES` and the item fails (no copy). |
| `~/x`, `/tmp/x`, `/mnt/root/rootfs/x` | Same filesystem, other subvolume: fallback (copy by default). |
| `/mnt/Mumei/sub/x` | Existing `/mnt/Mumei/.Trash-1000`, `Path=sub/x`. |
| `/run/user/1000/x` | Creates `/run/user/1000/.Trash-1000`. |
| `/nix/store/...` | `/nix/store` is itself a read-only mount: refused outright ("its filesystem is read-only"), before `choose()` or any copy attempt. |

## 4. On-disk names

- **Collision names inside `files/`.** `name`, `name~1`, `name~2`, ... (a
  suffix appended after any extension), truncated as needed so
  `name~N.trashinfo` fits `NAME_MAX` (255 bytes). A valid UTF-8 name is cut on
  a char boundary.
- **`.rip-staging/`.** One per trash root. Holds in-progress copy boxes
  (`put.<pid>.<n>`) during a cross-filesystem copy, and holds tombstones
  (`del.<pid>.<n>`) that `empty`/`purge` rename doomed entries to before the
  slow recursive delete, so the exclusive lock is held only for the rename.
  `delete_batch` only opens an existing `.rip-staging`, never creates one,
  when there is nothing doomed in that trash dir: a read-only trash with
  nothing selected exits 0 instead of failing to create a directory it does
  not need. Tombstone names continue from a shared counter across a whole
  batch instead of restarting at 0 per entry, so a batch of N doomed entries
  costs N renames total, not N(N+1)/2 (all under `LOCK_EX`). Tombstoning a
  top-level directory the current user owns but lacks `u+w` on (moving it
  needs write permission on itself, to update `..`) best-effort adds that
  bit first, so a read-only-mode trashed directory is still removable.
- **`.rip-restore.<pid>.<n>`.** A box created next to a restore target while
  copying a trash item back, so a half-copied restore is never mistaken for
  the real file.
- **Orphans.** `files/N` with no paired info (or an info that fails to parse)
  becomes an orphan, dated by its own ctime. Plain `empty` removes orphaned
  `files/` entries with no `.trashinfo`; every `empty` removes `.trashinfo`
  files whose `files/` entry is missing (dangling infos), with no grace
  period. A dangling info whose recheck under the lock shows it is no longer
  dangling (a concurrent put or restore resolved it first) is silently left
  alone: not counted as deleted, not reported as a failure.

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
| Removal removed nothing | Trash copy discarded (`discard_verified`, tied to the entry's own identity captured right after publish -- see §5.3). Source untouched. | Error: "it changed while it was copied". |

The copy path is: walk the source once, before any write, building a
manifest keyed by each entry's path relative to the walked tree's own top
(not by `(dev, ino)` alone: a bare inode is only unique within one `st_dev`,
nested btrfs subvolumes routinely reuse low inode numbers, and an inode a
concurrent writer renamed into a path the walk already visited would
otherwise match there even though `cp` never copied it at that path; mtime,
not ctime, for the content check, since unlinking one name of a hard-linked
inode changes ctime but not mtime); copy into a fresh box in `.rip-staging`;
walk the finished copy and verify it against the same manifest by path
(`verify_copy`: same type, size and mtime at the same relative path);
reserve the `.trashinfo`; publish with `NOREPLACE`; `syncfs`; then remove
the source. An entry is removable only where the pre-copy walk saw it
unchanged AND the post-copy walk found a matching copy of it at that exact
relative path -- proof the copy actually holds that entry, not merely that
some entry somewhere in the tree still has matching identity, size and
mtime.

### 5.3 Restore

| Killed after | State | Result |
|---|---|---|
| Parents created | Extra empty directories. | Harmless. |
| Rename back | Restored, with a dangling info. | `list` hides it, and `empty` removes it. |
| During the copy | A partial `.rip-restore.*` next to the destination. The item is intact and listed. | No loss. |
| Publish, before `syncfs` or tombstone | Destination complete. The item is still listed (a duplicate). | A second restore is refused because the path exists. |
| Tombstone | Destination complete, with a dangling info. | Same as the rename-back row. |
| Tombstone removal | Leftover in `.rip-staging`. | Removed by the next `empty`. |

A copy-back's size prompt is asked *before* any lock is taken, the same way
put's own copy-fallback prompt is: otherwise `empty`/`purge` (including an
unattended timer) would wait on a human answer with no time limit. The lock
is then taken and the entry is rechecked (`trash::still_same`, same
`files/NAME` identity, same info identity and bytes) immediately after the
lock, and again immediately before each point that actually touches
`files/NAME` -- the rename-back, and the copy -- since a concurrent
restore or put sharing the same `LOCK_SH` could have taken the name at any
point up to that instant. Once the destination is complete and durable, the
now-unneeded trash copy is removed with `trash::discard_verified`, tied to
the entry's own identity and info rather than deleted by name alone: if a
concurrent restore-plus-new-put has already reused the freed name for
something else, `discard_verified` renames it back untouched and refuses,
instead of destroying an unrelated item. `put.rs`'s own
copy-fallback rollback calls the same `discard_verified`, tied to the
identity it captured right after publishing its own entry, for the same
reason.

`restore`/`undo` sort a batch parents first; once a batch item's own
restore is declined, refused or fails, every later item whose original
path lies under it is skipped and reported ("its parent PATH was not
restored; it stays in the trash") instead of being restored into a
directory `restore` would otherwise have to fabricate in the real parent's
place. This counts as a failure (exit 1) even for the declined-parent case.

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
| NOREPLACE unsupported on FUSE (`EINVAL`) | `noreplace_with`: absent check, then plain rename | `noreplace_with` table; §5.1 (verified on Mumei) |
| A rename through a wrong or covered mount | fd verification (mount id and `(dev, ino)`) | `covered_persist_not_used`; mounts fixture tests |
| `.Trash-$uid` created on the home trash's filesystem | the `choose()` FsId rule | `side_bind_root_copies_no_topdir_trash`, `pside_bind_root_uses_home_trash`; `choose` table |
| A live bind source, a trash, or a dir holding mounts moved | `mount_conflict`, `inside_trash`, `contains_trash` | `refusals`; `mount_conflict` units |
| A read-only mount routed around through a writable alias | Refused up front (`own.ro`); `route()` skips a read-only candidate | `read_only_mount_is_refused_not_routed_around` |
| An operand inside a trash discovery skipped (missing `info/`) reused | `topdir_trash`'s own inside-trash check, not only discovery's | `skipped_half_trash_is_still_refused_not_repaired_and_reused` |
| `rip dir dir/f` moves `f` out of the trashed item | Each operand resolved just before it moves | `parent_then_child` |
| Source deleted before the copy is complete and durable | Box, then reserve, publish, `syncfs`, then removal | `copy_rollback_unreadable_file`, `ephemeral_root_copies` |
| A source entry changed during the copy is deleted | Manifest keyed by path, verified against the finished copy (`verify_copy`) | `remove_tree` manifest units; `Manifest`/`verify_copy` unit tests |
| A hard-linked tree is reported as changed | mtime, not ctime | `copy_hardlinked_tree`; hard-link unit |
| Copy complete but the source half-deleted | Full-tree preflight | `copy_refused_unwritable_subdir`, `copy_refused_sticky_foreign` |
| Removal crosses into another mount | Pre-check plus the mount-id guard | `never_crosses_mounts`, `refusals` |
| Removal follows a symlink | `O_NOFOLLOW` and `AT_SYMLINK_NOFOLLOW` everywhere | `never_follows_symlinks`; `remove_tree` unit |
| A `files` symlink makes empty delete home | Discovery opens with `O_NOFOLLOW` | `skips_invalid_dirs` |
| empty deletes an item restored concurrently, or a reused name | `LOCK_EX`, identity and byte recheck, tombstones | `delete_batch_skips_reused_name`, `waits_for_lock` |
| A copy-back restore, or put's copy-fallback rollback, discards a name reused by an unrelated item in the meantime | `discard_verified`, tied to the entry's own identity, refuses and puts it back on mismatch | `concurrent_restore_does_not_discard_a_name_reused_by_an_unrelated_item`; `discard_verified_puts_back_an_entry_that_changed_identity` |
| Undo/restore fabricates a parent directory for a child whose own parent restore was declined, refused or failed | Parents-first sort, then later children under a blocked parent are skipped | `undo_skips_child_when_parent_restore_is_declined` |
| empty deletes an in-flight put's info or box | `LOCK_SH` across the whole put; reserve after the copy | `waits_for_lock`, `crash_states` |
| A racing writer makes a `.trashinfo` read past its size cap | The read itself is capped, not just the `fstat` | `reread_info_never_exceeds_the_cap_even_if_the_file_grows_after_fstat` |
| A deep tree fails midway | Explicit stack, `RLIMIT_NOFILE` raise (called from `dispatch`), tombstone retry | 5000-level unit; `dispatch_raises_the_soft_nofile_limit` |
| Restore overwrites a file | lstat refusal plus NOREPLACE | `conflict_refused_and_rename` |
| Restore deletes the trash copy before the target is complete | Box, publish, `syncfs`, tombstone | `copy_back_enospc_keeps_item` |
| A hostile `Path=` writes outside its topdir | `RESOLVE_BENEATH` | `hostile_paths` |
| A FIFO info hangs rip | `O_NONBLOCK` and `S_ISREG` | `malformed_and_fifo_infos` |
| `--max-size` or `--older-than` deletes too much | Pure selection, whole-batch walk (never splits a `DeletionDate` batch by size) | `select_for_empty` table; `max_size` scenarios |
| `rip -rf empty` or `rip foo --config c empty -y` empties the trash | argv pre-scan | parse table; `argv_rules` |
| An irreversible action without a terminal | `confirm` fails | `no_tty_refuses`, `copy_threshold`, `purge_no_tty` |
| Undo cannot restore `rip dir/f dir` | Parents first | `undo_parents_first` |
| A hostile name reaches the terminal raw | `escape()` (C0, C1, DEL, invalid UTF-8) at every human-output site | `escape_c1_controls`; `escape_on_terminal`; the `*_escapes_a_hostile_*` put/restore tests |
| A command run from a since-deleted cwd exits 2 for every later command | `Cx::new` falls back to `$PWD`/`/` instead of propagating `getcwd`'s error | `commands_run_from_a_deleted_cwd_do_not_exit_2`; `cwd_fallback_*` units |

Power loss cannot be tested; the `syncfs` placement is a standing review item.

## 7. Accepted residual risks

Identified and deliberately left unfixed, each
because closing it needs either real-time coordination rip has no way to do
from a single process, or a scope well beyond "small and direct":

- **A save lands in the microsecond window between `remove_tree`'s lstat and
  its unlink.** Removal checks an entry immediately before removing it, but
  the check and the removal are still two syscalls, not one atomic
  operation; an editor's own atomic-save rename could in principle land in
  between. No portable, lock-free way to make lstat-then-unlink atomic
  exists on Linux.
- **A copy fallback's box, orphaned in `.rip-staging` after `rip` is
  `SIGKILL`ed mid-`cp`.** The source is untouched in this case (nothing is
  removed until after the copy is verified and published), so nothing is
  lost; the next `empty` cleans up the stale box, the same as any other
  crash during the copy.
- **A planted symlink under a shared filesystem, when restoring a home-trash
  item whose original directory still exists there.** Home-trash restores
  resolve from `/` with symlinks followed (unlike a topdir restore's
  `RESOLVE_BENEATH`), matching how the user originally reached the path; a
  symlink planted in a shared, writable parent between load and restore is
  the same risk `mv`/`cp` already carry for any absolute path, not one rip
  introduces.
- **`restore`/`purge` matching a `PATH` argument through a bind-mount
  alias.** An original path is compared as written; an alias that resolves
  to the same file (e.g. `/mnt/root/persist/...` vs `/persist/...`) will not
  match a `PATH` argument spelled the other way. The fzf picker (no `PATH`
  argument, selection by trash-relative name) is unaffected and is the
  documented way to restore/purge when the original path is unclear.

## 8. Module layout

```
src/main.rs     CLI types, argv pre-scan, config, prompts, display helpers, Cx, dispatch, exit codes
src/mounts.rs   [pure] mountinfo parser, FsId, path translation, mount refusals, route candidates
src/sys.rs      Dev/Ident/Meta, statx, open helpers, fd_path, rename_noreplace, lock, route, walk,
                remove_tree, cp_archive, make_box, syncfs, raise_nofile
src/info.rs     [pure] .trashinfo encode/parse, Path= codec, dates, Kind, original(), collision names
src/trash.rs    Trash, home trash, discovery, load, Reserved guard, staging, tombstones, delete_batch, discard_verified
src/put.rs      rip FILE...: resolution, refusals, choose(), topdir trash, rename, copy fallback
src/restore.rs  list, selection, fzf picker, restore, undo, purge command
src/empty.rs    empty, select_for_empty()
```

`rip` is Linux-only (`compile_error!` on other targets): the trash spec,
`/proc/self/mountinfo`, `renameat2`, `statx` and `flock` semantics it depends
on are Linux-specific. Development happens on macOS inside the Nix dev shell
for editing and `cargo fmt`; every build, clippy run and test runs on Linux
(artemis over SSH, or GitHub Actions CI).
