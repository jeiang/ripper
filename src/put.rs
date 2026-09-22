//! `rip FILE...`: operand splitting, refusals, trash placement (`choose`,
//! `choose_topdir`, `topdir_trash`), the rename path and the cross-filesystem
//! copy fallback. See docs/design.md §5 (placement for put) and §6 (put).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use jiff::civil;
use rustix::fs::{AtFlags, CWD, Mode, mkdirat, unlinkat};
use rustix::io::Errno;

use crate::info::{self, Kind};
use crate::mounts::{self, FsId, Mount, Mounts};
use crate::sys::{self, Check, Dev, Lock, Meta, Removal, Remove};
use crate::trash::{self, Trash};
use crate::{Cli, Cx, Fallback, confirm, escape, human};

// ---------------------------------------------------------------------------
// Placement (docs/design.md §5.4, pure)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Choice {
    Home,
    Topdir,
    Fallback(&'static str),
}

/// `Dev` is statx `st_dev` (one per btrfs subvolume). `FsId` is mountinfo
/// major:minor (one per filesystem). This is the only rule of the candidates
/// considered during design that never creates `.Trash-$uid` on the home
/// trash's own filesystem (docs/design.md §0.1 #2).
pub fn choose(src_dev: Dev, src_fs: FsId, home_dev: Dev, home_fs: FsId) -> Choice {
    if src_dev == home_dev {
        Choice::Home
    } else if src_fs == home_fs {
        Choice::Fallback("it is on another subvolume of the home trash's filesystem")
    } else {
        Choice::Topdir
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Entry {
    Missing,
    Dir {
        uid: u32,
        sticky: bool,
    },
    /// A symlink, a plain file, a FIFO, ...
    Other,
}

#[derive(Clone, Copy, Debug)]
pub enum Pick {
    Admin { create: bool },
    User { create: bool },
}

/// Spec methods 1 and 2. `admin_uid` is only meaningful when `admin` is a
/// sticky directory.
pub fn choose_topdir(
    admin: Entry,
    admin_uid: Entry,
    user: Entry,
    uid: u32,
) -> Result<Pick, &'static str> {
    if matches!(admin, Entry::Dir { sticky: true, .. }) {
        match admin_uid {
            Entry::Dir { uid: u, .. } if u == uid => return Ok(Pick::Admin { create: false }),
            Entry::Missing => return Ok(Pick::Admin { create: true }),
            _ => {} // invalid: fall through to method 2
        }
    }
    match user {
        Entry::Dir { uid: u, .. } if u == uid => Ok(Pick::User { create: false }),
        Entry::Missing => Ok(Pick::User { create: true }),
        Entry::Dir { .. } => Err("its .Trash-UID belongs to another user"),
        Entry::Other => Err("its .Trash-UID is not a directory"),
    }
}

fn lstat_entry(path: &Path) -> Entry {
    match sys::stat_at(CWD, path) {
        Ok(m) if m.is_dir() => Entry::Dir {
            uid: m.uid,
            sticky: m.sticky(),
        },
        Ok(_) => Entry::Other,
        Err(_) => Entry::Missing,
    }
}

fn mkdir_at_path(path: &Path) -> io::Result<()> {
    match mkdirat(CWD, path, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Opens or creates the topdir trash for `src` (already known, by `choose`,
/// to need one), reached through mount `own`. Nothing is created until the
/// mount-id and subvolume checks below pass (docs/design.md §5.4 step 1).
pub fn topdir_trash(own: &Mount, src: &Meta, uid: u32) -> Result<Trash, String> {
    let top = sys::open_path(CWD, &own.point).map_err(|e| e.to_string())?;
    let top_id = sys::ident(&top).map_err(|e| e.to_string())?;
    if top_id.mnt != own.id {
        return Err("its mount point is covered by another mount".into());
    }
    if top_id.dev != src.id.dev {
        return Err("its mount root is on another subvolume".into());
    }

    let admin_path = own.point.join(".Trash");
    let admin = lstat_entry(&admin_path);
    let admin_uid_path = admin_path.join(uid.to_string());
    let admin_uid = if matches!(admin, Entry::Dir { sticky: true, .. }) {
        lstat_entry(&admin_uid_path)
    } else {
        Entry::Missing
    };
    let user_path = own.point.join(format!(".Trash-{uid}"));
    let user = lstat_entry(&user_path);

    if matches!(admin, Entry::Dir { sticky: false, .. } | Entry::Other) {
        eprintln!(
            "rip: {} is not a sticky directory; not using it",
            admin_path.display()
        );
    }

    let mut pick = choose_topdir(admin, admin_uid, user, uid)?;
    let mut kind = match pick {
        Pick::Admin { .. } => Kind::Admin,
        Pick::User { .. } => Kind::User,
    };
    let mut trash_path = match kind {
        Kind::Admin => admin_uid_path,
        _ => user_path.clone(),
    };

    // Method 1 ("mkdirat(.Trash, uid, 0700)") can still fail even though
    // .Trash itself passed the sticky/ownership checks above (e.g. a quota
    // or an ACL); fall back to method 2 rather than failing the whole
    // operand (docs/design.md §5.4 step 2).
    if let Pick::Admin { create: true } = pick {
        if let Err(e) = mkdir_at_path(&trash_path) {
            eprintln!(
                "rip: cannot create {}: {e}; using {}",
                trash_path.display(),
                user_path.display()
            );
            pick = match user {
                Entry::Dir { uid: u, .. } if u == uid => Pick::User { create: false },
                Entry::Missing => Pick::User { create: true },
                Entry::Dir { .. } => return Err("its .Trash-UID belongs to another user".into()),
                Entry::Other => return Err("its .Trash-UID is not a directory".into()),
            };
            kind = Kind::User;
            trash_path = user_path;
        }
    }
    if let Pick::User { create: true } = pick {
        mkdir_at_path(&trash_path)
            .map_err(|e| format!("cannot create {}: {e}", trash_path.display()))?;
    }

    mkdir_at_path(&trash_path.join("files")).map_err(|e| e.to_string())?;
    mkdir_at_path(&trash_path.join("info")).map_err(|e| e.to_string())?;

    let t = trash::open_trash(&trash_path, kind, &own.point, uid)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "{}: could not open the trash directory",
                trash_path.display()
            )
        })?;

    // Step 4: the trash dir, its files/, and top must all still share the
    // source's mount and subvolume -- a race since step 1 is caught here,
    // never trusted (docs/design.md invariant 5).
    if t.id.mnt != own.id
        || t.id.dev != src.id.dev
        || t.files_id.mnt != own.id
        || t.files_id.dev != src.id.dev
    {
        return Err(format!(
            "{}: its trash directory is on another mount",
            trash_path.display()
        ));
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Session (docs/design.md §6.1)
// ---------------------------------------------------------------------------

/// One `rip FILE...` invocation: the home trash (always `trashes[0]`) plus
/// every topdir trash discovered up front or created by a later operand, one
/// `DeletionDate` shared by the whole batch, and the home trash's own
/// placement identity (`choose()`'s `home_dev`/`home_fs`).
struct Session {
    trashes: Vec<Trash>,
    home_dev: Dev,
    home_fs: FsId,
    /// The home trash's `files/`, as a path (used to route a rename through
    /// a shared mount alongside the source's own path).
    home_files: PathBuf,
    date: civil::DateTime,
}

impl Session {
    fn new(cx: &Cx) -> Result<Self, String> {
        let home_path = trash::home_path()?;
        let home = trash::open_home(&home_path, true, cx.uid)
            .map_err(|e| format!("{}: {e}", home_path.display()))?
            .ok_or_else(|| format!("{}: could not open the home trash", home_path.display()))?;
        let home_dev = home.files_id.dev;
        let home_fs = cx
            .mounts
            .by_id(home.files_id.mnt)
            .map(|m| m.fs)
            .ok_or_else(|| {
                format!(
                    "{}: its files/ mount is not in /proc/self/mountinfo",
                    home.path.display()
                )
            })?;
        let home_files = home.path.join("files");

        let mut warn = Vec::new();
        let trashes = trash::discover(Some(home), &cx.mounts, cx.uid, &mut warn);
        for w in &warn {
            eprintln!("rip: {w}");
        }

        Ok(Session {
            trashes,
            home_dev,
            home_fs,
            home_files,
            date: jiff::Zoned::now().datetime(),
        })
    }

    fn home(&self) -> &Trash {
        &self.trashes[0]
    }

    /// Adds a newly created topdir trash, so a later operand's refusal
    /// checks (`inside_trash`/`contains_trash`) see it too.
    fn add(&mut self, t: Trash) -> &Trash {
        self.trashes.push(t);
        self.trashes.last().expect("just pushed")
    }

    /// A trash dir whose own (dev, ino) matches `path` itself or one of its
    /// ancestors -- `path` is the trash dir itself, or lies inside it
    /// (docs/design.md §6.1, §5.3 "inside a trash").
    fn inside_trash(&self, path: &Path) -> Option<&Path> {
        let leaf = sys::stat_at(CWD, path).ok().map(|m| m.id);
        let ancestor_ids = path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .filter_map(|a| sys::stat(a).ok().map(|m| m.id));
        for id in leaf.into_iter().chain(ancestor_ids) {
            if let Some(t) = self.trashes.iter().find(|t| t.id.same_file(&id)) {
                return Some(&t.path);
            }
        }
        None
    }

    /// A trash dir found under `path`, comparing filesystem-internal paths
    /// (via `own`'s `FsId`) as well as the plain namespace view, so a trash
    /// reached through an alias mount (e.g. `/persist` vs `/mnt/root`) is
    /// still caught (docs/design.md §5.3, §6.1).
    fn contains_trash<'a>(&'a self, ms: &Mounts, own: &Mount, path: &Path) -> Option<&'a Path> {
        let own_inside = mounts::inside(own, path);
        self.trashes
            .iter()
            .find(|t| {
                t.path.starts_with(path)
                    || own_inside.as_deref().is_some_and(|p| {
                        ms.by_id(t.id.mnt).is_some_and(|tm| {
                            tm.fs == own.fs
                                && mounts::inside(tm, &t.path).is_some_and(|tp| tp.starts_with(p))
                        })
                    })
            })
            .map(|t| t.path.as_path())
    }
}

// ---------------------------------------------------------------------------
// Errors and the per-item result (docs/design.md §2.3, §6.1)
// ---------------------------------------------------------------------------

/// A `put_one` failure. `not_found` marks "the operand does not exist",
/// which `-f` silences (rm compatibility) instead of counting as a failure.
#[derive(Debug)]
struct PutErr {
    msg: String,
    not_found: bool,
}

impl std::fmt::Display for PutErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<&str> for PutErr {
    fn from(msg: &str) -> Self {
        PutErr {
            msg: msg.to_string(),
            not_found: false,
        }
    }
}

impl From<String> for PutErr {
    fn from(msg: String) -> Self {
        PutErr {
            msg,
            not_found: false,
        }
    }
}

impl From<io::Error> for PutErr {
    fn from(e: io::Error) -> Self {
        PutErr {
            not_found: e.kind() == io::ErrorKind::NotFound,
            msg: e.to_string(),
        }
    }
}

fn not_found(msg: &str) -> PutErr {
    PutErr {
        msg: msg.to_string(),
        not_found: true,
    }
}

/// Converts an owned `topdir_trash` error into `&'static str`, so it unifies
/// with the string-literal reasons in `put_one`'s placement match. Bounded:
/// at most one leak per operand that fails to get or create a topdir trash.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

enum Done {
    Moved(PathBuf),
    Copied(PathBuf, u64),
    Declined,
}

impl Done {
    /// `-v` output (design §2.3: goes to stdout).
    fn print(&self, arg: &Path) {
        match self {
            Done::Moved(dest) => println!("trashed '{}' -> {}", show(arg), dest.display()),
            Done::Copied(dest, size) => println!(
                "copied '{}' ({}) -> {}",
                show(arg),
                human(*size),
                dest.display()
            ),
            Done::Declined => {}
        }
    }
}

fn show(p: &Path) -> String {
    escape(p.as_os_str().as_bytes())
}

// ---------------------------------------------------------------------------
// Flow (docs/design.md §6.1)
// ---------------------------------------------------------------------------

pub fn run(cx: &Cx, cli: &Cli) -> Result<bool, String> {
    if !cli.force
        && cli.interactive_once
        && !cli.interactive
        && (cli.files.len() > 3
            || cli
                .files
                .iter()
                .any(|f| std::fs::symlink_metadata(f).is_ok_and(|m| m.is_dir())))
        && !confirm(&format!("trash {} arguments?", cli.files.len()), "-f")?
    {
        return Ok(true);
    }

    let mut s = Session::new(cx)?;
    let mut ok = true;
    for arg in &cli.files {
        match put_one(cx, cli, &mut s, arg) {
            Ok(done) => {
                if cli.verbose {
                    done.print(arg);
                }
            }
            Err(e) if cli.force && e.not_found => {}
            Err(e) => {
                eprintln!("rip: cannot trash '{}': {e}", show(arg));
                ok = false;
            }
        }
    }
    Ok(ok)
}

/// Splits `arg` (raw bytes, so a non-UTF-8 operand works) into
/// `(parent, last_component, had_trailing_slash)`. An empty operand is
/// treated as missing (`ENOENT`); `/`, `.`, `..` and `foo/.` are refused
/// (docs/design.md §6.1: `Path::file_name` would hide the `foo/.` case, so
/// this works on the raw bytes instead).
fn split_arg(arg: &Path) -> Result<(PathBuf, OsString, bool), PutErr> {
    let bytes = arg.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(not_found("No such file or directory"));
    }
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    let slash = end != bytes.len();
    if end == 0 {
        return Err("'/': refusing to trash the root directory".into());
    }
    let trimmed = &bytes[..end];
    let (parent, last) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => (&b"."[..], trimmed),
    };
    if matches!(last, b"." | b"..") {
        return Err(format!("'{}': refusing to trash '.' or '..'", escape(trimmed)).into());
    }
    let parent = if parent.is_empty() { &b"/"[..] } else { parent };
    Ok((
        PathBuf::from(OsStr::from_bytes(parent)),
        OsStr::from_bytes(last).to_owned(),
        slash,
    ))
}

