//! Fd-based system primitives: identity (`Dev`/`Ident`/`Meta`), open/stat/rename
//! helpers, `flock`, mount routing, the preflight walk and its manifest,
//! `remove_tree`, and the `cp -a` copy fallback. See docs/design.md §4
//! (on-disk names), §5 (crash consistency) and §6.5-§6.7 for the invariants
//! these primitives back: every rename is `NOREPLACE`, removal never follows
//! a symlink or crosses a mount, and a copy only ever reads its source.
//!
//! Every item here is fully implemented (§14.1's fixed signatures), but
//! nothing outside this file calls into it yet: `trash.rs`, `put.rs`,
//! `restore.rs` and `empty.rs` (C3-C4c) are still `unimplemented!()` stubs
//! that will call these primitives once they land. Until then the whole
//! module is unreachable from `main`, so `dead_code` would flag essentially
//! every item individually; a module-level allow says that once, instead of
//! repeating "first caller lands in a later checkpoint" on ~40 items.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustix::fs::{
    self, Access, AtFlags, CWD, Mode, OFlags, RenameFlags, StatxAttributes, StatxFlags, mkdirat,
    openat, renameat, renameat_with, unlinkat,
};
use rustix::io::{Errno, FdFlags, fcntl_setfd};
use rustix::path::Arg;
use rustix::process::{Resource, Rlimit, getrlimit, getuid, setrlimit};

use crate::mounts::{self, Mounts};

/// statx `st_dev`. On btrfs there is one per subvolume.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Dev(pub u32, pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ident {
    pub dev: Dev,
    pub ino: u64,
    pub mnt: u64,
}

impl Ident {
    pub fn same_file(&self, o: &Ident) -> bool {
        self.dev == o.dev && self.ino == o.ino
    }
}

#[derive(Debug)]
pub struct Meta {
    pub id: Ident,
    pub mode: u32,
    pub uid: u32,
    pub nlink: u32,
    pub size: u64,
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
    /// Masked: `stx_attributes & stx_attributes_mask`, so a bit the
    /// filesystem does not report reads as unset rather than as garbage.
    pub attrs: StatxAttributes,
}

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;
const S_ISVTX: u32 = 0o1_000;

