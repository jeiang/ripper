//! Trash-directory discovery and loading, the home trash, the `Reserved`
//! guard, `.rip-staging`, and permanent deletion (`delete_batch`/`discard`).
//! See docs/design.md §0 (invariants, especially 3, 5, 8 and 9), §4 (on-disk
//! names) and §5.4 (empty and purge crash consistency).

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::civil;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, mkdirat, openat, unlinkat};
use rustix::io::Errno;
use rustix::process::getuid;

use crate::info::{self, Kind};
use crate::mounts::{self, Mounts};
use crate::sys::{self, Ident, Lock, Remove};

/// A `.trashinfo` is capped here (docs/design.md invariant re: FIFO-safe
/// reads): a FIFO or symlink never reaches this check (it fails the
/// `S_ISREG` test first), and nothing legitimate needs a `.trashinfo` this
/// large.
const MAX_INFO_SIZE: u64 = 64 * 1024;

pub struct Trash {
    pub kind: Kind,
    /// Where it was found (Home: canonical).
    pub path: PathBuf,
    /// Relative `Path=` resolves here: `$XDG_DATA_HOME` for Home, `$topdir` otherwise.
    pub base: PathBuf,
    /// `O_RDONLY|O_DIRECTORY|O_NOFOLLOW`. `dir` is the flock target.
    pub dir: OwnedFd,
    pub files: OwnedFd,
    pub info: OwnedFd,
    pub id: Ident,
    /// First read by put.rs's home-trash routing, `s.home().files_id` (C4a).
    #[allow(dead_code)]
    pub files_id: Ident,
}

pub struct Item {
    /// Indexes the `[Trash]` slice `load` was given. First read by
    /// restore.rs's selection (`Target::Item`, C4b).
    #[allow(dead_code)]
    pub trash: usize,
    pub name: OsString,
    pub original: PathBuf,
    pub date: civil::DateTime,
    /// `files/NAME` at load time.
    pub entry: Ident,
    /// Info file identity and bytes at load time, checked again under the lock.
    pub info: (Ident, Vec<u8>),
}

pub struct Orphan {
    /// Indexes the `[Trash]` slice `load` was given. First read by empty.rs
    /// (C4c) to group a batch's doomed orphans by trash dir.
    #[allow(dead_code)]
    pub trash: usize,
    pub name: OsString,
    /// lstat ctime, local. First read by empty.rs's `select_for_empty` (C4c).
    #[allow(dead_code)]
    pub date: civil::DateTime,
    pub entry: Ident,
    /// `Some`: a malformed info exists for this entry. Its `Vec<u8>` is
    /// empty when the info was never actually read (not `S_ISREG`, or over
    /// `MAX_INFO_SIZE`: reading it would defeat the point of the type/size
    /// check), and holds the real bytes when the info parsed as a file but
    /// failed `info::parse`/`info::original`.
    pub info: Option<(Ident, Vec<u8>)>,
}

pub struct Dangling {
    /// Indexes the `[Trash]` slice `load` was given. First read by empty.rs
    /// (C4c) to group a batch's doomed dangling infos by trash dir.
    #[allow(dead_code)]
    pub trash: usize,
    pub name: OsString,
    pub info: Ident,
}

#[derive(Default)]
pub struct Contents {
    pub items: Vec<Item>,
    pub orphans: Vec<Orphan>,
    pub dangling: Vec<Dangling>,
    pub warnings: Vec<String>,
}

/// Holds a reserved `info/NAME.trashinfo`. Dropping it without `commit()`
/// unlinks the file, so every early return, error and panic rolls back
/// (docs/design.md invariant 3). First constructed by put.rs's move-in and
/// copy-fallback paths (C4a).
#[allow(dead_code)]
pub struct Reserved<'t> {
    t: &'t Trash,
    base: OsString,
    k: u64,
    name: OsString,
    text: Vec<u8>,
    live: bool,
}