fn put_one(cx: &Cx, cli: &Cli, s: &mut Session, arg: &Path) -> Result<Done, PutErr> {
    let (parent, name, slash) = split_arg(arg)?;
    let pfd = sys::open_path(CWD, &parent)?; // O_PATH|O_DIRECTORY: follows symlinks in the parent, as rm does
    let pid = sys::ident(&pfd)?;
    let dir = sys::fd_path(&pfd)?; // canonical path to the parent in this namespace
    if !sys::stat(&dir).is_ok_and(|m| m.id == pid) {
        return Err("its parent directory moved while trashing".into());
    }
    let path = dir.join(&name);
    let st = sys::stat_at(&pfd, &name)?; // lstat: a symlink is trashed as a link
    if slash && !st.is_dir() {
        return Err(if st.is_symlink() {
            "'X/' names a symbolic link; to trash the link, drop the trailing '/'"
        } else {
            "Not a directory"
        }
        .into());
    }

    let own = cx
        .mounts
        .by_id(st.id.mnt)
        .ok_or("its mount is not in /proc/self/mountinfo")?;
    // A read-only mount is refused outright, the way `rm` is (docs/design.md
    // c3): never routed around through some other, writable alias of the
    // same subvolume. The copy fallback already refuses the same way
    // (`removable_top_checks`'s accessat check), so this keeps both
    // placement paths consistent.
    if own.ro {
        return Err("its filesystem is read-only".into());
    }
    if let Some(why) = mounts::mount_conflict(&cx.mounts, own, &path, st.is_mount_root()) {
        return Err(format!("it {why}; not trashing it").into());
    }
    if let Some(t) = s.inside_trash(&path) {
        return Err(format!(
            "it is inside the trash at {}; use `rip purge` to delete it",
            t.display()
        )
        .into());
    }
    if let Some(t) = s.contains_trash(&cx.mounts, own, &path) {
        return Err(format!("it contains the trash directory {}", t.display()).into());
    }

    if cli.interactive && !cli.force && !confirm(&format!("trash '{}'?", show(arg)), "-f")? {
        return Ok(Done::Declined);
    }

    let date = s.date;
    let why = match choose(st.id.dev, own.fs, s.home_dev, s.home_fs) {
        Choice::Home => {
            match sys::route(&cx.mounts, &dir, pid, &s.home_files, s.home().files_id)? {
                Some((from, to)) => match move_in(s.home(), &from, &name, &to, &path, date) {
                    Ok(n) => return Ok(Done::Moved(s.home().path.join("files").join(n))),
                    Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
                        "the rename crossed a filesystem boundary"
                    }
                    Err(e) => return Err(e.into()),
                },
                None => "no mount shows both it and the home trash",
            }
        }
        Choice::Topdir => match topdir_trash(own, &st, cx.uid) {
            Ok(t) => {
                let t = s.add(t);
                match move_in(t, &pfd, &name, &t.files, &path, date) {
                    Ok(n) => return Ok(Done::Moved(t.path.join("files").join(n))),
                    Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
                        "the rename crossed a filesystem boundary"
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(why) => leak(why),
        },
        Choice::Fallback(why) => why,
    };
    copy_to_home(cx, cli, s, &pfd, &name, &path, &st, why)
}

// ---------------------------------------------------------------------------
// Reservation and the rename path (docs/design.md §6.3)
// ---------------------------------------------------------------------------

/// The raw bytes a `.trashinfo`'s `Path=` should hold for `path`, landing in
/// trash `t`: absolute for the home trash (written as the user reached it),
/// relative to `t.base` ($topdir) otherwise.
fn path_field(t: &Trash, path: &Path) -> Vec<u8> {
    if t.kind == Kind::Home {
        path.as_os_str().as_bytes().to_vec()
    } else {
        path.strip_prefix(&t.base)
            .unwrap_or(path)
            .as_os_str()
            .as_bytes()
            .to_vec()
    }
}

fn move_in(
    t: &Trash,
    from: &OwnedFd,
    name: &OsStr,
    to: &OwnedFd,
    path: &Path,
    date: civil::DateTime,
) -> io::Result<OsString> {
    let _lock = sys::lock(&t.dir, Lock::Shared)?; // empty cannot remove the reserved info meanwhile
    let mut r = trash::reserve(t, name, info::encode(&path_field(t, path), date))?;
    loop {
        match sys::rename_noreplace(from, name, to, r.name()) {
            Ok(()) => return Ok(r.commit()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => r.advance()?,
            Err(e) => return Err(e), // EXDEV, EACCES, EPERM (sticky), EBUSY, EINVAL: r drops, info unlinked
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-filesystem copy into the home trash (docs/design.md §6.5-§6.7)
// ---------------------------------------------------------------------------

fn removal_summary(rm: &Removal) -> String {
    rm.kept
        .iter()
        .map(|(p, why)| format!("{}: {why}", p.display()))
        .collect::<Vec<_>>()
        .join("; ")
}

#[allow(clippy::too_many_arguments)]
fn copy_to_home(
    cx: &Cx,
    cli: &Cli,
    s: &Session,
    pfd: &OwnedFd,
    name: &OsStr,
    path: &Path,
    st: &Meta,
    why: &str,
) -> Result<Done, PutErr> {
    if cx.cfg.fallback == Fallback::Refuse {
        return Err(format!(
            "no usable trash on its filesystem ({why}); left it untouched (fallback = \"refuse\" in {})",
            cx.cfg.source.display()
        )
        .into());
    }

    let mut w = sys::walk(pfd.as_fd(), name, Check::Removable { uid: cx.uid })?; // one pass, before any write
    if let Some(p) = w.problem {
        return Err(format!("{p}; not copying it").into());
    }
    if !cli.force
        && w.size > cx.cfg.copy_threshold
        && !confirm(
            &format!(
                "'{}' ({}) has no usable trash on its filesystem ({why}). Copy it into {} and delete the original?",
                path.display(),
                human(w.size),
                s.home().path.display()
            ),
            "-f",
        )?
    {
        return Ok(Done::Declined);
    }
    eprintln!(
        "rip: '{}' has no usable trash on its filesystem ({why}); copying {} into the home trash",
        path.display(),
        human(w.size)
    );

    let home = s.home();
    let _lock = sys::lock(&home.dir, Lock::Shared)?; // held for the whole copy: empty waits
    let staging = trash::staging(home)?;
    let (bx, bfd) = sys::make_box(staging.as_fd(), "put")?;
    let drop_box = |bx: &OsStr| {
        sys::remove_tree(
            staging.as_fd(),
            bx,
            &Remove {
                mnt: home.id.mnt,
                manifest: None,
                trash: true,
            },
        );
    };

    // 1. Copy. The source is only read. No info exists yet, so a long copy
    // leaves nothing dangling.
    if let Err(e) = sys::cp_archive(pfd.as_fd(), name, bfd.as_fd(), OsStr::new("item")) {
        drop_box(&bx);
        return Err(format!("copying into the home trash failed: {e}; left it untouched").into());
    }

    // 1b. Verify: walk what `cp` actually produced, so step 5 below deletes
    // a source entry only where the trash demonstrably holds a copy of it at
    // the same relative path -- not merely one whose inode, size and mtime
    // still match something the pre-copy walk saw somewhere in the tree
    // (docs/design.md c1: a concurrent rename during the copy must not
    // authorize deleting an entry `cp` never actually copied).
    if let Err(e) = w.manifest.verify_copy(bfd.as_fd(), OsStr::new("item")) {
        drop_box(&bx);
        return Err(format!("could not verify the copy: {e}; left it untouched").into());
    }

    // 2. Reserve, 3. publish with NOREPLACE.
    let mut r = match trash::reserve(
        home,
        name,
        info::encode(path.as_os_str().as_bytes(), s.date),
    ) {
        Ok(r) => r,
        Err(e) => {
            drop_box(&bx);
            return Err(e.into());
        }
    };
    loop {
        match sys::rename_noreplace(&bfd, OsStr::new("item"), &home.files, r.name()) {
            Ok(()) => break,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if let Err(e) = r.advance() {
                    drop_box(&bx);
                    return Err(e.into());
                }
            }
            Err(e) => {
                drop_box(&bx);
                return Err(e.into());
            }
        }
    }
    let n = r.commit();
    let _ = unlinkat(&staging, &bx, AtFlags::REMOVEDIR);

    // 4. The copy, its info and the rename are on disk before any source
    // byte is removed.
    sys::syncfs(&home.files)?;

    // 5. Delete only what the walk saw, unchanged, through the same parent
    // fd cp copied from.
    let rm = sys::remove_tree(
        pfd.as_fd(),
        name,
        &Remove {
            mnt: st.id.mnt,
            manifest: Some(&w.manifest),
            trash: false,
        },
    );
    if !rm.removed_any {
        // Nothing gone: no duplicate stays behind.
        trash::discard(home, &cx.mounts, &n)?;
        return Err(format!(
            "it changed while it was copied; left it untouched ({})",
            removal_summary(&rm)
        )
        .into());
    }
    if !rm.kept.is_empty() {
        return Err(format!(
            "the trash holds a complete copy ({}); these source entries were kept: {}",
            home.path.join("files").join(&n).display(),
            removal_summary(&rm)
        )
        .into());
    }
    Ok(Done::Copied(home.path.join("files").join(n), w.size))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- choose() (docs/design.md §5.4) ----

    #[test]
    fn choose_same_dev_is_home() {
        let home_dev = Dev(1, 0);
        let home_fs = FsId(8, 0);
        assert!(matches!(
            choose(home_dev, FsId(8, 1), home_dev, home_fs),
            Choice::Home
        ));
    }

    #[test]
    fn choose_same_fs_other_dev_is_fallback() {
        let home_dev = Dev(1, 0);
        let home_fs = FsId(8, 0);
        let src_dev = Dev(1, 1); // a different subvolume of the same filesystem
        assert!(matches!(
            choose(src_dev, home_fs, home_dev, home_fs),
            Choice::Fallback(_)
        ));
    }

    #[test]
    fn choose_other_fs_is_topdir() {
        let home_dev = Dev(1, 0);
        let home_fs = FsId(8, 0);
        assert!(matches!(
            choose(Dev(2, 0), FsId(9, 0), home_dev, home_fs),
            Choice::Topdir
        ));
    }

    // ---- choose_topdir() (docs/design.md §5.4, §13.1) ----

    fn is_sticky_dir(e: Entry) -> bool {
        matches!(e, Entry::Dir { sticky: true, .. })
    }

    fn owned_by(e: Entry, uid: u32) -> bool {
        matches!(e, Entry::Dir { uid: u, .. } if u == uid)
    }

    fn usable(e: Entry, uid: u32) -> bool {
        matches!(e, Entry::Missing) || owned_by(e, uid)
    }

    fn entries() -> [Entry; 6] {
        [
            Entry::Missing,
            Entry::Dir {
                uid: 1000,
                sticky: true,
            },
            Entry::Dir {
                uid: 1000,
                sticky: false,
            },
            Entry::Dir {
                uid: 2000,
                sticky: true,
            },
            Entry::Dir {
                uid: 2000,
                sticky: false,
            },
            Entry::Other,
        ]
    }

    #[test]
    fn choose_topdir_never_returns_a_directory_owned_by_another_uid() {
        let uid = 1000;
        for admin in entries() {
            for admin_uid in entries() {
                for user in entries() {
                    if let Ok(Pick::Admin { .. }) = choose_topdir(admin, admin_uid, user, uid) {
                        assert!(
                            usable(admin_uid, uid),
                            "Admin picked with an unusable admin_uid: {admin_uid:?}"
                        );
                    }
                    if let Ok(Pick::User { .. }) = choose_topdir(admin, admin_uid, user, uid) {
                        assert!(
                            usable(user, uid),
                            "User picked with an unusable user: {user:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn choose_topdir_picks_admin_only_if_sticky() {
        let uid = 1000;
        for admin in entries() {
            for admin_uid in entries() {
                for user in entries() {
                    if let Ok(Pick::Admin { .. }) = choose_topdir(admin, admin_uid, user, uid) {
                        assert!(
                            is_sticky_dir(admin),
                            "Admin picked without a sticky .Trash: {admin:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn choose_topdir_create_true_only_for_missing() {
        let uid = 1000;
        for admin in entries() {
            for admin_uid in entries() {
                for user in entries() {
                    match choose_topdir(admin, admin_uid, user, uid) {
                        Ok(Pick::Admin { create }) => {
                            assert_eq!(
                                create,
                                matches!(admin_uid, Entry::Missing),
                                "{admin_uid:?}"
                            );
                        }
                        Ok(Pick::User { create }) => {
                            assert_eq!(create, matches!(user, Entry::Missing), "{user:?}");
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }

    #[test]
    fn choose_topdir_falls_to_method_2_exactly_when_method_1_is_invalid() {
        let uid = 1000;
        for admin in entries() {
            for admin_uid in entries() {
                for user in entries() {
                    let method1_valid = is_sticky_dir(admin) && usable(admin_uid, uid);
                    let result = choose_topdir(admin, admin_uid, user, uid);
                    let picked_admin = matches!(result, Ok(Pick::Admin { .. }));
                    assert_eq!(
                        picked_admin, method1_valid,
                        "admin={admin:?} admin_uid={admin_uid:?} user={user:?} -> {result:?}"
                    );
                    if !method1_valid {
                        // Falls through to method 2's own outcome exactly.
                        match result {
                            Ok(Pick::User { .. }) => assert!(usable(user, uid)),
                            Err(_) => assert!(!usable(user, uid)),
                            Ok(Pick::Admin { .. }) => unreachable!(),
                        }
                    }
                }
            }
        }
    }

    // ---- split_arg (docs/design.md §6.1, §13.1) ----

    #[test]
    fn split_arg_root_is_refused() {
        assert!(split_arg(Path::new("/")).is_err());
        assert!(split_arg(Path::new("///")).is_err());
    }

    #[test]
    fn split_arg_dot_and_dotdot_are_refused() {
        assert!(split_arg(Path::new(".")).is_err());
        assert!(split_arg(Path::new("..")).is_err());
        assert!(split_arg(Path::new("foo/.")).is_err());
    }

    #[test]
    fn split_arg_trailing_slashes_set_the_flag() {
        let (parent, name, slash) = split_arg(Path::new("a//")).unwrap();
        assert_eq!(parent, Path::new("."));
        assert_eq!(name, OsStr::new("a"));
        assert!(slash);
    }

    #[test]
    fn split_arg_dash_is_a_plain_file() {
        let (parent, name, slash) = split_arg(Path::new("-")).unwrap();
        assert_eq!(parent, Path::new("."));
        assert_eq!(name, OsStr::new("-"));
        assert!(!slash);
    }

    #[test]
    fn split_arg_ordinary_absolute_path() {
        let (parent, name, slash) = split_arg(Path::new("/a/b/c")).unwrap();
        assert_eq!(parent, Path::new("/a/b"));
        assert_eq!(name, OsStr::new("c"));
        assert!(!slash);
    }

    #[test]
    fn split_arg_empty_is_not_found() {
        let err = split_arg(Path::new("")).unwrap_err();
        assert!(err.not_found);
    }

    #[test]
    fn split_arg_root_relative_name_has_root_parent() {
        let (parent, name, _) = split_arg(Path::new("/x")).unwrap();
        assert_eq!(parent, Path::new("/"));
        assert_eq!(name, OsStr::new("x"));
    }
}