impl Meta {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    pub fn is_reg(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    /// `STATX_ATTR_MOUNT_ROOT`, already masked by `stx_attributes_mask`
    /// (docs/design.md §5.3): absent on a kernel that does not report it,
    /// never a false positive from an unmasked bit.
    pub fn is_mount_root(&self) -> bool {
        self.attrs.contains(StatxAttributes::MOUNT_ROOT)
    }

    pub fn sticky(&self) -> bool {
        self.mode & S_ISVTX != 0
    }
}

fn statx_mask() -> StatxFlags {
    StatxFlags::BASIC_STATS | StatxFlags::MNT_ID
}

fn to_meta(x: fs::Statx) -> Meta {
    Meta {
        id: Ident {
            dev: Dev(x.stx_dev_major, x.stx_dev_minor),
            ino: x.stx_ino,
            mnt: x.stx_mnt_id,
        },
        mode: u32::from(x.stx_mode),
        uid: x.stx_uid,
        nlink: x.stx_nlink,
        size: x.stx_size,
        mtime: (x.stx_mtime.tv_sec, x.stx_mtime.tv_nsec),
        ctime: (x.stx_ctime.tv_sec, x.stx_ctime.tv_nsec),
        attrs: x.stx_attributes & x.stx_attributes_mask,
    }
}

/// Ordinary `stat`: follows symlinks. Used for absolute, already-resolved
/// paths (e.g. checking a directory's identity after `fd_path`).
pub fn stat(p: &Path) -> io::Result<Meta> {
    let x = fs::statx(CWD, p, AtFlags::empty(), statx_mask())?;
    Ok(to_meta(x))
}

/// `lstat`: never follows a symlink in the final component (docs/design.md
/// §0.3 invariant 4 and §3).
pub fn stat_at(d: impl AsFd, n: impl Arg) -> io::Result<Meta> {
    let x = fs::statx(d, n, AtFlags::SYMLINK_NOFOLLOW, statx_mask())?;
    Ok(to_meta(x))
}

/// The identity of `fd` itself (like `fstat`), for any fd type.
pub fn ident(fd: impl AsFd) -> io::Result<Ident> {
    let x = fs::statx(fd, "", AtFlags::EMPTY_PATH, statx_mask())?;
    Ok(Ident {
        dev: Dev(x.stx_dev_major, x.stx_dev_minor),
        ino: x.stx_ino,
        mnt: x.stx_mnt_id,
    })
}

/// `readlink /proc/self/fd/N`: the canonical path to `fd` in this namespace.
pub fn fd_path(fd: impl AsFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}

/// `O_RDONLY|O_DIRECTORY|O_NOFOLLOW`: never follows a symlink named `n`.
pub fn open_dir(d: impl AsFd, n: impl Arg) -> io::Result<OwnedFd> {
    Ok(openat(
        d,
        n,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// `O_PATH|O_DIRECTORY`: follows symlinks in the parent, as `rm` does.
pub fn open_path(d: impl AsFd, n: impl Arg) -> io::Result<OwnedFd> {
    Ok(openat(
        d,
        n,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Every name in `fd` except `.` and `..`.
pub fn read_names(fd: impl AsFd) -> io::Result<Vec<OsString>> {
    let mut dir = fs::Dir::read_from(fd)?;
    let mut out = Vec::new();
    while let Some(entry) = dir.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        out.push(OsStr::from_bytes(name.to_bytes()).to_owned());
    }
    Ok(out)
}

pub fn rename_noreplace(a: impl AsFd, an: &OsStr, b: impl AsFd, bn: &OsStr) -> io::Result<()> {
    let (a, b) = (a.as_fd(), b.as_fd());
    noreplace_with(
        || Ok(renameat_with(a, an, b, bn, RenameFlags::NOREPLACE)?),
        || match stat_at(b, bn) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        },
        || Ok(renameat(a, an, b, bn)?),
    )
}

/// FUSE without RENAME2 (ntfs-3g; likely `/mnt/Mumei`) rejects the flag with
/// `EINVAL`. The target is a name rip reserved (`O_EXCL` info, under the
/// trash lock) or chose after an `lstat`. After seeing it absent, a plain
/// rename is used. A real `EINVAL` (moving a directory into itself) fails
/// again on the plain rename. A separate function so the decision is
/// unit-tested with closures, without a real unsupported filesystem.
pub(crate) fn noreplace_with(
    rename2: impl FnOnce() -> io::Result<()>,
    exists: impl FnOnce() -> io::Result<bool>,
    rename: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match rename2() {
        Err(e) if e.raw_os_error() == Some(Errno::INVAL.raw_os_error()) => {
            if exists()? {
                Err(Errno::EXIST.into())
            } else {
                rename()
            }
        }
        r => r,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Lock {
    Shared,
    Exclusive,
}

pub struct LockGuard<'a> {
    fd: BorrowedFd<'a>,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::flock(self.fd, fs::FlockOperation::Unlock);
    }
}

/// `Ok(None)`: the filesystem does not support `flock` (e.g. some FUSE
/// mounts). Tries `LOCK_NB` first; if that would block, prints the waiting
/// message once and then blocks (docs/design.md §0.2 "Waiting").
pub fn lock(dir: &OwnedFd, l: Lock) -> io::Result<Option<LockGuard<'_>>> {
    let fd = dir.as_fd();
    let (nb, blocking) = match l {
        Lock::Shared => (
            fs::FlockOperation::NonBlockingLockShared,
            fs::FlockOperation::LockShared,
        ),
        Lock::Exclusive => (
            fs::FlockOperation::NonBlockingLockExclusive,
            fs::FlockOperation::LockExclusive,
        ),
    };
    match fs::flock(fd, nb) {
        Ok(()) => return Ok(Some(LockGuard { fd })),
        Err(Errno::WOULDBLOCK) => {}
        Err(e) if flock_unsupported(e) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    eprintln!("rip: waiting for another rip");
    match fs::flock(fd, blocking) {
        Ok(()) => Ok(Some(LockGuard { fd })),
        Err(e) if flock_unsupported(e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A handful of filesystems (older FUSE backends among them) reject `flock`
/// outright rather than honoring it; treat that as "no lock available" the
/// same way the design treats missing `RENAME_NOREPLACE` support.
fn flock_unsupported(e: Errno) -> bool {
    matches!(e, Errno::NOSYS | Errno::OPNOTSUPP | Errno::INVAL)
}

/// Opens directories `a` and `b` through ONE mount that shows both, so a
/// rename between the fds cannot fail with `EXDEV` at a mount boundary. Each
/// candidate is checked, not trusted: each opened fd must have that mount's
/// id and the expected `(dev, ino)`. A covered candidate fails this check. A
/// read-only candidate is skipped, not returned: renaming through it would
/// fail with `EROFS` (or, worse, succeed and silently defeat a read-only
/// view someone set up on purpose -- docs/design.md c3), and a later
/// candidate on the same filesystem may still be writable.
pub fn route(
    ms: &Mounts,
    a: &Path,
    a_id: Ident,
    b: &Path,
    b_id: Ident,
) -> io::Result<Option<(OwnedFd, OwnedFd)>> {
    for (mnt, pa, pb) in mounts::route_candidates(ms, a_id.mnt, a, b_id.mnt, b) {
        if ms.by_id(mnt).is_some_and(|m| m.ro) {
            continue;
        }
        let (Ok(fa), Ok(fb)) = (open_path(CWD, &pa), open_path(CWD, &pb)) else {
            continue;
        };
        let ok = |fd: &OwnedFd, want: Ident| ident(fd).map(|g| g.same_file(&want) && g.mnt == mnt);
        if ok(&fa, a_id)? && ok(&fb, b_id)? {
            return Ok(Some((fa, fb)));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Debug)]
pub enum Check {
    Size,
    Removable { uid: u32 },
}

#[derive(Debug)]
pub struct Walk {
    pub size: u64,
    pub problem: Option<String>,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    is_dir: bool,
    size: u64,
    mtime: (i64, u32),
}

#[derive(Debug, Clone, Copy)]
struct PreEntry {
    dev: Dev,
    ino: u64,
    snap: Snapshot,
}

/// What the preflight walk saw (`pre`), and -- once `verify_copy` has walked
/// the finished copy -- what `cp` actually produced (`copy`). Both are keyed
/// by `manifest_key`: the entry's path relative to the tree's own top, not
/// by `(dev, ino)` alone. A bare inode number is only unique within one
/// `st_dev` (nested btrfs subvolumes routinely reuse low inode numbers,
/// docs/design.md §1), and worse, an inode that a concurrent writer renamed
/// into a path the walk visited keeps its inode number, size and mtime, so
/// an identity-only manifest would match it there even though `cp` never
/// copied it at that path. Keying by path, and requiring a verified copy at
/// that same path, closes both holes: a source entry is removable only where
/// the walk saw it AND the trash demonstrably holds a copy of it, at the
/// same relative location (docs/design.md c1).
///
/// mtime, not ctime, for the content check: unlinking one name of a
/// hard-linked inode changes the inode's ctime, so a ctime-based check would
/// report every other link in the tree as changed.
#[derive(Debug, Default)]
pub struct Manifest {
    pre: HashMap<PathBuf, PreEntry>,
    /// `None` until `verify_copy` runs; `unchanged`/`dir_ok` authorize
    /// nothing until then, so a caller cannot forget the step and silently
    /// fall back to identity-only matching.
    copy: Option<HashMap<PathBuf, Snapshot>>,
    /// Inodes already counted into `Walk::size`, so a hard-linked pair of
    /// paths is not double-counted. Unrelated to the per-path checks above.
    seen_ino: HashSet<(Dev, u64)>,
}

/// `rel` without its leading component: the operand's own top-level name on
/// the source side, or the copy box's fixed entry name (e.g. `item`) on the
/// copy side. Stripping it means the two walks key their manifests
/// identically regardless of what their respective top-level entries are
/// called.
fn manifest_key(rel: &Path) -> PathBuf {
    rel.components().skip(1).collect()
}

impl Manifest {
    /// A non-directory entry the walk recorded at `rel` may be removed: its
    /// identity and content are unchanged since the walk, AND the verified
    /// copy holds a matching leaf at the same relative path (`cp -a` keeps
    /// mtime) -- not merely some entry `cp` copied from a different path
    /// with coincidentally equal content.
    pub fn unchanged(&self, rel: &Path, m: &Meta) -> bool {
        let Some(copy) = self.copy.as_ref() else {
            return false;
        };
        let now = Snapshot {
            is_dir: false,
            size: m.size,
            mtime: m.mtime,
        };
        let pre_ok = self
            .pre
            .get(rel)
            .is_some_and(|p| p.dev == m.id.dev && p.ino == m.id.ino && p.snap == now);
        pre_ok && copy.get(rel) == Some(&now)
    }

    /// A directory the walk descended into at `rel` may be entered (and, if
    /// it ends up empty afterward, removed): same identity as the walk saw,
    /// and the verified copy has a directory at the same relative path. Its
    /// own size/mtime are not compared: a directory's mtime changes whenever
    /// a child is added or removed, which is ordinary mid-copy and is judged
    /// per child by `unchanged`/`dir_ok`, not here.
    pub fn dir_ok(&self, rel: &Path, m: &Meta) -> bool {
        let Some(copy) = self.copy.as_ref() else {
            return false;
        };
        let pre_ok = self
            .pre
            .get(rel)
            .is_some_and(|p| p.dev == m.id.dev && p.ino == m.id.ino && p.snap.is_dir);
        pre_ok && copy.get(rel).is_some_and(|c| c.is_dir)
    }

    /// Walks the copy `cp_archive` just produced (rooted at
    /// `box_fd`/`copy_name`, e.g. the staging box's `item`) and records what
    /// actually landed at each relative path, keyed the same way the source
    /// walk keys its own entries (relative to ITS OWN top), so the two line
    /// up regardless of what the copy's top-level entry is called. Until
    /// this runs, `unchanged`/`dir_ok` authorize no removal (docs/design.md
    /// c1: the source may be deleted only where the trash holds a verified
    /// copy of that exact entry).
    pub fn verify_copy(&mut self, box_fd: BorrowedFd, copy_name: &OsStr) -> io::Result<()> {
        let w = walk(box_fd, copy_name, Check::Size)?;
        self.copy = Some(
            w.manifest
                .pre
                .into_iter()
                .map(|(k, v)| (k, v.snap))
                .collect(),
        );
        Ok(())
    }
}

struct WalkFrame {
    dir: OwnedFd,
    rel: PathBuf,
    todo: Vec<OsString>,
    /// The directory's own sticky bit and owner, needed to judge whether a
    /// *child* entry found inside it belongs to someone else (docs/design.md
    /// §6.6, "a sticky directory ... contains an entry that is not ours").
    sticky: bool,
    owner: u32,
}

/// One pass over `parent/name` with `statx(AT_SYMLINK_NOFOLLOW)`, using the
/// same explicit-stack frame loop as `remove_tree` (open fds equal the tree
/// depth, not its width; never follows a symlink). `Check::Removable` also
/// records the first reason `rm` (and, since this walk gates a copy, `cp`)
/// would fail partway through, so rip refuses the item before it copies a
/// byte (docs/design.md §6.6).
pub fn walk(parent: BorrowedFd, name: &OsStr, check: Check) -> io::Result<Walk> {
    let uid = match check {
        Check::Removable { uid } => Some(uid),
        Check::Size => None,
    };
    let mut w = Walk {
        size: 0,
        problem: None,
        manifest: Manifest::default(),
    };

    let top = stat_at(parent, name)?;
    let own_mnt = top.id.mnt;

    if let Some(uid) = uid {
        removable_top_checks(parent, name, &top, uid, &mut w);
    }

    if !top.is_dir() {
        count_leaf(Path::new(name), &top, &mut w);
        return Ok(w);
    }

    let ctx = WalkCtx { own_mnt, uid };
    let mut stack: Vec<WalkFrame> = Vec::new();
    if let Some(f) = walk_open_dir(parent, name, PathBuf::from(name), &top, &ctx, &mut w) {
        stack.push(f);
    }

    while let Some(top_frame) = stack.last_mut() {
        let Some(n) = top_frame.todo.pop() else {
            stack.pop();
            continue;
        };
        let rel = top_frame.rel.join(&n);
        let (sticky, owner, dirfd) = (top_frame.sticky, top_frame.owner, top_frame.dir.as_fd());
        if let Some(f) = walk_child(dirfd, n, rel, &ctx, sticky, owner, &mut w) {
            stack.push(f);
        }
    }
    Ok(w)
}

/// Carries the two values every recursive step of the walk needs but never
/// changes, so the per-entry helpers stay under clippy's argument-count
/// lint instead of repeating `own_mnt`/`uid` in every parameter list.
struct WalkCtx {
    own_mnt: u64,
    uid: Option<u32>,
}

fn count_leaf(rel: &Path, m: &Meta, w: &mut Walk) {
    let ino_key = (m.id.dev, m.id.ino);
    // A hard-linked inode (nlink > 1) counts once: skip the size add on a
    // repeat sighting. Keyed by (dev, ino), so a same-numbered inode on a
    // different subvolume is never mistaken for a repeat sighting of this
    // one. This is separate from the per-path manifest below: two distinct
    // paths that happen to be hard links of one inode still each get their
    // own manifest entry, since removal is judged per path.
    if w.manifest.seen_ino.insert(ino_key) {
        w.size += m.size;
    }
    w.manifest.pre.insert(
        manifest_key(rel),
        PreEntry {
            dev: m.id.dev,
            ino: m.id.ino,
            snap: Snapshot {
                is_dir: false,
                size: m.size,
                mtime: m.mtime,
            },
        },
    );
}

/// Escapes a path or name for a `Walk::problem` message (docs/design.md
/// c7): every one of these embeds an entry's own name from inside the tree
/// being walked, which can be hostile, and `Display` for `Path`/`OsStr`
/// writes it raw. `problem` strings end up in put's stderr/prompt output by
/// way of `PutErr`, never a `Path::display()`.
fn show(p: impl AsRef<OsStr>) -> String {
    crate::escape(p.as_ref().as_bytes())
}

fn flag_immutable(m: &Meta, rel: &Path, w: &mut Walk) {
    if m.attrs
        .intersects(StatxAttributes::IMMUTABLE | StatxAttributes::APPEND)
    {
        w.problem
            .get_or_insert_with(|| format!("{} is immutable", show(rel)));
    }
}

/// `accessat(W_OK)` catches an immutable parent (the kernel's
/// `inode_permission` refuses `MAY_WRITE` for `IS_IMMUTABLE`) but not an
/// append-only one: `IS_APPEND` is enforced only in `may_delete`, which
/// `unlinkat`/`rmdir` hit later, after `rip` has already published a copy
/// and started deleting the source (docs/design.md c12). Checked here, on
/// the parent `Meta` the caller already fetched, so the whole tree is
/// refused up front like every other `removable_top_checks` reason.
fn parent_flags_problem(pm: &Meta) -> Option<String> {
    pm.attrs
        .intersects(StatxAttributes::IMMUTABLE | StatxAttributes::APPEND)
        .then(|| "its parent directory is immutable or append-only".to_string())
}

/// docs/design.md §6.6 bullets 1-3: unlinking the operand itself needs
/// write+exec on its parent and, if the parent is sticky, ownership of the
/// parent or the operand; the operand itself must not be immutable/append;
/// and (since this walk gates a copy, not a bare `rm`) a regular file must be
/// readable, or `cp` would fail partway through it.
fn removable_top_checks(parent: BorrowedFd<'_>, name: &OsStr, top: &Meta, uid: u32, w: &mut Walk) {
    if let Err(e) = fs::accessat(
        parent,
        ".",
        Access::WRITE_OK | Access::EXEC_OK,
        AtFlags::EACCESS,
    ) {
        let msg = if e == Errno::ROFS {
            "its filesystem is read-only".to_string()
        } else {
            format!("its parent directory: {e}")
        };
        w.problem.get_or_insert(msg);
    }
    if let Ok(pm) = stat_at(parent, ".") {
        if let Some(msg) = parent_flags_problem(&pm) {
            w.problem.get_or_insert(msg);
        }
        if pm.sticky() && pm.uid != uid && top.uid != uid {
            w.problem.get_or_insert_with(|| {
                format!("{} is in a sticky directory it does not own", show(name))
            });
        }
    }
    flag_immutable(top, Path::new(name), w);
    if top.is_reg() && fs::accessat(parent, name, Access::READ_OK, AtFlags::EACCESS).is_err() {
        w.problem
            .get_or_insert_with(|| format!("{} is not readable", show(name)));
    }
}

/// A directory found during the walk: descend into it, recording problems for
/// docs/design.md §6.6 bullets 4-6. `None` means there is nothing to push
/// (accounted for, or could not be entered).
fn walk_open_dir(
    dir: BorrowedFd<'_>,
    n: &OsStr,
    rel: PathBuf,
    m: &Meta,
    ctx: &WalkCtx,
    w: &mut Walk,
) -> Option<WalkFrame> {
    w.manifest.pre.insert(
        manifest_key(&rel),
        PreEntry {
            dev: m.id.dev,
            ino: m.id.ino,
            snap: Snapshot {
                is_dir: true,
                size: 0,
                mtime: (0, 0),
            },
        },
    );
    if m.id.mnt != ctx.own_mnt {
        if ctx.uid.is_some() {
            w.problem
                .get_or_insert_with(|| format!("{} is on another mount", show(&rel)));
        }
        return None;
    }
    let fd = match open_dir(dir, n) {
        Ok(fd) => fd,
        Err(e) => {
            if ctx.uid.is_some() {
                w.problem
                    .get_or_insert_with(|| format!("{}: {e}", show(&rel)));
            }
            return None;
        }
    };
    if !ident(&fd).is_ok_and(|i| i.same_file(&m.id) && i.mnt == ctx.own_mnt) {
        if ctx.uid.is_some() {
            w.problem
                .get_or_insert_with(|| format!("{} changed during the walk", show(&rel)));
        }
        return None;
    }
    if ctx.uid.is_some()
        && fs::accessat(
            &fd,
            ".",
            Access::READ_OK | Access::WRITE_OK | Access::EXEC_OK,
            AtFlags::EACCESS,
        )
        .is_err()
    {
        w.problem.get_or_insert_with(|| {
            format!("{} is not readable, writable and searchable", show(&rel))
        });
    }
    let todo = match read_names(&fd) {
        Ok(v) => v,
        Err(e) => {
            if ctx.uid.is_some() {
                w.problem
                    .get_or_insert_with(|| format!("{}: {e}", show(&rel)));
            }
            return None;
        }
    };
    Some(WalkFrame {
        dir: fd,
        rel,
        todo,
        sticky: m.sticky(),
        owner: m.uid,
    })
}

fn walk_child(
    dir: BorrowedFd<'_>,
    n: OsString,
    rel: PathBuf,
    ctx: &WalkCtx,
    sticky: bool,
    owner: u32,
    w: &mut Walk,
) -> Option<WalkFrame> {
    let m = match stat_at(dir, &n) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => {
            if ctx.uid.is_some() {
                w.problem
                    .get_or_insert_with(|| format!("{}: {e}", show(&rel)));
            }
            return None;
        }
    };
    if let Some(uid) = ctx.uid {
        flag_immutable(&m, &rel, w);
        if sticky && owner != uid && m.uid != uid {
            w.problem.get_or_insert_with(|| {
                format!("{} is in a sticky directory it does not own", show(&rel))
            });
        }
        if m.is_reg() && fs::accessat(dir, &n, Access::READ_OK, AtFlags::EACCESS).is_err() {
            w.problem
                .get_or_insert_with(|| format!("{} is not readable", show(&rel)));
        }
    }
    if !m.is_dir() {
        count_leaf(&rel, &m, w);
        return None;
    }
    walk_open_dir(dir, &n, rel, &m, ctx, w)
}

pub struct Remove<'a> {
    pub mnt: u64,
    pub manifest: Option<&'a Manifest>,
    pub trash: bool,
}

#[derive(Default, Debug)]
pub struct Removal {
    pub removed_any: bool,
    pub kept: Vec<(PathBuf, String)>,
}

struct RmFrame {
    dir: OwnedFd,
    name: OsString,
    rel: PathBuf,
    todo: Vec<OsString>,
}

/// Deletes `parent/name` and everything under it with fd-relative calls.
/// Never follows a symlink, never enters another mount, never stops at the
/// first error: it removes what it safely can and reports the rest. With a
/// manifest, it deletes only entries the walk saw, unchanged. `trash` allows
/// adding `u+rwx` to directories we own (read-only trees such as Go's module
/// cache); it is never used on user sources. An explicit stack of frames:
/// depth costs fds, not thread stack.
pub fn remove_tree(parent: BorrowedFd, name: &OsStr, o: &Remove) -> Removal {
    let mut r = Removal::default();
    let mut stack: Vec<RmFrame> = Vec::new();
    if let Some(f) = enter_or_unlink(parent, name.to_owned(), PathBuf::from(name), o, &mut r) {
        stack.push(f);
    }
    while let Some(top) = stack.last_mut() {
        if let Some(n) = top.todo.pop() {
            let rel = top.rel.join(&n);
            if let Some(f) = enter_or_unlink(top.dir.as_fd(), n, rel, o, &mut r) {
                stack.push(f);
            }
            continue;
        }
        let f = stack.pop().unwrap();
        let up = stack.last().map_or(parent, |p| p.dir.as_fd());
        match unlinkat(up, &f.name, AtFlags::REMOVEDIR) {
            Ok(()) => r.removed_any = true,
            Err(Errno::NOENT) => {}
            // ENOTEMPTY: something inside was kept, or a new entry appeared.
            Err(e) => r.kept.push((f.rel, e.to_string())),
        }
    }
    r
}

fn enter_or_unlink(
    dir: BorrowedFd<'_>,
    n: OsString,
    rel: PathBuf,
    o: &Remove,
    r: &mut Removal,
) -> Option<RmFrame> {
    let m = match stat_at(dir, &n) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => {
            r.kept.push((rel, e.to_string()));
            return None;
        }
    };
    if !m.is_dir() {
        if o.manifest
            .is_some_and(|mf| !mf.unchanged(&manifest_key(&rel), &m))
        {
            r.kept
                .push((rel, "changed or new since it was copied".into()));
            return None;
        }
        match unlinkat(dir, &n, AtFlags::empty()) {
            Ok(()) => r.removed_any = true,
            Err(Errno::NOENT) => {}
            Err(e) => r.kept.push((rel, e.to_string())),
        }
        return None;
    }
    if m.id.mnt != o.mnt {
        r.kept.push((rel, "a mount point; not crossing it".into()));
        return None;
    }
    if o.manifest
        .is_some_and(|mf| !mf.dir_ok(&manifest_key(&rel), &m))
    {
        r.kept.push((rel, "new since it was copied".into()));
        return None;
    }
    let opened = match open_dir(dir, &n) {
        Err(e)
            if o.trash
                && Errno::from_io_error(&e) == Some(Errno::ACCESS)
                && m.uid == getuid().as_raw() =>
        {
            chmod_via_opath(dir, &n, &m).and_then(|()| open_dir(dir, &n))
        }
        x => x,
    };
    let fd = match opened {
        Ok(fd) => fd,
        Err(e) => {
            r.kept.push((rel, e.to_string()));
            return None;
        }
    };
    if !ident(&fd).is_ok_and(|i| i.same_file(&m.id) && i.mnt == o.mnt) {
        r.kept.push((rel, "changed during deletion".into()));
        return None;
    }
    if o.trash && m.uid == getuid().as_raw() && m.mode & 0o700 != 0o700 {
        let _ = fs::fchmod(&fd, Mode::from_raw_mode(m.mode | 0o700));
    }
    match read_names(&fd) {
        Ok(todo) => Some(RmFrame {
            dir: fd,
            name: n,
            rel,
            todo,
        }),
        Err(e) => {
            r.kept.push((rel, e.to_string()));
            None
        }
    }
}

/// `fchmod` refuses an `O_PATH` fd, so this reopens the target through
/// `/proc/self/fd/N` (which does accept normal flags) after confirming, via
/// an `O_PATH` open that bypasses the very permission bits being repaired,
/// that it is still the same directory.
fn chmod_via_opath(dir: BorrowedFd<'_>, n: &OsStr, m: &Meta) -> io::Result<()> {
    let op = openat(
        dir,
        n,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let id = ident(&op)?;
    if !id.same_file(&m.id) {
        return Err(io::Error::other("changed before it could be made writable"));
    }
    let via = format!("/proc/self/fd/{}", op.as_raw_fd());
    fs::chmodat(
        CWD,
        via.as_str(),
        Mode::from_raw_mode(m.mode | 0o700),
        AtFlags::empty(),
    )?;
    Ok(())
}

/// Clears `FD_CLOEXEC` on `fd` for the lifetime of the guard, restoring it on
/// drop. Used only around a single, synchronous `cp` spawn (docs/design.md
/// §1, decision 9): rip is single-threaded and spawns nothing else while a
/// guard is alive, so no other process can inherit the fd.
struct Inherit<'a> {
    fd: BorrowedFd<'a>,
}

impl<'a> Inherit<'a> {
    fn new(fd: BorrowedFd<'a>) -> io::Result<Self> {
        fcntl_setfd(fd, FdFlags::empty())?;
        Ok(Inherit { fd })
    }
}

impl Drop for Inherit<'_> {
    fn drop(&mut self) {
        let _ = fcntl_setfd(self.fd, FdFlags::CLOEXEC);
    }
}

/// Copies `src_dir/src` to `dst_dir/dst` with GNU `cp -a -T --reflink=auto`.
/// `cp` reaches both through `/proc/self/fd`, so it copies exactly the
/// directory rip walked, even if a path changes meanwhile. `dst` is expected
/// not to exist (a fresh box), though `cp -a -T` onto an existing directory
/// merges into it rather than erroring (docs/design.md §0.3 invariant 1).
pub fn cp_archive(
    src_dir: BorrowedFd,
    src: &OsStr,
    dst_dir: BorrowedFd,
    dst: &OsStr,
) -> io::Result<()> {
    fn via(fd: BorrowedFd<'_>, n: &OsStr) -> PathBuf {
        let mut p = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        p.push(n);
        p
    }
    let _a = Inherit::new(src_dir)?;
    let _b = Inherit::new(dst_dir)?;
    let out = Command::new("cp")
        .args(["-a", "-T", "--reflink=auto", "--"])
        .arg(via(src_dir, src))
        .arg(via(dst_dir, dst))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                io::Error::other("cp not found on PATH (rip needs GNU coreutils)")
            } else {
                e
            }
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "cp failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// `mkdirat(dir, "<prefix>.<pid>.<n>", 0700)`, `n+1` on `EEXIST`; returns the
/// name and an `O_RDONLY|O_DIRECTORY` fd.
pub fn make_box(dir: BorrowedFd, prefix: &str) -> io::Result<(OsString, OwnedFd)> {
    let pid = std::process::id();
    for n in 0u64..1_000_000 {
        let name = OsString::from(format!("{prefix}.{pid}.{n}"));
        match mkdirat(dir, &name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                let fd = open_dir(dir, &name)?;
                return Ok((name, fd));
            }
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(io::Error::other("could not create a staging directory"))
}

pub fn syncfs(fd: impl AsFd) -> io::Result<()> {
    Ok(fs::syncfs(fd)?)
}

/// Sets the soft `RLIMIT_NOFILE` to the hard limit, so deep trees do not hit
/// `EMFILE` early. Best effort: a failure here is not fatal.
pub fn raise_nofile() {
    let cur = getrlimit(Resource::Nofile);
    if let Some(max) = cur.maximum {
        let _ = setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(max),
                maximum: cur.maximum,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    use super::*;

    fn root(dir: &tempfile::TempDir) -> OwnedFd {
        open_dir(CWD, dir.path()).unwrap()
    }

    /// A `Meta` for a regular file, without touching the filesystem: used to
    /// simulate an inode-number collision across two devices (as two nested
    /// btrfs subvolumes commonly produce, docs/design.md §1) without needing
    /// a real multi-device test filesystem.
    fn synthetic_file_meta(dev: Dev, ino: u64, size: u64, mtime: (i64, u32)) -> Meta {
        Meta {
            id: Ident { dev, ino, mnt: 0 },
            mode: S_IFREG,
            uid: 0,
            nlink: 1,
            size,
            mtime,
            ctime: mtime,
            attrs: StatxAttributes::empty(),
        }
    }

    // ---- noreplace_with (docs/design.md §6.3) ----

    #[test]
    fn noreplace_with_einval_and_exists_gives_eexist_without_plain_rename() {
        let result = noreplace_with(
            || Err(Errno::INVAL.into()),
            || Ok(true),
            || panic!("the plain rename must not run when the target exists"),
        );
        let err = result.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::EXIST.raw_os_error()));
    }

    #[test]
    fn noreplace_with_einval_and_absent_runs_plain_rename() {
        let result = noreplace_with(|| Err(Errno::INVAL.into()), || Ok(false), || Ok(()));
        assert!(result.is_ok());
    }

    #[test]
    fn noreplace_with_other_errors_pass_through() {
        let result = noreplace_with(
            || Err(Errno::ACCESS.into()),
            || panic!("exists() must not run for a non-EINVAL error"),
            || panic!("rename() must not run for a non-EINVAL error"),
        );
        let err = result.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::ACCESS.raw_os_error()));
    }

    #[test]
    fn noreplace_with_ok_passes_through() {
        let result = noreplace_with(
            || Ok(()),
            || panic!("exists() must not run on success"),
            || panic!("rename() must not run on success"),
        );
        assert!(result.is_ok());
    }

    // ---- walk ----

    #[test]
    fn walk_hard_links_count_once() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "d", Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("d/a"), b"12345").unwrap();
        std::fs::hard_link(dir.path().join("d/a"), dir.path().join("d/b")).unwrap();

        let w = walk(r.as_fd(), OsStr::new("d"), Check::Size).unwrap();
        assert_eq!(w.size, 5);
    }

    #[test]
    fn count_leaf_size_dedups_hardlinks_not_colliding_ino_on_a_different_dev() {
        // Two entries that share an inode number but live on different
        // devices, as two nested btrfs subvolumes commonly do for their
        // root directory and first ordinary file (docs/design.md §1
        // "Nested btrfs subvolumes count", since `cp -a` copies them). Size
        // dedup is keyed by (dev, ino) (for real hard links), so it must
        // not mistake this collision for a repeat sighting and undercount.
        let dev_a = Dev(1, 0);
        let dev_b = Dev(1, 1);
        let colliding_ino = 257;
        let file_a = synthetic_file_meta(dev_a, colliding_ino, 5, (100, 0));
        let file_b = synthetic_file_meta(dev_b, colliding_ino, 9, (200, 0));

        let mut w = Walk {
            size: 0,
            problem: None,
            manifest: Manifest::default(),
        };
        count_leaf(Path::new("top/a"), &file_a, &mut w);
        count_leaf(Path::new("top/b"), &file_b, &mut w);

        assert_eq!(
            w.size,
            5 + 9,
            "colliding ino on a different dev undercounted the walk size"
        );
        assert_eq!(w.manifest.pre.len(), 2, "both paths must be recorded");
    }

    #[test]
    fn walk_symlink_counts_its_own_length() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        let target = "this/is/the/target";
        std::os::unix::fs::symlink(target, dir.path().join("link")).unwrap();

        let w = walk(r.as_fd(), OsStr::new("link"), Check::Size).unwrap();
        assert_eq!(w.size, target.len() as u64);
    }