#[allow(dead_code)] // first constructed and used by put.rs (C4a)
impl Reserved<'_> {
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Something took `files/NAME` after the check (`EEXIST` from the
    /// rename): release this name and claim the next one.
    pub fn advance(&mut self) -> io::Result<()> {
        self.release();
        self.claim()
    }

    /// The `files/` entry is in place: keep the info file.
    pub fn commit(mut self) -> OsString {
        self.live = false;
        std::mem::take(&mut self.name)
    }

    /// First free name: `files/NAME` absent (lstat; skips orphans too),
    /// then `info/NAME.trashinfo` made with `O_EXCL` and written.
    fn claim(&mut self) -> io::Result<()> {
        while self.k < 100_000 {
            let n = info::candidate(&self.base, self.k, info::INFO_SUFFIX.len());
            self.k += 1;
            match sys::stat_at(&self.t.files, &n) {
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let info_name = info_file_name(&n);
            let fd = match openat(
                &self.t.info,
                &info_name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(e.into()),
                Ok(fd) => fd,
            };
            let mut file = std::fs::File::from(fd);
            if let Err(e) = file.write_all(&self.text) {
                let _ = unlinkat(&self.t.info, &info_name, AtFlags::empty());
                return Err(e);
            }
            self.name = n;
            self.live = true;
            return Ok(());
        }
        Err(io::Error::other(
            "100000 trashed items already use this name",
        ))
    }

    fn release(&mut self) {
        if std::mem::take(&mut self.live) {
            let _ = unlinkat(&self.t.info, info_file_name(&self.name), AtFlags::empty());
        }
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[allow(dead_code)] // first called by put.rs's move-in and copy-fallback paths (C4a)
pub fn reserve<'t>(t: &'t Trash, base: &OsStr, text: Vec<u8>) -> io::Result<Reserved<'t>> {
    let mut r = Reserved {
        t,
        base: base.to_owned(),
        k: 0,
        name: OsString::new(),
        text,
        live: false,
    };
    r.claim()?;
    Ok(r)
}

/// `NAME.trashinfo`.
fn info_file_name(name: &OsStr) -> OsString {
    let mut n = name.to_owned();
    n.push(info::INFO_SUFFIX);
    n
}

// ---------------------------------------------------------------------------
// Discovery and the home trash
// ---------------------------------------------------------------------------

/// `ENOENT`, `EACCES`, `ENOTDIR`, `ELOOP` and a foreign owner all mean "not a
/// usable trash directory here", skipped without a warning (docs/design.md
/// §4 "Errors"). Anything else is worth telling the user about.
fn silent_skip(e: &io::Error) -> bool {
    matches!(
        Errno::from_io_error(e),
        Some(Errno::NOENT | Errno::ACCESS | Errno::NOTDIR | Errno::LOOP)
    )
}

/// `$XDG_DATA_HOME/Trash` if `XDG_DATA_HOME` is set, non-empty and absolute;
/// otherwise `$HOME/.local/share/Trash`, with `HOME` required to be
/// absolute. Reads the environment directly (this has no `Cx` to draw on),
/// matching the check `main::home_dir` already performs before any command
/// runs.
pub fn home_path() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return Ok(p.join("Trash"));
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err("HOME is not an absolute path".into());
    }
    Ok(home.join(".local/share/Trash"))
}

/// Opens (and, if `create`, first makes) the home trash at `path`
/// (`home_path()`'s result). The path is canonicalized so a symlinked
/// `Trash` is followed -- unlike a topdir trash, this one is entirely under
/// the user's own home directory tree, so following it is expected, not a
/// hazard `O_NOFOLLOW` needs to guard against. Read-only commands pass
/// `create: false` and get `Ok(None)` for a trash that does not exist yet.
pub fn open_home(path: &Path, create: bool, uid: u32) -> io::Result<Option<Trash>> {
    if create {
        ensure_home(path)?;
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if silent_skip(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let dir = match sys::open_dir(CWD, &canonical) {
        Ok(fd) => fd,
        Err(e) if silent_skip(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    if sys::stat_at(&dir, ".")?.uid != uid {
        return Ok(None);
    }
    let base = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| canonical.clone());
    // `missing_ok: true`: an impermanence setup routinely bind-mounts the
    // home trash's *parent* directory into place before `rip` ever runs
    // there, so `Trash/` itself can exist, real and correctly owned, with
    // *both* `files/` and `info/` absent -- exactly like "no home trash at
    // all" to a read-only command, not a warning-worthy oddity. Only one of
    // the two missing is never silenced by `missing_ok` (see `finish_open`).
    finish_open(dir, Kind::Home, canonical, base, true)
}

/// `create_dir_all($XDG_DATA_HOME)`, then `mkdirat` `Trash`, `files` and
/// `info` with mode 0700 (`EEXIST` ignored throughout).
fn ensure_home(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    mkdir_ignore_exists(CWD, path)?;
    let dir = sys::open_dir(CWD, path)?;
    mkdir_ignore_exists(&dir, "files")?;
    mkdir_ignore_exists(&dir, "info")?;
    Ok(())
}

fn mkdir_ignore_exists(d: impl AsFd, n: impl rustix::path::Arg) -> io::Result<()> {
    match mkdirat(d, n, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Opens a topdir trash (`Kind::Admin` or `Kind::User`) at `path`, checking
/// it the way docs/design.md §4/§0 invariant 5 require: `O_NOFOLLOW`, owner
/// `uid`, `files/`+`info/` present and on the same mount.
pub fn open_trash(path: &Path, kind: Kind, base: &Path, uid: u32) -> io::Result<Option<Trash>> {
    let dir = match sys::open_dir(CWD, path) {
        Ok(fd) => fd,
        Err(e) if silent_skip(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    if sys::stat_at(&dir, ".")?.uid != uid {
        return Ok(None);
    }
    // `missing_ok: false`: rip itself always creates a topdir trash's
    // `files/` and `info/` together (`topdir_trash`, C4a) before ever
    // returning it from discovery, so one existing without them is unusual
    // enough to warn about rather than silently skip.
    finish_open(dir, kind, path.to_path_buf(), base.to_path_buf(), false)
}

/// Shared by `open_home` and `open_trash`: given an already-opened,
/// already-owner-checked trash root, opens `files/` and `info/`
/// (`O_NOFOLLOW`; a `files -> $HOME` symlink must never let `empty` delete
/// the home directory) and requires both to share the root's mount.
/// `missing_ok` decides what *both* `files/` and `info/` being absent
/// means: `Ok(None)` (ordinary, silent -- see `open_home`'s caller) or a
/// warning-worthy `Err` (see `open_trash`'s caller, which always passes
/// `false`). Exactly one of the two missing is never silent, `missing_ok`
/// or not: that is corruption (e.g. a crash between `ensure_home()`'s two
/// `mkdirat` calls), not "no trash dir yet", and it can hide real trashed
/// data if swallowed.
fn finish_open(
    dir: OwnedFd,
    kind: Kind,
    path: PathBuf,
    base: PathBuf,
    missing_ok: bool,
) -> io::Result<Option<Trash>> {
    let id = sys::ident(&dir)?;
    let files_r = sys::open_dir(&dir, "files");
    let info_r = sys::open_dir(&dir, "info");
    let not_found =
        |r: &io::Result<OwnedFd>| matches!(r, Err(e) if e.kind() == ErrorKind::NotFound);
    if missing_ok && not_found(&files_r) && not_found(&info_r) {
        return Ok(None);
    }
    let files = match files_r {
        Ok(fd) => fd,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(io::Error::other("its files/ subdirectory is missing"));
        }
        Err(e) => return Err(e),
    };
    let info = match info_r {
        Ok(fd) => fd,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(io::Error::other("its info/ subdirectory is missing"));
        }
        Err(e) => return Err(e),
    };
    let files_id = sys::ident(&files)?;
    let info_id = sys::ident(&info)?;
    if files_id.mnt != id.mnt || info_id.mnt != id.mnt {
        return Err(io::Error::other(
            "its files/ or info/ subdirectory is on another mount",
        ));
    }
    Ok(Some(Trash {
        kind,
        path,
        base,
        dir,
        files,
        info,
        id,
        files_id,
    }))
}

/// The home trash first, then `.Trash/$uid` and `.Trash-$uid` at each mount
/// point (skipping `autofs`: a lookup inside one starts an automount).
/// Deduplicated by the trash dir's `(dev, ino)`; the first path seen wins.
#[allow(clippy::ptr_arg)]
pub fn discover(home: Option<Trash>, ms: &Mounts, uid: u32, warn: &mut Vec<String>) -> Vec<Trash> {
    let mut out: Vec<Trash> = home.into_iter().collect();
    for m in ms.iter().filter(|m| m.fstype.as_bytes() != b"autofs") {
        let admin = m.point.join(".Trash");
        if sys::stat_at(CWD, &admin).is_ok_and(|a| a.is_dir() && a.sticky()) {
            let candidate = admin.join(uid.to_string());
            let result = open_trash(&candidate, Kind::Admin, &m.point, uid);
            push_new(&mut out, result, &candidate, warn);
        }
        let user_path = m.point.join(format!(".Trash-{uid}"));
        let result = open_trash(&user_path, Kind::User, &m.point, uid);
        push_new(&mut out, result, &user_path, warn);
    }
    out
}

fn push_new(
    out: &mut Vec<Trash>,
    result: io::Result<Option<Trash>>,
    path: &Path,
    warn: &mut Vec<String>,
) {
    match result {
        Ok(Some(t)) => {
            if !out.iter().any(|o| o.id.same_file(&t.id)) {
                out.push(t);
            }
        }
        Ok(None) => {}
        Err(e) => warn.push(format!("skipping {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Re-reads `info/NAME.trashinfo` the way `load` first read it: `O_NOFOLLOW`
/// (never redirected by a symlink) and `O_NONBLOCK` (opening a FIFO for
/// reading never blocks waiting for a writer). Returns its identity, and its
/// bytes when it is a plain, small (`<= MAX_INFO_SIZE`) regular file --
/// `Vec::new()` otherwise, since reading a non-regular or oversized info
/// would defeat the point of the check. `None` means it could not be opened
/// at all (raced away, or genuinely not `S_ISREG`-openable).
fn reread_info(t: &Trash, name: &OsStr) -> Option<(Ident, Vec<u8>)> {
    let info_name = info_file_name(name);
    let fd = openat(
        &t.info,
        &info_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let id = sys::ident(&fd).ok()?;
    let mut file = std::fs::File::from(fd);
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_INFO_SIZE {
        return Some((id, Vec::new()));
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some((id, buf))
}

fn strip_info_suffix(info_name: &OsStr) -> Option<&OsStr> {
    info_name
        .as_bytes()
        .strip_suffix(info::INFO_SUFFIX.as_bytes())
        .map(OsStr::from_bytes)
}

fn local_ctime(m: &sys::Meta) -> civil::DateTime {
    let (secs, nanos) = m.ctime;
    jiff::Timestamp::new(secs, nanos as i32)
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .to_zoned(jiff::tz::TimeZone::system())
        .datetime()
}

/// Every trash dir (the home trash, opened read-only, then the topdir
/// trashes) and their contents. Discovery warnings come first in
/// `Contents::warnings`. A home trash that fails to open is a warning, the
/// same as a topdir trash in `discover`.
pub fn load_all(ms: &Mounts, uid: u32) -> Result<(Vec<Trash>, Contents), String> {
    let home_path = home_path()?;
    let mut warn = Vec::new();
    let home = open_home(&home_path, false, uid).unwrap_or_else(|e| {
        warn.push(format!("skipping {}: {e}", home_path.display()));
        None
    });
    let trashes = discover(home, ms, uid, &mut warn);
    let mut contents = load(&trashes);
    warn.append(&mut contents.warnings);
    contents.warnings = warn;
    Ok((trashes, contents))
}

pub fn load(ts: &[Trash]) -> Contents {
    let mut c = Contents::default();
    for (idx, t) in ts.iter().enumerate() {
        load_one(idx, t, &mut c);
    }
    c
}

fn load_one(idx: usize, t: &Trash, c: &mut Contents) {
    let mut paired: HashSet<OsString> = HashSet::new();
    match sys::read_names(&t.info) {
        Ok(names) => {
            for info_name in names {
                let Some(name) = strip_info_suffix(&info_name) else {
                    continue; // a stray file in info/ that is not a .trashinfo
                };
                let name = name.to_owned();
                load_info_entry(idx, t, &name, c, &mut paired);
            }
        }
        Err(e) => c
            .warnings
            .push(format!("{}: reading info/: {e}", t.path.display())),
    }

    match sys::read_names(&t.files) {
        Ok(names) => {
            for n in names {
                if paired.contains(&n) {
                    continue;
                }
                if let Ok(m) = sys::stat_at(&t.files, &n) {
                    c.orphans.push(Orphan {
                        trash: idx,
                        name: n,
                        date: local_ctime(&m),
                        entry: m.id,
                        info: None,
                    });
                }
            }
        }
        Err(e) => c
            .warnings
            .push(format!("{}: reading files/: {e}", t.path.display())),
    }
}

/// One `info/NAME.trashinfo`: decides Item / Orphan / Dangling and pushes
/// it, marking `name` as `paired` so the files/ scan does not double-count
/// it (docs/design.md §4 "Loading"). `files/NAME` is lstat'd first: whether
/// it is missing decides Dangling regardless of the info's own content or
/// type, matching what `delete_batch`'s later recheck of a Dangling entry
/// does (an lstat of the info path, not a re-read of its content).
fn load_info_entry(
    idx: usize,
    t: &Trash,
    name: &OsStr,
    c: &mut Contents,
    paired: &mut HashSet<OsString>,
) {
    let Ok(info_lstat) = sys::stat_at(&t.info, info_file_name(name)) else {
        return; // raced away since read_names
    };
    let files_meta = sys::stat_at(&t.files, name);
    match files_meta {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            c.dangling.push(Dangling {
                trash: idx,
                name: name.to_owned(),
                info: info_lstat.id,
            });
            paired.insert(name.to_owned());
            return;
        }
        Err(_) => return, // some other stat error: leave unpaired, try again next time
        Ok(_) => {}
    }
    let entry_meta = files_meta.unwrap();
    let Some((info_id, bytes)) = reread_info(t, name) else {
        return; // raced away entirely between the lstat above and this read
    };
    let parsed = (!bytes.is_empty())
        .then(|| info::parse(&bytes))
        .and_then(Result::ok)
        .and_then(|(path_bytes, date)| {
            info::original(t.kind, &t.base, &path_bytes)
                .ok()
                .map(|orig| (orig, date))
        });
    match parsed {
        Some((original, date)) => {
            c.items.push(Item {
                trash: idx,
                name: name.to_owned(),
                original,
                date,
                entry: entry_meta.id,
                info: (info_id, bytes),
            });
        }
        None => {
            c.warnings.push(format!(
                "{}: {}: malformed .trashinfo",
                t.path.display(),
                crate::escape(name.as_bytes())
            ));
            c.orphans.push(Orphan {
                trash: idx,
                name: name.to_owned(),
                date: local_ctime(&entry_meta),
                entry: entry_meta.id,
                info: Some((info_id, bytes)),
            });
        }
    }
    paired.insert(name.to_owned());
}

// ---------------------------------------------------------------------------
// Staging, discard
// ---------------------------------------------------------------------------

// First called by put.rs's copy fallback (C4a) and trash::discard/delete_batch below.
#[allow(dead_code)]
const STAGING_NAME: &str = ".rip-staging";

/// `.rip-staging` in the trash root: `mkdirat` 0700 (`EEXIST` fine), then
/// opened `O_NOFOLLOW`, checked owned by the current user and on the
/// trash's own mount (docs/design.md §4 "on-disk names"). First called by
/// put.rs's copy fallback (C4a); already used here by `discard`/`delete_batch`.
#[allow(dead_code)]
pub fn staging(t: &Trash) -> io::Result<OwnedFd> {
    mkdir_ignore_exists(&t.dir, STAGING_NAME)?;
    let fd = sys::open_dir(&t.dir, STAGING_NAME)?;
    let meta = sys::stat_at(&t.dir, STAGING_NAME)?;
    if meta.uid != getuid().as_raw() {
        return Err(io::Error::other(
            ".rip-staging is not owned by the current user",
        ));
    }
    let id = sys::ident(&fd)?;
    if id.mnt != t.id.mnt {
        return Err(io::Error::other(
            ".rip-staging is not on the trash's own mount",
        ));
    }
    Ok(fd)
}

/// Opens `t.base` (the topdir) with `O_PATH|O_DIRECTORY`, requiring its
/// `dev` to equal the trash's own -- used by restore to resolve a topdir
/// item's original path beneath it with `RESOLVE_BENEATH`. First called by
/// restore.rs's `restore_item` (C4b).
#[allow(dead_code)]
pub fn open_top(t: &Trash) -> io::Result<OwnedFd> {
    let fd = sys::open_path(CWD, &t.base)?;
    let id = sys::ident(&fd)?;
    if id.dev != t.id.dev {
        return Err(io::Error::other("its topdir is on another subvolume"));
    }
    Ok(fd)
}

/// Re-lstats `files/NAME` and re-reads `info/NAME.trashinfo`, requiring both
/// to be unchanged since `info`/`entry` were recorded (docs/design.md §5.4:
/// a restore plus a new put can reuse a name between load time and delete
/// time). Used by `delete_item` below; also first called directly by
/// restore.rs's `restore_item` (C4b).
#[allow(dead_code)]
pub fn still_same(t: &Trash, name: &OsStr, info: &(Ident, Vec<u8>), entry: Ident) -> bool {
    let Ok(cur) = sys::stat_at(&t.files, name) else {
        return false;
    };
    if !cur.id.same_file(&entry) {
        return false;
    }
    matches!(reread_info(t, name), Some((id, bytes)) if id.same_file(&info.0) && bytes == info.1)
}

// Used by `delete_orphan` below, whose own first live caller (empty.rs, C4c)
// makes this reachable too.
#[allow(dead_code)]
fn orphan_still_same(t: &Trash, o: &Orphan) -> bool {
    let Ok(cur) = sys::stat_at(&t.files, &o.name) else {
        return false;
    };
    if !cur.id.same_file(&o.entry) {
        return false;
    }
    match &o.info {
        None => sys::stat_at(&t.info, info_file_name(&o.name))
            .is_err_and(|e| e.kind() == ErrorKind::NotFound),
        Some((id, bytes)) => {
            matches!(reread_info(t, &o.name), Some((cid, cbytes)) if cid.same_file(id) && cbytes == *bytes)
        }
    }
}

// First called by restore.rs's `restore_item`, after a successful
// rename-back (C4b).
#[allow(dead_code)]
pub fn unlink_info(t: &Trash, name: &OsStr) {
    let _ = unlinkat(&t.info, info_file_name(name), AtFlags::empty());
}

/// Whether `t.files/NAME` must not be touched right now: it has become a
/// mount point, a bind source, or a directory containing one, since it was
/// trashed (docs/design.md §5.3 "Mount refusals", reused here for both
/// `delete_batch` and `discard`'s own-item cleanup). Reachable in the live
/// build only once `discard`'s own first caller (put.rs's copy-fallback
/// rollback, C4a) or `delete_batch`'s (empty.rs/restore.rs's purge, C4b/C4c)
/// lands.
#[allow(dead_code)]
fn entry_conflict(t: &Trash, ms: &Mounts, name: &OsStr) -> Option<String> {
    let meta = sys::stat_at(&t.files, name).ok()?;
    let own = ms.iter().find(|m| m.id == t.id.mnt)?;
    let path = sys::fd_path(&t.files).ok()?.join(name);
    mounts::mount_conflict(ms, own, &path, meta.is_mount_root())
}

/// Never resets across calls within this process: each tombstone continues
/// from the last number that succeeded, instead of every `tombstone()`/
/// `fresh_tombstone()` call restarting at 0 and re-colliding (`EEXIST`) with
/// every tombstone already in `.rip-staging` from earlier in the same
/// batch. A batch of N doomed entries would otherwise cost N(N+1)/2 renames,
/// all under `LOCK_EX`, blocking any concurrent put or restore on this
/// trash for the whole time (docs/design.md §5.4, invariant 9).
static NEXT_TOMBSTONE: AtomicU64 = AtomicU64::new(0);

fn next_tombstone_name(pid: u32) -> OsString {
    let n = NEXT_TOMBSTONE.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!("del.{pid}.{n}"))
}

/// `rename_noreplace(files, NAME, staging, "del.<pid>.<n>")`. See
/// `entry_conflict` above for why this is not yet reachable from `main`.
#[allow(dead_code)]
fn tombstone(t: &Trash, staging: &OwnedFd, name: &OsStr) -> io::Result<OsString> {
    let pid = std::process::id();
    for _ in 0u64..1_000_000 {
        let tomb = next_tombstone_name(pid);
        match sys::rename_noreplace(&t.files, name, staging, &tomb) {
            Ok(()) => return Ok(tomb),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not create a tombstone name"))
}

/// Renames a stray staging entry (a leftover `put.*` copy box) to a fresh
/// `del.*` tombstone name, in place, within `staging` itself: pid reuse can
/// then never let a later put's own `make_box` claim the exact name this
/// process is about to remove. First reachable once `delete_batch`'s own
/// first caller (empty.rs/restore.rs's purge, C4b/C4c) lands.
#[allow(dead_code)]
fn fresh_tombstone(staging: &OwnedFd, name: &OsStr) -> io::Result<OsString> {
    let pid = std::process::id();
    for _ in 0u64..1_000_000 {
        let tomb = next_tombstone_name(pid);
        match sys::rename_noreplace(staging, name, staging, &tomb) {
            Ok(()) => return Ok(tomb),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not create a tombstone name"))
}

/// Permanently removes one entry this process itself owns (a restored
/// item's now-unneeded trash copy, or put's own fresh copy after a failed
/// rollback): tombstone, unlink the info, `remove_tree` the tombstone.
/// Holds no lock of its own -- the caller already holds `LOCK_SH` across the
/// whole put or restore that created this entry (docs/design.md §5.4).
/// First called by put.rs's copy-fallback rollback (C4a) and restore.rs's
/// `restore_item` (C4b).
#[allow(dead_code)]
pub fn discard(t: &Trash, ms: &Mounts, name: &OsStr) -> io::Result<()> {
    if let Some(why) = entry_conflict(t, ms, name) {
        return Err(io::Error::other(format!(
            "cannot remove the trash copy: it {why}"
        )));
    }
    let staging_fd = staging(t)?;
    let tomb = tombstone(t, &staging_fd, name)?;
    let _ = unlinkat(&t.info, info_file_name(name), AtFlags::empty());
    let removal = sys::remove_tree(
        staging_fd.as_fd(),
        &tomb,
        &Remove {
            mnt: t.id.mnt,
            manifest: None,
            trash: true,
        },
    );
    if !removal.kept.is_empty() {
        return Err(io::Error::other(format!(
            "could not fully remove the discarded copy ({} entries kept)",
            removal.kept.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// delete_batch (empty, purge)
// ---------------------------------------------------------------------------

// `Orphan`/`Dangling` are first constructed by empty.rs's own `run` (C4c);
// `Item` is already exercised by this module's own tests.
#[allow(dead_code)]
pub enum Doomed<'a> {
    Item(&'a Item),
    Orphan(&'a Orphan),
    Dangling(&'a Dangling),
}

// First constructed (by `delete_batch` below) and inspected by callers once
// empty.rs/restore.rs's purge lands (C4b/C4c).
#[allow(dead_code)]
#[derive(Default)]
pub struct Report {
    pub deleted: u64,
    pub kept: Vec<(PathBuf, String)>,
}

#[allow(dead_code)] // see `Report` above
impl Report {
    fn skip(&mut self, name: &OsStr, why: &str) {
        self.kept.push((PathBuf::from(name), why.to_string()));
    }

    fn fail(mut self, t: &Trash, e: io::Error) -> Self {
        self.kept.push((t.path.clone(), e.to_string()));
        self
    }
}

/// Permanently deletes `doomed` (all in trash `t`). `LOCK_EX` is held only
/// while entries are renamed to tombstones or unlinked; the slow recursive
/// delete of each tombstone runs after the lock is released, so a
/// timer-run `empty` never blocks an interactive put or restore, and a
/// restore can never receive a directory that is mid-deletion through an
/// open fd (docs/design.md §5.4). First called by empty.rs's `run` and
/// restore.rs's `purge` (C4b/C4c); already exercised here by this module's
/// own tests.
#[allow(dead_code)]
pub fn delete_batch(t: &Trash, ms: &Mounts, doomed: &[Doomed], clean_staging: bool) -> Report {
    let mut rep = Report::default();
    let staging_fd = match staging(t) {
        Ok(s) => s,
        Err(e) => return rep.fail(t, e),
    };
    let mut tombs: Vec<OsString> = Vec::new();
    {
        let _lock = match sys::lock(&t.dir, Lock::Exclusive) {
            Ok(l) => l,
            Err(e) => return rep.fail(t, e),
        };
        for d in doomed {
            match d {
                Doomed::Item(it) => delete_item(t, ms, &staging_fd, it, &mut tombs, &mut rep),
                Doomed::Orphan(o) => delete_orphan(t, ms, &staging_fd, o, &mut tombs, &mut rep),
                Doomed::Dangling(g) => delete_dangling(t, g, &mut rep),
            }
        }
        if clean_staging {
            if let Ok(names) = sys::read_names(&staging_fd) {
                for n in names {
                    if n.as_bytes().starts_with(b"del.") {
                        tombs.push(n);
                    } else if let Ok(d) = fresh_tombstone(&staging_fd, &n) {
                        tombs.push(d);
                    }
                }
            }
        }
    } // lock released
    for n in tombs {
        let r = sys::remove_tree(
            staging_fd.as_fd(),
            &n,
            &Remove {
                mnt: t.id.mnt,
                manifest: None,
                trash: true,
            },
        );
        rep.kept.extend(r.kept);
    }
    rep
}

// Reachable once `delete_batch`'s own first caller lands (C4b/C4c).
#[allow(dead_code)]
fn delete_item(
    t: &Trash,
    ms: &Mounts,
    staging: &OwnedFd,
    it: &Item,
    tombs: &mut Vec<OsString>,
    rep: &mut Report,
) {
    if !still_same(t, &it.name, &it.info, it.entry) {
        rep.skip(&it.name, "changed since it was listed");
        return;
    }
    if let Some(why) = entry_conflict(t, ms, &it.name) {
        rep.skip(&it.name, &why);
        return;
    }
    match tombstone(t, staging, &it.name) {
        Ok(n) => {
            tombs.push(n);
            let _ = unlinkat(&t.info, info_file_name(&it.name), AtFlags::empty());
            rep.deleted += 1;
        }
        Err(e) => rep.skip(&it.name, &e.to_string()),
    }
}

// Reachable once `delete_batch`'s own first caller lands (C4b/C4c).
#[allow(dead_code)]
fn delete_orphan(
    t: &Trash,
    ms: &Mounts,
    staging: &OwnedFd,
    o: &Orphan,
    tombs: &mut Vec<OsString>,
    rep: &mut Report,
) {
    if !orphan_still_same(t, o) {
        rep.skip(&o.name, "changed since it was listed");
        return;
    }
    if let Some(why) = entry_conflict(t, ms, &o.name) {
        rep.skip(&o.name, &why);
        return;
    }
    match tombstone(t, staging, &o.name) {
        Ok(n) => {
            tombs.push(n);
            if o.info.is_some() {
                let _ = unlinkat(&t.info, info_file_name(&o.name), AtFlags::empty());
            }
            rep.deleted += 1;
        }
        Err(e) => rep.skip(&o.name, &e.to_string()),
    }
}

// Reachable once `delete_batch`'s own first caller lands (C4b/C4c).
#[allow(dead_code)]
fn delete_dangling(t: &Trash, g: &Dangling, rep: &mut Report) {
    let info_name = info_file_name(&g.name);
    let still_missing =
        sys::stat_at(&t.files, &g.name).is_err_and(|e| e.kind() == ErrorKind::NotFound);
    let same_info = sys::stat_at(&t.info, &info_name).is_ok_and(|m| m.id.same_file(&g.info));
    if still_missing && same_info {
        let _ = unlinkat(&t.info, &info_name, AtFlags::empty());
        rep.deleted += 1;
    } else {
        rep.skip(&g.name, "changed since it was listed");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trash(dir: &Path) -> Trash {
        for sub in ["files", "info"] {
            std::fs::create_dir(dir.join(sub)).unwrap();
        }
        let dir_fd = sys::open_dir(CWD, dir).unwrap();
        let files = sys::open_dir(&dir_fd, "files").unwrap();
        let info = sys::open_dir(&dir_fd, "info").unwrap();
        let id = sys::ident(&dir_fd).unwrap();
        let files_id = sys::ident(&files).unwrap();
        Trash {
            kind: Kind::User,
            path: dir.to_path_buf(),
            base: dir.to_path_buf(),
            dir: dir_fd,
            files,
            info,
            id,
            files_id,
        }
    }

    fn info_text(path_field: &str, date: &str) -> Vec<u8> {
        info::encode(path_field.as_bytes(), date.parse().unwrap())
    }

    // ---- reserve (docs/design.md §4, invariant 3) ----

    #[test]
    fn reserve_skips_an_orphan_and_an_existing_info() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());
        // "x" is already taken by an orphan (files/x with no info).
        std::fs::write(dir.path().join("files/x"), b"orphan").unwrap();
        // "x~1" is already reserved by an unrelated info file.
        std::fs::write(
            dir.path().join("info/x~1.trashinfo"),
            info_text("x~1", "2026-01-01T00:00:00"),
        )
        .unwrap();

        let r = reserve(&t, OsStr::new("x"), info_text("x", "2026-01-01T00:00:00")).unwrap();
        assert_eq!(r.name(), OsStr::new("x~2"));
        assert!(dir.path().join("info/x~2.trashinfo").is_file());
    }

    #[test]
    fn dropping_reserved_unlinks_the_info() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());
        {
            let r = reserve(&t, OsStr::new("x"), info_text("x", "2026-01-01T00:00:00")).unwrap();
            assert_eq!(r.name(), OsStr::new("x"));
            assert!(dir.path().join("info/x.trashinfo").is_file());
        }
        assert!(!dir.path().join("info/x.trashinfo").exists());
    }

    #[test]
    fn advance_releases_the_old_name_and_commit_keeps_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());
        let mut r = reserve(&t, OsStr::new("x"), info_text("x", "2026-01-01T00:00:00")).unwrap();
        // Simulate a rename EEXIST: something else took files/x meanwhile.
        std::fs::write(dir.path().join("files/x"), b"raced").unwrap();
        r.advance().unwrap();
        assert_eq!(r.name(), OsStr::new("x~1"));
        assert!(!dir.path().join("info/x.trashinfo").exists());
        let n = r.commit();
        assert_eq!(n, OsStr::new("x~1"));
        assert!(dir.path().join("info/x~1.trashinfo").is_file());
    }

    // ---- load (docs/design.md §4 "Loading") ----

    #[test]
    fn load_classifies_items_both_orphan_kinds_and_dangling() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());

        std::fs::write(dir.path().join("files/item"), b"data").unwrap();
        std::fs::write(
            dir.path().join("info/item.trashinfo"),
            info_text("item", "2026-01-01T00:00:00"),
        )
        .unwrap();

        // Orphan kind 1: a files/ entry with no paired info at all.
        std::fs::write(dir.path().join("files/no-info"), b"x").unwrap();

        // Orphan kind 2: a files/ entry whose info exists but fails to parse.
        std::fs::write(dir.path().join("files/bad-info"), b"x").unwrap();
        std::fs::write(
            dir.path().join("info/bad-info.trashinfo"),
            b"[Trash Info]\nPath=\nDeletionDate=2026-01-01T00:00:00\n",
        )
        .unwrap();

        // Dangling: info exists, files/ entry does not.
        std::fs::write(
            dir.path().join("info/gone.trashinfo"),
            info_text("gone", "2026-01-01T00:00:00"),
        )
        .unwrap();

        let c = load(std::slice::from_ref(&t));

        assert_eq!(
            c.items.len(),
            1,
            "{:?}",
            c.items.iter().map(|i| &i.name).collect::<Vec<_>>()
        );
        assert_eq!(c.items[0].name, OsStr::new("item"));
        assert_eq!(c.items[0].original, dir.path().join("item"));

        let orphan_names: Vec<&OsStr> = c.orphans.iter().map(|o| o.name.as_os_str()).collect();
        assert_eq!(c.orphans.len(), 2, "{orphan_names:?}");
        assert!(orphan_names.contains(&OsStr::new("no-info")));
        assert!(orphan_names.contains(&OsStr::new("bad-info")));

        assert_eq!(c.dangling.len(), 1);
        assert_eq!(c.dangling[0].name, OsStr::new("gone"));
    }

    #[test]
    fn load_a_fifo_info_does_not_hang_and_becomes_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());
        std::fs::write(dir.path().join("files/f"), b"x").unwrap();
        rustix::fs::mkfifoat(&t.info, "f.trashinfo", Mode::from_raw_mode(0o600)).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let c = load(std::slice::from_ref(&t));
                let _ = tx.send(c.orphans.len());
            });
            let got = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("load() hung reading a FIFO .trashinfo");
            assert_eq!(got, 1);
        });
    }

    // ---- delete_batch (docs/design.md §5.4) ----

    #[test]
    fn delete_batch_skips_reused_name() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());
        std::fs::write(dir.path().join("files/x"), b"old").unwrap();
        std::fs::write(
            dir.path().join("info/x.trashinfo"),
            info_text("x", "2026-01-01T00:00:00"),
        )
        .unwrap();

        let c = load(std::slice::from_ref(&t));
        assert_eq!(c.items.len(), 1);
        let old_item = &c.items[0];

        // Restore the old X by hand (not through rip): it leaves files/.
        std::fs::remove_file(dir.path().join("files/x")).unwrap();
        unlinkat(&t.info, "x.trashinfo", AtFlags::empty()).unwrap();

        // A new X lands under the same name (a different inode).
        std::fs::write(dir.path().join("files/x"), b"new").unwrap();
        std::fs::write(
            dir.path().join("info/x.trashinfo"),
            info_text("x", "2026-02-02T00:00:00"),
        )
        .unwrap();

        let ms = Mounts::parse(b"");
        let report = delete_batch(&t, &ms, &[Doomed::Item(old_item)], false);

        assert_eq!(report.deleted, 0);
        assert_eq!(report.kept.len(), 1, "{:?}", report.kept);
        assert_eq!(std::fs::read(dir.path().join("files/x")).unwrap(), b"new");
        assert!(dir.path().join("info/x.trashinfo").is_file());
    }

    #[test]
    fn delete_batch_cleans_stale_staging_boxes() {
        let dir = tempfile::tempdir().unwrap();
        let t = make_trash(dir.path());
        let staging_fd = staging(&t).unwrap();
        let (box_name, box_fd) = sys::make_box(staging_fd.as_fd(), "put").unwrap();
        std::fs::write(
            dir.path()
                .join(STAGING_NAME)
                .join(&box_name)
                .join("leftover"),
            b"x",
        )
        .unwrap();
        drop(box_fd);

        let ms = Mounts::parse(b"");
        let report = delete_batch(&t, &ms, &[], true);
        assert!(report.kept.is_empty(), "{:?}", report.kept);

        let remaining = sys::read_names(staging(&t).unwrap()).unwrap();
        assert!(remaining.is_empty(), "{remaining:?}");
    }
}