    #[test]
    fn parent_flags_problem_catches_append_only_which_accessat_would_miss() {
        // docs/design.md c12: chattr +a needs CAP_LINUX_IMMUTABLE, which the
        // bwrap sandbox cannot grant, so this exercises the fixed check
        // directly on a synthetic parent Meta rather than through a real
        // chattr'd directory. accessat(W_OK) alone would NOT catch this: the
        // kernel only enforces IS_APPEND in may_delete (unlink/rmdir), not
        // in the MAY_WRITE check accessat makes.
        let mut pm = synthetic_file_meta(Dev(1, 0), 99, 0, (0, 0));
        pm.mode = S_IFDIR | 0o755;
        pm.attrs = StatxAttributes::APPEND;
        assert!(parent_flags_problem(&pm).is_some());

        pm.attrs = StatxAttributes::IMMUTABLE;
        assert!(parent_flags_problem(&pm).is_some());

        pm.attrs = StatxAttributes::empty();
        assert!(parent_flags_problem(&pm).is_none());
    }

    #[test]
    fn walk_flags_0555_subdirectory_as_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        std::fs::create_dir(dir.path().join("top")).unwrap();
        std::fs::create_dir(dir.path().join("top/sub")).unwrap();
        std::fs::set_permissions(
            dir.path().join("top/sub"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let w = walk(
            r.as_fd(),
            OsStr::new("top"),
            Check::Removable {
                uid: getuid().as_raw(),
            },
        )
        .unwrap();
        assert!(w.problem.is_some());

        // Let tempdir clean itself up.
        std::fs::set_permissions(
            dir.path().join("top/sub"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    #[test]
    fn walk_flags_an_unreadable_file_as_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        std::fs::create_dir(dir.path().join("top")).unwrap();
        std::fs::write(dir.path().join("top/secret"), b"x").unwrap();
        std::fs::set_permissions(
            dir.path().join("top/secret"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let w = walk(
            r.as_fd(),
            OsStr::new("top"),
            Check::Removable {
                uid: getuid().as_raw(),
            },
        )
        .unwrap();
        assert!(w.problem.is_some());
    }

    #[test]
    fn walk_flags_a_foreign_entry_in_a_sticky_directory_as_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        std::fs::create_dir(dir.path().join("top")).unwrap();
        std::fs::set_permissions(
            dir.path().join("top"),
            std::fs::Permissions::from_mode(0o1777),
        )
        .unwrap();
        std::fs::write(dir.path().join("top/f"), b"x").unwrap();

        // No other uid is available unprivileged, so the check is exercised
        // with a `uid` that matches neither the directory's nor the entry's
        // real owner, which is exactly what "foreign" means to this check.
        let other_uid = getuid().as_raw().wrapping_add(1);
        let w = walk(
            r.as_fd(),
            OsStr::new("top"),
            Check::Removable { uid: other_uid },
        )
        .unwrap();
        assert!(w.problem.is_some());
    }

    // ---- remove_tree ----

    #[test]
    fn remove_tree_symlink_to_outside_is_removed_target_intact() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        std::fs::create_dir(dir.path().join("outside")).unwrap();
        std::fs::write(dir.path().join("outside/keepme"), b"data").unwrap();
        std::fs::create_dir(dir.path().join("victim")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside"), dir.path().join("victim/link"))
            .unwrap();

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("victim"),
            &Remove {
                mnt,
                manifest: None,
                trash: false,
            },
        );
        assert!(res.removed_any);
        assert!(res.kept.is_empty(), "{:?}", res.kept);
        assert!(matches!(stat_at(&r, "victim"), Err(e) if e.kind() == ErrorKind::NotFound));
        assert_eq!(
            std::fs::read(dir.path().join("outside/keepme")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn remove_tree_hardlinked_file_fully_removed() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "d", Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("d/a"), b"x").unwrap();
        std::fs::hard_link(dir.path().join("d/a"), dir.path().join("d/b")).unwrap();

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("d"),
            &Remove {
                mnt,
                manifest: None,
                trash: false,
            },
        );
        assert!(res.removed_any);
        assert!(res.kept.is_empty(), "{:?}", res.kept);
        assert!(matches!(stat_at(&r, "d"), Err(e) if e.kind() == ErrorKind::NotFound));
    }

    #[test]
    fn remove_tree_user_policy_keeps_0555_and_0311_directories() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        let mnt = ident(&r).unwrap().mnt;

        for (name, mode) in [("a", 0o555u32), ("b", 0o311u32)] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("f"), b"x").unwrap();
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(mode)).unwrap();

            let res = remove_tree(
                r.as_fd(),
                OsStr::new(name),
                &Remove {
                    mnt,
                    manifest: None,
                    trash: false,
                },
            );
            assert!(
                !res.kept.is_empty(),
                "expected {name} ({mode:o}) to be kept under user policy"
            );

            // Let tempdir clean itself up.
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn remove_tree_trash_policy_removes_0555_and_0311_directories() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        let mnt = ident(&r).unwrap().mnt;

        for (name, mode) in [("a", 0o555u32), ("b", 0o311u32)] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("f"), b"x").unwrap();
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(mode)).unwrap();

            let res = remove_tree(
                r.as_fd(),
                OsStr::new(name),
                &Remove {
                    mnt,
                    manifest: None,
                    trash: true,
                },
            );
            assert!(res.removed_any, "{name} ({mode:o})");
            assert!(res.kept.is_empty(), "{name} ({mode:o}): {:?}", res.kept);
            assert!(matches!(stat_at(&r, name), Err(e) if e.kind() == ErrorKind::NotFound));
        }
    }

    /// A manifest whose `copy` mirrors `pre` exactly: every entry the walk
    /// saw is treated as verified in the trash, unchanged. Simulates an
    /// ordinary, uneventful `cp` for tests that want to exercise
    /// `unchanged`/`dir_ok`'s content checks in isolation, without driving a
    /// real `cp_archive` + `verify_copy` round trip.
    fn manifest_with_copy_mirroring_pre(w: &Walk) -> Manifest {
        Manifest {
            pre: w.manifest.pre.clone(),
            copy: Some(
                w.manifest
                    .pre
                    .iter()
                    .map(|(k, v)| (k.clone(), v.snap))
                    .collect(),
            ),
            seen_ino: HashSet::new(),
        }
    }

    #[test]
    fn remove_tree_manifest_keeps_changed_and_new_entries() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "victim", Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("victim/keep.txt"), b"same").unwrap();
        std::fs::write(dir.path().join("victim/change.txt"), b"before").unwrap();

        let w = walk(
            r.as_fd(),
            OsStr::new("victim"),
            Check::Removable {
                uid: getuid().as_raw(),
            },
        )
        .unwrap();
        assert!(w.problem.is_none(), "{:?}", w.problem);
        let manifest = manifest_with_copy_mirroring_pre(&w);

        // Mutate after the walk: rewrite one file (different length, so the
        // manifest's size check alone is enough to catch it), add a new one.
        std::fs::write(dir.path().join("victim/change.txt"), b"after-a-longer-body").unwrap();
        std::fs::write(dir.path().join("victim/new.txt"), b"new").unwrap();

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("victim"),
            &Remove {
                mnt,
                manifest: Some(&manifest),
                trash: false,
            },
        );
        assert!(res.removed_any, "keep.txt should have been removed");
        let kept: Vec<String> = res
            .kept
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert!(kept.iter().any(|n| n.contains("change.txt")), "{kept:?}");
        assert!(kept.iter().any(|n| n.contains("new.txt")), "{kept:?}");
        assert!(!kept.iter().any(|n| n.contains("keep.txt")), "{kept:?}");
    }

    #[test]
    fn remove_tree_manifest_dir_does_not_match_colliding_ino_on_a_different_dev() {
        // Simulates the nested-btrfs-subvolume case (docs/design.md §1):
        // a manifest entry recorded on one device (`foreign_dev`) shares an
        // inode number with a real, *different* directory on this test's
        // own device. `victim/sub` was never actually walked, so it must
        // stay "new" and be kept, not be mistaken for the manifest's entry
        // just because their ino numbers coincide.
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "victim", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "victim/sub", Mode::from_raw_mode(0o700)).unwrap();

        let victim = stat_at(&r, "victim").unwrap();
        let sub = stat_at(&r, "victim/sub").unwrap();
        let foreign_dev = Dev(sub.id.dev.0, sub.id.dev.1.wrapping_add(1));
        let dir_snap = Snapshot {
            is_dir: true,
            size: 0,
            mtime: (0, 0),
        };
        let mut manifest = Manifest::default();
        // "victim" itself is recorded correctly, so the walk descends into
        // it; only "sub" gets the foreign-dev collision entry.
        manifest.pre.insert(
            PathBuf::new(),
            PreEntry {
                dev: victim.id.dev,
                ino: victim.id.ino,
                snap: dir_snap,
            },
        );
        manifest.pre.insert(
            PathBuf::from("sub"),
            PreEntry {
                dev: foreign_dev,
                ino: sub.id.ino,
                snap: dir_snap,
            },
        );
        manifest.copy = Some(HashMap::from([
            (PathBuf::new(), dir_snap),
            (PathBuf::from("sub"), dir_snap),
        ]));

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("victim"),
            &Remove {
                mnt,
                manifest: Some(&manifest),
                trash: false,
            },
        );

        let kept: Vec<String> = res
            .kept
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert!(kept.iter().any(|n| n.contains("sub")), "{kept:?}");
        assert!(
            stat_at(&r, "victim/sub").is_ok(),
            "victim/sub must not be removed: it never matched the manifest"
        );
    }

    // ---- c1: the manifest must key on path, and require a verified copy,
    // not just a matching (dev, ino, size, mtime) found anywhere ----

    #[test]
    fn remove_tree_verified_copy_removes_only_what_it_confirms() {
        // End-to-end happy path through the real fix: walk, `cp_archive`,
        // `verify_copy`, then `remove_tree`. Exercises the actual key
        // alignment between the source's own top-level name ("victim") and
        // the copy box's fixed top-level name ("item"), which the isolated
        // Manifest-only tests above do not cover.
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "victim", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "victim/sub", Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("victim/a"), b"AAAA").unwrap();
        std::fs::write(dir.path().join("victim/sub/b"), b"BB").unwrap();

        let mut w = walk(
            r.as_fd(),
            OsStr::new("victim"),
            Check::Removable {
                uid: getuid().as_raw(),
            },
        )
        .unwrap();
        assert!(w.problem.is_none(), "{:?}", w.problem);

        mkdirat(&r, "box", Mode::from_raw_mode(0o700)).unwrap();
        let box_fd = open_dir(&r, "box").unwrap();
        cp_archive(
            r.as_fd(),
            OsStr::new("victim"),
            box_fd.as_fd(),
            OsStr::new("item"),
        )
        .unwrap();
        w.manifest
            .verify_copy(box_fd.as_fd(), OsStr::new("item"))
            .unwrap();

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("victim"),
            &Remove {
                mnt,
                manifest: Some(&w.manifest),
                trash: false,
            },
        );
        assert!(res.removed_any);
        assert!(res.kept.is_empty(), "{:?}", res.kept);
        assert!(matches!(stat_at(&r, "victim"), Err(e) if e.kind() == ErrorKind::NotFound));
        assert_eq!(
            std::fs::read(dir.path().join("box/item/a")).unwrap(),
            b"AAAA"
        );
        assert_eq!(
            std::fs::read(dir.path().join("box/item/sub/b")).unwrap(),
            b"BB"
        );
    }

    #[test]
    fn remove_tree_keeps_an_entry_renamed_to_a_path_the_walk_never_saw() {
        // Reproduces the c1 finding: `t/b/f` is renamed to `t/a/f` between
        // the preflight walk and the copy (simulated here directly, since
        // the underlying bug is a data-modeling one, not a timing one: the
        // manifest must refuse a path it never walked, regardless of
        // whether the identity now sitting there matches some other entry
        // the walk saw). Before the fix, a manifest keyed by (dev, ino)
        // alone would match `t/a/f`'s unchanged inode/size/mtime and delete
        // it, even though `cp` copied nothing at that path.
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "t", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "t/a", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "t/b", Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("t/a/one"), b"1").unwrap();
        std::fs::write(dir.path().join("t/b/marker"), b"MARKER").unwrap();

        let mut w = walk(
            r.as_fd(),
            OsStr::new("t"),
            Check::Removable {
                uid: getuid().as_raw(),
            },
        )
        .unwrap();
        assert!(w.problem.is_none(), "{:?}", w.problem);

        // `cp` copies the tree as it stood at the walk (this test does not
        // need to race a real `cp`: what matters is that the copy has no
        // entry at "t/a/marker", exactly as a real race would leave it,
        // since neither cp's visit to `t/a` nor its visit to `t/b` ever
        // saw the file at that path).
        mkdirat(&r, "box", Mode::from_raw_mode(0o700)).unwrap();
        let box_fd = open_dir(&r, "box").unwrap();
        cp_archive(
            r.as_fd(),
            OsStr::new("t"),
            box_fd.as_fd(),
            OsStr::new("item"),
        )
        .unwrap();
        w.manifest
            .verify_copy(box_fd.as_fd(), OsStr::new("item"))
            .unwrap();

        // Now simulate the concurrent writer: the file is renamed into a
        // directory the walk already recorded, with its (dev, ino, size,
        // mtime) unchanged (a rename preserves all four).
        std::fs::rename(dir.path().join("t/b/marker"), dir.path().join("t/a/marker")).unwrap();

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("t"),
            &Remove {
                mnt,
                manifest: Some(&w.manifest),
                trash: false,
            },
        );
        assert!(
            std::fs::exists(dir.path().join("t/a/marker")).unwrap(),
            "the renamed file must survive: cp never copied it at that path"
        );
        let kept: Vec<String> = res
            .kept
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert!(kept.iter().any(|n| n.contains("marker")), "{kept:?}");
        // The untouched sibling is still removed normally.
        assert!(!std::fs::exists(dir.path().join("t/a/one")).unwrap());
    }

    #[test]
    fn remove_tree_keeps_an_entry_the_verified_copy_does_not_actually_hold() {
        // The path-keyed identity check alone is not enough (a verifier's
        // correction to the c1 finding): a file that moves away and back to
        // its original path during the copy window keeps its (dev, ino,
        // size, mtime) match at that same path, yet `cp` may never have
        // copied it there. Only a copy that `verify_copy` actually confirms
        // may authorize deletion.
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "t", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "t/zz", Mode::from_raw_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("t/zz/f"), b"VERIFY-ME").unwrap();

        let mut w = walk(
            r.as_fd(),
            OsStr::new("t"),
            Check::Removable {
                uid: getuid().as_raw(),
            },
        )
        .unwrap();
        assert!(w.problem.is_none(), "{:?}", w.problem);

        mkdirat(&r, "box", Mode::from_raw_mode(0o700)).unwrap();
        let box_fd = open_dir(&r, "box").unwrap();
        cp_archive(
            r.as_fd(),
            OsStr::new("t"),
            box_fd.as_fd(),
            OsStr::new("item"),
        )
        .unwrap();
        // Simulate the race's outcome directly: `cp` never actually copied
        // "t/zz/f" (it moved away before cp visited "zz" and back before
        // removal), so the verified copy must not hold it, even though it
        // does hold everything else and the source file itself never moved
        // in this test.
        std::fs::remove_file(dir.path().join("box/item/zz/f")).unwrap();
        w.manifest
            .verify_copy(box_fd.as_fd(), OsStr::new("item"))
            .unwrap();

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("t"),
            &Remove {
                mnt,
                manifest: Some(&w.manifest),
                trash: false,
            },
        );
        assert!(
            std::fs::exists(dir.path().join("t/zz/f")).unwrap(),
            "must survive: identity matches the walk, but the verified copy does not hold it"
        );
        let kept: Vec<String> = res
            .kept
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert!(kept.iter().any(|n| n.contains('f')), "{kept:?}");
    }

    #[test]
    fn remove_tree_5000_level_tree() {
        raise_nofile();
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        {
            let mut cur = root(&dir);
            for _ in 0..5000 {
                mkdirat(&cur, "d", Mode::from_raw_mode(0o700)).unwrap();
                cur = open_dir(&cur, "d").unwrap();
            }
        }

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("d"),
            &Remove {
                mnt,
                manifest: None,
                trash: false,
            },
        );
        assert!(res.removed_any);
        assert!(res.kept.is_empty(), "kept {} entries", res.kept.len());
        assert!(matches!(stat_at(&r, "d"), Err(e) if e.kind() == ErrorKind::NotFound));
    }

    #[test]
    fn remove_tree_500_entries_span_several_getdents_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "many", Mode::from_raw_mode(0o700)).unwrap();
        let many = dir.path().join("many");
        for i in 0..500 {
            std::fs::write(many.join(format!("f{i:04}")), b"x").unwrap();
        }

        let mnt = ident(&r).unwrap().mnt;
        let res = remove_tree(
            r.as_fd(),
            OsStr::new("many"),
            &Remove {
                mnt,
                manifest: None,
                trash: false,
            },
        );
        assert!(res.removed_any);
        assert!(res.kept.is_empty(), "kept {} entries", res.kept.len());
        assert!(matches!(stat_at(&r, "many"), Err(e) if e.kind() == ErrorKind::NotFound));
    }

    // ---- cp_archive ----

    #[test]
    fn cp_archive_through_fds() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "src", Mode::from_raw_mode(0o700)).unwrap();
        let src_fd = open_dir(&r, "src").unwrap();
        let src_path = dir.path().join("src");

        let bad_name = OsStr::from_bytes(&[0x66, 0x6f, 0x80, 0x6f]); // "fo<0x80>o"
        std::fs::write(src_path.join(bad_name), b"payload").unwrap();
        rustix::fs::mkfifoat(&src_fd, "fifo", Mode::from_raw_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("nowhere", src_path.join("dangling")).unwrap();

        // `dst` already exists with an unrelated file: `cp -a -T` must merge
        // into it rather than nesting `src` inside it or erroring (this is
        // why put's copy fallback always copies into a fresh box first).
        mkdirat(&r, "dst", Mode::from_raw_mode(0o700)).unwrap();
        let dst_fd = open_dir(&r, "dst").unwrap();
        std::fs::write(dir.path().join("dst/already-there"), b"kept").unwrap();

        cp_archive(r.as_fd(), OsStr::new("src"), r.as_fd(), OsStr::new("dst")).unwrap();
        drop((src_fd, dst_fd));

        let dst_path = dir.path().join("dst");
        assert_eq!(
            std::fs::read(dst_path.join("already-there")).unwrap(),
            b"kept"
        );
        assert_eq!(std::fs::read(dst_path.join(bad_name)).unwrap(), b"payload");
        assert!(
            std::fs::symlink_metadata(dst_path.join("fifo"))
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert_eq!(
            std::fs::read_link(dst_path.join("dangling")).unwrap(),
            Path::new("nowhere")
        );
    }

    // ---- design §15.2 U1/U2: verify before route()/mount_conflict() rely on them ----

    #[test]
    fn mnt_id_matches_mountinfo() {
        let text = std::fs::read_to_string("/proc/self/mountinfo").expect("read mountinfo");
        let want = text
            .lines()
            .find_map(|l| {
                let mut f = l.split(' ');
                let id = f.next()?;
                f.next()?; // parent
                f.next()?; // major:minor
                f.next()?; // root
                let point = f.next()?;
                (point == "/").then(|| id.parse::<u64>().ok()).flatten()
            })
            .expect("no mountinfo line for /");

        let got = stat(Path::new("/")).unwrap().id.mnt;
        assert_eq!(got, want, "stx_mnt_id must equal the mountinfo id for /");
    }

    #[test]
    fn mount_root_attr_reported() {
        let m = stat(Path::new("/")).unwrap();
        assert!(
            m.is_mount_root(),
            "STATX_ATTR_MOUNT_ROOT was not reported for /; mount_conflict would need \
             `path == own.point` alone (docs/design.md §5.3, §15.2 item 3)"
        );
    }

    // ---- lock ----

    #[test]
    fn lock_shared_twice_is_fine_and_exclusive_nb_reports_wouldblock() {
        let dir = tempfile::tempdir().unwrap();
        let fd1 = root(&dir);
        let fd2 = root(&dir);
        let fd3 = root(&dir);

        let g1 = lock(&fd1, Lock::Shared).unwrap();
        assert!(g1.is_some());
        let g2 = lock(&fd2, Lock::Shared).unwrap();
        assert!(g2.is_some());

        // A separate open file description: LOCK_EX|LOCK_NB must not block
        // while a shared lock is held elsewhere (design §13.1 "lock").
        let err = rustix::fs::flock(&fd3, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .unwrap_err();
        assert_eq!(err, Errno::WOULDBLOCK);

        drop(g1);
        drop(g2);
        rustix::fs::flock(&fd3, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    }

    #[test]
    fn lock_exclusive_blocks_until_released_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let holder = root(&dir);
        let waiter = root(&dir);
        let start = std::time::Instant::now();

        // A scoped thread, so `LockGuard<'_>` (borrowed from `holder`) never
        // needs to be `'static`.
        std::thread::scope(|s| {
            let held = lock(&holder, Lock::Exclusive).unwrap().unwrap();
            s.spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                drop(held);
            });

            // Blocks (via the LOCK_NB-then-block fallback) until the thread
            // above releases the lock, rather than failing immediately.
            let g = lock(&waiter, Lock::Exclusive).unwrap();
            assert!(g.is_some());
        });
        assert!(start.elapsed() >= std::time::Duration::from_millis(100));
    }

    // ---- route (docs/design.md §5.2) ----
    //
    // `Mounts` has no public constructor from `Mount` values directly (its
    // inner `Vec` field is private), so these go through the real mountinfo
    // text parser with one hand-written line, the same thing the fixture
    // tests in mounts.rs do with a whole captured file. That also keeps
    // these tests portable: a plain `cargo test` process cannot create a
    // second real mount of the same filesystem without a mount namespace
    // (that is what the bwrap sandbox in tests/common is for, and it is not
    // reachable from a `src/` unit test), so each test below uses exactly
    // one real mount and either a trivial same-mount candidate (the
    // "successful route" case, which self-cancels to the original paths
    // regardless of the fabricated `root`/`point` values) or a deliberately
    // wrong expected identity (the "covered candidate" case) to exercise
    // `route`'s real open-and-verify logic without needing a second one.

    /// A `Mounts` with exactly one entry: `id`, mounted at `point`, sharing
    /// an arbitrary `FsId`. `root` is never dereferenced on disk; it only
    /// has to round-trip through `inside`/`through`'s path algebra.
    fn one_mount(id: u64, point: &Path) -> Mounts {
        let text = format!(
            "{id} 1 77:77 /fake-root {} rw - tmpfs none rw\n",
            point.display()
        );
        mounts::Mounts::parse(text.as_bytes())
    }

    /// Same as `one_mount`, but marked `ro` in mountinfo field 6.
    fn one_mount_ro(id: u64, point: &Path) -> Mounts {
        let text = format!(
            "{id} 1 77:77 /fake-root {} ro - tmpfs none ro\n",
            point.display()
        );
        mounts::Mounts::parse(text.as_bytes())
    }

    #[test]
    fn route_finds_a_working_candidate_through_its_own_mount() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "a", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "b", Mode::from_raw_mode(0o700)).unwrap();

        let real_mnt = ident(&r).unwrap().mnt;
        let ms = one_mount(real_mnt, dir.path());

        let a_path = dir.path().join("a");
        let b_path = dir.path().join("b");
        let a_id = ident(open_dir(&r, "a").unwrap()).unwrap();
        let b_id = ident(open_dir(&r, "b").unwrap()).unwrap();

        let (fa, fb) = route(&ms, &a_path, a_id, &b_path, b_id)
            .unwrap()
            .expect("a same-mount candidate must be found");
        assert!(ident(&fa).unwrap().same_file(&a_id));
        assert!(ident(&fb).unwrap().same_file(&b_id));
    }

    #[test]
    fn route_skips_a_read_only_candidate_instead_of_returning_it() {
        // docs/design.md c3: renaming through a read-only mount either fails
        // with EROFS (if it is the only candidate, as here) or, worse, would
        // silently defeat the read-only view if a writable alias were tried
        // instead without checking it too. Either way, a read-only candidate
        // must never be the one `route` hands back.
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "a", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "b", Mode::from_raw_mode(0o700)).unwrap();

        let real_mnt = ident(&r).unwrap().mnt;
        let ms = one_mount_ro(real_mnt, dir.path());

        let a_path = dir.path().join("a");
        let b_path = dir.path().join("b");
        let a_id = ident(open_dir(&r, "a").unwrap()).unwrap();
        let b_id = ident(open_dir(&r, "b").unwrap()).unwrap();

        let result = route(&ms, &a_path, a_id, &b_path, b_id).unwrap();
        assert!(
            result.is_none(),
            "the only candidate is read-only; route must not return it"
        );
    }

    #[test]
    fn route_rejects_a_candidate_whose_identity_does_not_match() {
        // "Each candidate is checked, not trusted" (docs/design.md §5.2):
        // the translated path for `b` opens fine here (it is a real
        // directory), but the caller's expected identity does not match it
        // -- as if something else were mounted over it since `b_id` was
        // recorded. The one candidate must be rejected, not returned as a
        // false match.
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "a", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "real_b", Mode::from_raw_mode(0o700)).unwrap();
        mkdirat(&r, "decoy", Mode::from_raw_mode(0o700)).unwrap();

        let real_mnt = ident(&r).unwrap().mnt;
        let ms = one_mount(real_mnt, dir.path());

        let a_path = dir.path().join("a");
        let b_path = dir.path().join("real_b");
        let a_id = ident(open_dir(&r, "a").unwrap()).unwrap();
        let wrong_b_id = ident(open_dir(&r, "decoy").unwrap()).unwrap();

        let result = route(&ms, &a_path, a_id, &b_path, wrong_b_id).unwrap();
        assert!(result.is_none(), "a covered candidate must not be returned");
    }

    #[test]
    fn route_returns_none_for_an_unknown_mount_id() {
        let dir = tempfile::tempdir().unwrap();
        let r = root(&dir);
        mkdirat(&r, "a", Mode::from_raw_mode(0o700)).unwrap();

        let real_mnt = ident(&r).unwrap().mnt;
        let ms = one_mount(real_mnt, dir.path());
        let a_path = dir.path().join("a");
        let a_id = ident(open_dir(&r, "a").unwrap()).unwrap();
        let bogus = Ident {
            dev: a_id.dev,
            ino: a_id.ino,
            mnt: real_mnt.wrapping_add(999_999),
        };

        let result = route(&ms, &a_path, a_id, Path::new("/nowhere"), bogus).unwrap();
        assert!(result.is_none());
    }
}
