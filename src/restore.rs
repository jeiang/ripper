//! `list`, selection (shared by `restore` and `purge`), the fzf picker,
//! `restore`, `undo` and `purge`. See docs/design.md §7 (restore, undo, and
//! purge selection) and §9 (fzf integration).

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use jiff::civil;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, ResolveFlags, mkdirat, openat2, unlinkat};
use rustix::io::{Errno, fcntl_dupfd_cloexec};

use crate::info::{self, Kind};
use crate::sys::{self, Check, Lock, Remove};
use crate::trash::{self, Contents, Doomed, Item, Orphan, Trash};
use crate::{Cx, confirm, escape, human};

pub fn list(cx: &Cx, all: bool, null: bool) -> Result<bool, String> {
    let (_, contents) = trash::load_all(&cx.mounts, cx.uid)?;
    for w in &contents.warnings {
        eprintln!("rip: {w}");
    }

    let mut items: Vec<&Item> = contents
        .items
        .iter()
        .filter(|it| all || it.original.starts_with(&cx.cwd))
        .collect();
    items.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| a.original.cmp(&b.original))
    });

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let term = out.is_terminal();
    match write_items(&mut out, &items, &cx.cwd, null, term) {
        Ok(()) => Ok(true),
        // A reader that closed early (e.g. `rip list | head`) is success,
        // not a failure (design §2.2 "BrokenPipe on stdout in list counts
        // as success").
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e.to_string()),
    }
}

/// `original`, relative to `cwd` when it lies under `cwd`, absolute
/// otherwise (design §11).
fn display_path(cwd: &Path, original: &Path) -> PathBuf {
    original
        .strip_prefix(cwd)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| original.to_path_buf())
}

/// Writes `-0`'s `DATE<TAB>PATH<NUL>` records, or plain `DATE  PATH\n`
/// lines, oldest first (the order `items` is already sorted in). PATH is
/// written as raw bytes throughout, except that it is passed through
/// `escape()` for a plain listing to an actual terminal, so a hostile name
/// on a shared filesystem cannot inject terminal escape sequences (design
/// §2.4, §11).
fn write_items(
    out: &mut impl Write,
    items: &[&Item],
    cwd: &Path,
    null: bool,
    term: bool,
) -> io::Result<()> {
    for it in items {
        let path = display_path(cwd, &it.original);
        let date = it.date.strftime("%Y-%m-%d %H:%M:%S");
        if null {
            write!(out, "{date}\t")?;
            out.write_all(path.as_os_str().as_bytes())?;
            out.write_all(b"\0")?;
        } else if term {
            writeln!(out, "{date}  {}", escape(path.as_os_str().as_bytes()))?;
        } else {
            write!(out, "{date}  ")?;
            out.write_all(path.as_os_str().as_bytes())?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn print_warnings(warnings: &[String]) {
    for w in warnings {
        eprintln!("rip: {w}");
    }
}

/// Writes a restored path to stdout, relative to `cwd` when it lies under
/// it (the same display rule as `list`, design §7.2 "Restored paths print
/// to stdout, relative to cwd where possible"). Escaped only when stdout is
/// an actual terminal, matching `list`'s own escaping rule (design §2.4).
fn print_path(cwd: &Path, path: &Path) {
    let rel = display_path(cwd, path);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if out.is_terminal() {
        let _ = writeln!(out, "{}", escape(rel.as_os_str().as_bytes()));
    } else {
        let _ = out.write_all(rel.as_os_str().as_bytes());
        let _ = out.write_all(b"\n");
    }
}

// ---------------------------------------------------------------------------
// Selection (design §7.1), shared by restore and purge
// ---------------------------------------------------------------------------

/// A selected trash entry: an `Item` for restore or purge, or an `Orphan`
/// (reachable only by its trash path, and only when `purge` allows it).
enum Target<'a> {
    Item(&'a Item),
    Orphan(&'a Orphan),
}

fn scope<'a>(items: &'a [Item], cwd: &Path, all: bool) -> Vec<&'a Item> {
    items
        .iter()
        .filter(|it| all || it.original.starts_with(cwd))
        .collect()
}

/// Collapses a target selected more than once (e.g. the same trash entry
/// named by two different alias paths) to its first occurrence. `(trash
/// index, entry name)` identifies a `files/` entry uniquely, regardless of
/// whether it was reached as an `Item` or an `Orphan`.
fn dedup(targets: &mut Vec<Target<'_>>) {
    let mut seen: HashSet<(usize, OsString)> = HashSet::new();
    targets.retain(|t| {
        let key = match t {
            Target::Item(it) => (it.trash, it.name.clone()),
            Target::Orphan(o) => (o.trash, o.name.clone()),
        };
        seen.insert(key)
    });
}

/// Resolves a `restore`/`purge` PATH argument to an absolute path
/// comparable with a trashed item's `original`, or with a trash entry's
/// on-disk location. The target itself may no longer exist as such (an
/// original path was just trashed; a trash path names something that may be
/// a symlink), so only the PARENT is canonicalized (following any symlinks
/// in it, which also collapses a bind-mount alias to whatever `original`
/// was written against); the leaf name is then joined lexically. When even
/// the parent does not exist, the whole path is normalized lexically
/// instead (design §7.1 "resolve every PATH before any change").
fn resolve(cwd: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(leaf)) if !parent.as_os_str().is_empty() => {
            match std::fs::canonicalize(parent) {
                Ok(canon) => canon.join(leaf),
                Err(_) => lexical_normalize(&joined),
            }
        }
        _ => lexical_normalize(&joined),
    }
}

/// Collapses `.` and `..` components without touching the filesystem.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `abs` names a trash entry directly: its parent (or an ancestor)
/// is some trash's `files/` directory. A direct child (`files/NAME`) names
/// that entry; anything deeper (`files/NAME/sub`) is refused, since only
/// the whole item can be restored or purged (design §7.1 "Trash paths").
fn by_trash_path<'a>(
    ts: &'a [Trash],
    c: &'a Contents,
    abs: &Path,
    orphans_ok: bool,
    verb: &str,
) -> Result<Option<Target<'a>>, String> {
    let ancestors: Vec<&Path> = abs
        .ancestors()
        .skip(1)
        .take_while(|p| !p.as_os_str().is_empty())
        .collect();
    for (depth, anc) in ancestors.iter().enumerate() {
        let Ok(meta) = sys::stat(anc) else {
            continue;
        };
        let Some(idx) = ts.iter().position(|t| t.files_id.same_file(&meta.id)) else {
            continue;
        };
        if depth > 0 {
            let item_name = ancestors[depth - 1].file_name().unwrap_or(OsStr::new(""));
            return Err(format!(
                "{}: {verb} the whole item '{}'",
                abs.display(),
                escape(item_name.as_bytes())
            ));
        }
        let Some(name) = abs.file_name() else {
            return Ok(None);
        };
        if let Some(it) = c
            .items
            .iter()
            .find(|i| i.trash == idx && i.name.as_os_str() == name)
        {
            return Ok(Some(Target::Item(it)));
        }
        if let Some(o) = c
            .orphans
            .iter()
            .find(|o| o.trash == idx && o.name.as_os_str() == name)
        {
            return if orphans_ok {
                Ok(Some(Target::Orphan(o)))
            } else {
                Err(format!(
                    "{}: no valid .trashinfo (try `rip purge`)",
                    abs.display()
                ))
            };
        }
        return Ok(None);
    }
    Ok(None)
}

/// One line per variant, plus a hint, for the no-terminal case (design §7.1
/// "several variants ... without a terminal fail and list DATE and
/// TRASHPATH per variant with a hint").
fn variants_message(ts: &[Trash], abs: &Path, matches: &[&Item]) -> String {
    let mut msg = format!(
        "{} names {} trashed items; pass one of these trash paths instead:",
        abs.display(),
        matches.len()
    );
    for it in matches {
        let trash_path = ts[it.trash].path.join("files").join(&it.name);
        msg.push_str(&format!(
            "\n  {}  {}",
            it.date.strftime("%Y-%m-%d %H:%M:%S"),
            trash_path.display()
        ));
    }
    msg
}

/// Selects the items or orphans `restore`/`purge` act on. With no `paths`,
/// opens the fzf picker over items under `cwd` (or every item, with
/// `all`). Otherwise resolves every `PATH` first -- any error aborts before
/// any change is made -- matching it to a trash path, or to items sharing
/// that original path (one restores or purges it directly; several open a
/// picker limited to them, or list them with a no-terminal error).
fn select<'a>(
    cx: &Cx,
    ts: &'a [Trash],
    c: &'a Contents,
    paths: &[PathBuf],
    all: bool,
    verb: &str,
    orphans_ok: bool,
) -> Result<Vec<Target<'a>>, String> {
    let mut out: Vec<Target<'a>> = Vec::new();
    if paths.is_empty() {
        let pool = scope(&c.items, &cx.cwd, all);
        if pool.is_empty() {
            return Err(format!(
                "nothing trashed under {} (try --all)",
                cx.cwd.display()
            ));
        }
        out.extend(pick(&pool, verb, &cx.cwd)?.into_iter().map(Target::Item));
    } else {
        for p in paths {
            let abs = resolve(&cx.cwd, p);
            if let Some(t) = by_trash_path(ts, c, &abs, orphans_ok, verb)? {
                out.push(t);
                continue;
            }
            let matches: Vec<&Item> = c.items.iter().filter(|i| i.original == abs).collect();
            match matches.len() {
                0 => {
                    return Err(format!(
                        "nothing in the trash has the original path {}",
                        abs.display()
                    ));
                }
                1 => out.push(Target::Item(matches[0])),
                _ if io::stdin().is_terminal() => {
                    out.extend(pick(&matches, verb, &cx.cwd)?.into_iter().map(Target::Item));
                }
                _ => return Err(variants_message(ts, &abs, &matches)),
            }
        }
    }
    dedup(&mut out);
    Ok(out)
}

// ---------------------------------------------------------------------------
// fzf integration (design §9)
// ---------------------------------------------------------------------------

fn fzf_args(verb: &str) -> Vec<String> {
    [
        "--multi",
        "--read0",
        "--print0",
        "--delimiter=\t",
        "--with-nth=2..",
        "--tiebreak=index",
    ]
    .map(String::from)
    .into_iter()
    .chain([format!("--prompt={verb}> ")])
    .collect()
}

/// One `INDEX<TAB>DATE<TAB>PATH<NUL>` record (design §9). The display path
/// is escaped, so no record holds a raw tab or newline in PATH; a selection
/// is mapped back only by the leading index, never by parsing PATH.
fn fzf_record(index: usize, it: &Item, cwd: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    write!(buf, "{index}\t{}\t", it.date.strftime("%Y-%m-%d %H:%M:%S")).unwrap();
    buf.extend_from_slice(
        escape(display_path(cwd, &it.original).as_os_str().as_bytes()).as_bytes(),
    );
    buf.push(0);
    buf
}

/// Decodes fzf's `--print0` output back to indices into the picked pool.
/// Every record's leading field must parse as a `usize` strictly less than
/// `len`; anything else (out of range, non-numeric, non-UTF-8) is an error,
/// since it means fzf's own output no longer matches what rip sent it.
/// Pure, so this is unit-tested directly.
fn decode_fzf_output(stdout: &[u8], len: usize) -> Result<Vec<usize>, String> {
    stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            let field = r.split(|&b| b == b'\t').next().unwrap_or(r);
            std::str::from_utf8(field)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&i| i < len)
                .ok_or_else(|| "fzf returned an unknown record".to_string())
        })
        .collect()
}

fn pick<'a>(items: &[&'a Item], verb: &str, cwd: &Path) -> Result<Vec<&'a Item>, String> {
    if !io::stdin().is_terminal() {
        return Err("the picker needs a terminal; pass paths instead".into());
    }
    let mut child = Command::new("fzf")
        .args(fzf_args(verb))
        // The output format must not change under the person's own fzf
        // config (design §9).
        .env_remove("FZF_DEFAULT_OPTS")
        .env_remove("FZF_DEFAULT_OPTS_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                "fzf is not on PATH; install fzf or pass paths".to_string()
            } else {
                format!("cannot run fzf: {e}")
            }
        })?;
    let mut input = Vec::new();
    for (i, it) in items.iter().enumerate() {
        input.extend_from_slice(&fzf_record(i, it, cwd));
    }
    // Dropping the piped stdin here closes it, giving fzf EOF; a BrokenPipe
    // (the person left the picker early) is not an error.
    let _ = child
        .stdin
        .take()
        .expect("fzf's stdin is piped")
        .write_all(&input);
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    match out.status.code() {
        Some(0) => {}
        // No match, or cancelled: nothing happens, exit 0 (design §9).
        Some(1) | Some(130) => return Ok(Vec::new()),
        _ => return Err("fzf failed".into()),
    }
    let idxs = decode_fzf_output(&out.stdout, items.len())?;
    Ok(idxs.into_iter().map(|i| items[i]).collect())
}

// ---------------------------------------------------------------------------
// Restoring one item (design §7.2)
// ---------------------------------------------------------------------------

/// `restore_item`'s failure. `Conflict` is specifically "the destination
/// already exists"; `undo` (which never takes `--rename`) turns that one
/// into a `rip restore --rename <trash path>` hint. Everything else is
/// `Other`.
enum RestoreError {
    Conflict(String),
    Other(String),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreError::Conflict(m) | RestoreError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl From<io::Error> for RestoreError {
    fn from(e: io::Error) -> Self {
        RestoreError::Other(e.to_string())
    }
}

/// Resolves each prefix of `rel` under `base`, creating any missing
/// directory component (mode 0o777, subject to umask). `beneath` requires
/// every step to stay under `base` and never cross a magic link
/// (`RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS`, design §7.2), which is what
/// makes a hostile `Path=` (or a planted symlink inside a topdir trash)
/// unable to write outside `base` even though ordinary symlinks inside it
/// still resolve normally. Returns the final parent's fd (always a plain
/// `O_RDONLY|O_DIRECTORY` fd, whatever `base` itself is) and, for each
/// directory it had to create, the fd of ITS OWN parent paired with its
/// name -- exactly what `rmdir_reverse` needs to undo them, deepest first,
/// on any later failure.
fn ensure_parent(
    base: &OwnedFd,
    rel: &Path,
    beneath: bool,
) -> io::Result<(OwnedFd, Vec<(OwnedFd, OsString)>)> {
    let resolve = if beneath {
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS
    } else {
        ResolveFlags::empty()
    };
    let open_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    // Opening "." through `base` (rather than duplicating `base` itself)
    // gives a plain fd even when `base` is `O_PATH` (both Home's "/" and a
    // topdir's `open_top` are), and even when `rel` has no components at
    // all (an item trashed directly under the root).
    let mut cur = openat2(base, ".", open_flags, Mode::empty(), resolve)?;
    let mut created: Vec<(OwnedFd, OsString)> = Vec::new();
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            continue;
        };
        match openat2(&cur, name, open_flags, Mode::empty(), resolve) {
            Ok(next) => cur = next,
            Err(Errno::NOENT) => {
                mkdirat(&cur, name, Mode::from_raw_mode(0o777))?;
                let next = openat2(&cur, name, open_flags, Mode::empty(), resolve)?;
                let parent = fcntl_dupfd_cloexec(&cur, 0)?;
                created.push((parent, name.to_owned()));
                cur = next;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok((cur, created))
}

/// Undoes `ensure_parent`'s created directories, deepest first (each is
/// empty at that point, since nothing was written into it yet, or the
/// caller already failed before writing anything).
fn rmdir_reverse(created: &[(OwnedFd, OsString)]) {
    for (parent, name) in created.iter().rev() {
        let _ = unlinkat(parent, name, AtFlags::REMOVEDIR);
    }
}

/// Renames `from/src` to `to/name` (or, with `rename`, the first free
/// `name~k`) without ever overwriting anything.
fn rename_free(
    from: &OwnedFd,
    src: &OsStr,
    to: &OwnedFd,
    name: &OsStr,
    rename: bool,
) -> io::Result<OsString> {
    for k in 0..100_000u64 {
        let n = if k == 0 {
            name.to_owned()
        } else {
            info::candidate(name, k, 0)
        };
        match sys::rename_noreplace(from, src, to, &n) {
            Err(e) if rename && e.kind() == io::ErrorKind::AlreadyExists => continue,
            r => return r.map(|()| n),
        }
    }
    Err(io::Error::other("no free name"))
}

/// Restores one item: renames it back when its destination shares the
/// trash's subvolume and a mount shows both, otherwise copies it back
/// (design §7.2). `Ok(None)` means the person declined the copy-back size
/// prompt; the item stays in the trash, and that is not a failure.
fn restore_item(
    cx: &Cx,
    ts: &[Trash],
    it: &Item,
    rename: bool,
    yes: bool,
) -> Result<Option<PathBuf>, RestoreError> {
    let t = &ts[it.trash];
    let _lock = sys::lock(&t.dir, Lock::Shared)?;
    if !trash::still_same(t, &it.name, &it.info, it.entry) {
        return Err(RestoreError::Other(
            "it changed since it was listed; run the command again".into(),
        ));
    }

    let name = it
        .original
        .file_name()
        .ok_or_else(|| RestoreError::Other("bad original path".into()))?;
    let parent_orig = it
        .original
        .parent()
        .ok_or_else(|| RestoreError::Other("bad original path".into()))?;

    // Home: resolve from "/" (the user's own paths; symlinks followed).
    // Topdir: every lookup is RESOLVE_BENEATH the topdir fd, so a hostile
    // Path= or a planted symlink cannot leave the topdir.
    let (base, rel, beneath) = match t.kind {
        Kind::Home => {
            let base = sys::open_path(CWD, "/")?;
            let rel = parent_orig
                .strip_prefix("/")
                .map_err(|_| RestoreError::Other("bad original path".into()))?
                .to_path_buf();
            (base, rel, false)
        }
        _ => {
            let base = trash::open_top(t)?;
            let rel = parent_orig
                .strip_prefix(&t.base)
                .map_err(|_| RestoreError::Other("its original path is outside the topdir".into()))?
                .to_path_buf();
            (base, rel, true)
        }
    };

    let (pfd, created) = ensure_parent(&base, &rel, beneath)?;

    if !rename && sys::stat_at(&pfd, name).is_ok() {
        rmdir_reverse(&created);
        return Err(RestoreError::Conflict(
            "exists; not replacing it (use --rename)".into(),
        ));
    }

    let pid = sys::ident(&pfd)?;
    let pdir = sys::fd_path(&pfd)?;

    if pid.dev == t.files_id.dev {
        if let Some((from, to)) =
            sys::route(&cx.mounts, &t.path.join("files"), t.files_id, &pdir, pid)?
        {
            match rename_free(&from, &it.name, &to, name, rename) {
                Ok(n) => {
                    trash::unlink_info(t, &it.name);
                    return Ok(Some(pdir.join(n)));
                }
                // The rename crossed a filesystem boundary after all
                // (`route`'s own fd checks raced): fall through to copy.
                Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {}
                Err(e) => {
                    rmdir_reverse(&created);
                    return Err(RestoreError::Other(e.to_string()));
                }
            }
        }
    }

    // Copy back. The destination is complete and durable before the trash
    // copy goes (design §0.3 invariant 2).
    let size = sys::walk(t.files.as_fd(), &it.name, Check::Size)?.size;
    if !yes && size > cx.cfg.copy_threshold {
        let confirmed = confirm(
            &format!("copy {} back to {}?", human(size), it.original.display()),
            "-y",
        )
        .map_err(RestoreError::Other)?;
        if !confirmed {
            rmdir_reverse(&created);
            return Ok(None);
        }
    }

    let (bx, bfd) = match sys::make_box(pfd.as_fd(), ".rip-restore") {
        Ok(v) => v,
        Err(e) => {
            rmdir_reverse(&created);
            return Err(e.into());
        }
    };
    let discard_box = |bx: &OsStr| {
        sys::remove_tree(
            pfd.as_fd(),
            bx,
            &Remove {
                mnt: pid.mnt,
                manifest: None,
                trash: true,
            },
        );
    };
    if let Err(e) = sys::cp_archive(t.files.as_fd(), &it.name, bfd.as_fd(), OsStr::new("item")) {
        discard_box(&bx);
        rmdir_reverse(&created);
        return Err(RestoreError::Other(format!("copying back failed: {e}")));
    }
    let n = match rename_free(&bfd, OsStr::new("item"), &pfd, name, rename) {
        Ok(n) => n,
        Err(e) => {
            discard_box(&bx);
            rmdir_reverse(&created);
            return Err(RestoreError::Other(e.to_string()));
        }
    };
    let _ = unlinkat(&pfd, &bx, AtFlags::REMOVEDIR);
    sys::syncfs(&pfd)?;
    if let Err(e) = trash::discard(t, &cx.mounts, &it.name) {
        eprintln!("rip: restored, but the trash copy remains: {e}");
    }
    Ok(Some(pdir.join(n)))
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

/// Sorts by original-path component count, then by raw path bytes, so a
/// batch that includes both a directory and something inside it restores
/// the directory first (design §7.1 "Restore and undo sort their batch
/// parents first").
fn sort_parents_first(items: &mut [&Item]) {
    items.sort_by(|a, b| {
        a.original
            .components()
            .count()
            .cmp(&b.original.components().count())
            .then_with(|| {
                a.original
                    .as_os_str()
                    .as_bytes()
                    .cmp(b.original.as_os_str().as_bytes())
            })
    });
}

pub fn restore(
    cx: &Cx,
    paths: &[PathBuf],
    all: bool,
    rename: bool,
    yes: bool,
) -> Result<bool, String> {
    let (trashes, contents) = trash::load_all(&cx.mounts, cx.uid)?;
    print_warnings(&contents.warnings);

    let targets = select(cx, &trashes, &contents, paths, all, "restore", false)?;
    let mut items: Vec<&Item> = Vec::with_capacity(targets.len());
    for t in targets {
        match t {
            Target::Item(it) => items.push(it),
            // select() with orphans_ok = false never returns an orphan.
            Target::Orphan(_) => return Err("internal error: restore selected an orphan".into()),
        }
    }
    sort_parents_first(&mut items);

    let mut ok = true;
    for it in items {
        match restore_item(cx, &trashes, it, rename, yes) {
            Ok(Some(path)) => print_path(&cx.cwd, &path),
            Ok(None) => {}
            Err(e) => {
                eprintln!("rip: cannot restore '{}': {e}", it.original.display());
                ok = false;
            }
        }
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------
// undo (design §7.4)
// ---------------------------------------------------------------------------

/// The newest `DeletionDate` across every trashed item, or `None` when the
/// trash is empty.
fn newest_date(items: &[Item]) -> Option<civil::DateTime> {
    items.iter().map(|it| it.date).max()
}

/// Every item with exactly `date`, sorted parents first.
fn undo_batch(items: &[Item], date: civil::DateTime) -> Vec<&Item> {
    let mut batch: Vec<&Item> = items.iter().filter(|it| it.date == date).collect();
    sort_parents_first(&mut batch);
    batch
}

pub fn undo(cx: &Cx, yes: bool) -> Result<bool, String> {
    let (trashes, contents) = trash::load_all(&cx.mounts, cx.uid)?;
    print_warnings(&contents.warnings);

    let Some(newest) = newest_date(&contents.items) else {
        return Err("nothing to undo".into());
    };
    let batch = undo_batch(&contents.items, newest);

    let mut ok = true;
    for it in batch {
        match restore_item(cx, &trashes, it, false, yes) {
            Ok(Some(path)) => print_path(&cx.cwd, &path),
            Ok(None) => {}
            Err(RestoreError::Conflict(_)) => {
                let trash_path = trashes[it.trash].path.join("files").join(&it.name);
                eprintln!(
                    "rip: cannot restore '{}': it exists; use `rip restore --rename {}`",
                    it.original.display(),
                    trash_path.display()
                );
                ok = false;
            }
            Err(RestoreError::Other(msg)) => {
                eprintln!("rip: cannot restore '{}': {msg}", it.original.display());
                ok = false;
            }
        }
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------
// purge (design §7.4)
// ---------------------------------------------------------------------------

fn purge_line(cwd: &Path, trashes: &[Trash], t: &Target<'_>) -> String {
    match t {
        Target::Item(it) => format!(
            "{}  {}",
            it.date.strftime("%Y-%m-%d %H:%M:%S"),
            escape(display_path(cwd, &it.original).as_os_str().as_bytes())
        ),
        Target::Orphan(o) => format!(
            "{}  {} (orphan, no .trashinfo)",
            o.date.strftime("%Y-%m-%d %H:%M:%S"),
            escape(
                trashes[o.trash]
                    .path
                    .join("files")
                    .join(&o.name)
                    .as_os_str()
                    .as_bytes()
            )
        ),
    }
}

pub fn purge(cx: &Cx, paths: &[PathBuf], all: bool, yes: bool) -> Result<bool, String> {
    let (trashes, contents) = trash::load_all(&cx.mounts, cx.uid)?;
    print_warnings(&contents.warnings);

    let targets = select(cx, &trashes, &contents, paths, all, "purge", true)?;

    if !yes {
        for t in targets.iter().take(20) {
            eprintln!("  {}", purge_line(&cx.cwd, &trashes, t));
        }
        if targets.len() > 20 {
            eprintln!("  and {} more", targets.len() - 20);
        }
        if !confirm(
            &format!("permanently delete these {} items?", targets.len()),
            "-y",
        )? {
            return Ok(true);
        }
    }

    let mut by_trash: Vec<Vec<Doomed>> = (0..trashes.len()).map(|_| Vec::new()).collect();
    for t in &targets {
        match t {
            Target::Item(it) => by_trash[it.trash].push(Doomed::Item(it)),
            Target::Orphan(o) => by_trash[o.trash].push(Doomed::Orphan(o)),
        }
    }

    let mut ok = true;
    for (idx, doomed) in by_trash.into_iter().enumerate() {
        if doomed.is_empty() {
            continue;
        }
        let report = trash::delete_batch(&trashes[idx], &cx.mounts, &doomed, false);
        for (path, why) in &report.kept {
            eprintln!("rip: cannot delete '{}': {why}", path.display());
            ok = false;
        }
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{Dev, Ident};

    fn ident(ino: u64) -> Ident {
        Ident {
            dev: Dev(0, 0),
            ino,
            mnt: 0,
        }
    }

    fn item(trash: usize, original: &str, date: &str, ino: u64) -> Item {
        Item {
            trash,
            name: OsString::from(original.rsplit('/').next().unwrap()),
            original: PathBuf::from(original),
            date: date.parse().unwrap(),
            entry: ident(ino),
            info: (ident(ino + 1_000_000), Vec::new()),
        }
    }

    // ---- fzf_record / decode_fzf_output (design §13.1 "fzf record encode
    // and index decode") ----

    #[test]
    fn fzf_record_is_index_tab_date_tab_path_nul() {
        let it = item(0, "/home/u/Downloads/a b", "2026-01-02T03:04:05", 1);
        let rec = fzf_record(2, &it, Path::new("/home/u"));
        assert_eq!(rec, b"2\t2026-01-02 03:04:05\tDownloads/a b\0");
    }

    #[test]
    fn decode_fzf_output_accepts_valid_and_duplicate_indices() {
        let out = decode_fzf_output(b"1\tx\ty\x000\tx\ty\x001\tx\ty\x00", 2).unwrap();
        assert_eq!(
            out,
            vec![1, 0, 1],
            "duplicates must round-trip, not be dropped"
        );
    }

    #[test]
    fn decode_fzf_output_rejects_out_of_range() {
        let err = decode_fzf_output(b"5\tx\ty\x00", 2).unwrap_err();
        assert!(err.contains("unknown record"), "{err}");
    }

    #[test]
    fn decode_fzf_output_rejects_junk() {
        assert!(decode_fzf_output(b"nope\tx\ty\x00", 2).is_err());
        assert!(
            decode_fzf_output(b"\x00", 2).unwrap().is_empty(),
            "an empty record is skipped, not an error"
        );
    }

    // ---- undo selection (design §13.1 "Undo batch across trash dirs.
    // Parents-first ordering.") ----

    #[test]
    fn newest_date_across_trash_dirs() {
        let items = vec![
            item(0, "/home/u/a", "2026-01-01T00:00:00", 1),
            item(1, "/home/u/b", "2026-02-02T00:00:00", 2),
            item(0, "/home/u/c", "2026-01-15T00:00:00", 3),
        ];
        assert_eq!(
            newest_date(&items),
            Some("2026-02-02T00:00:00".parse().unwrap())
        );
    }

    #[test]
    fn newest_date_empty_is_none() {
        assert_eq!(newest_date(&[]), None);
    }

    #[test]
    fn undo_batch_picks_only_the_newest_date_across_trash_dirs() {
        let items = vec![
            item(0, "/home/u/old", "2026-01-01T00:00:00", 1),
            item(1, "/home/u/new-a", "2026-02-02T00:00:00", 2),
            item(0, "/home/u/new-b", "2026-02-02T00:00:00", 3),
        ];
        let batch = undo_batch(&items, "2026-02-02T00:00:00".parse().unwrap());
        let names: Vec<&str> = batch
            .iter()
            .map(|it| it.original.to_str().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"/home/u/new-a") && names.contains(&"/home/u/new-b"));
        assert!(!names.contains(&"/home/u/old"));
    }

    #[test]
    fn undo_batch_sorts_parents_first() {
        // `rip dir/f dir` then `rip undo`: dir must come back before dir/f.
        let items = vec![
            item(0, "/home/u/dir/f", "2026-01-01T00:00:00", 1),
            item(0, "/home/u/dir", "2026-01-01T00:00:00", 2),
            item(0, "/home/u/dir/sub/g", "2026-01-01T00:00:00", 3),
        ];
        let batch = undo_batch(&items, "2026-01-01T00:00:00".parse().unwrap());
        let names: Vec<&str> = batch
            .iter()
            .map(|it| it.original.to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["/home/u/dir", "/home/u/dir/f", "/home/u/dir/sub/g"],
            "{names:?}"
        );
    }

    // ---- resolve / lexical_normalize (design §7.1) ----

    #[test]
    fn lexical_normalize_collapses_dot_and_dot_dot() {
        assert_eq!(
            lexical_normalize(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn resolve_joins_a_relative_path_against_cwd() {
        // The tempdir is real, so the parent canonicalizes; the (missing)
        // leaf is appended lexically.
        let dir = tempfile::tempdir().unwrap();
        let got = resolve(dir.path(), Path::new("gone"));
        assert_eq!(got, dir.path().canonicalize().unwrap().join("gone"));
    }

    #[test]
    fn resolve_falls_back_to_lexical_when_the_parent_does_not_exist() {
        let got = resolve(Path::new("/"), Path::new("/no/such/dir/../also-gone"));
        assert_eq!(got, PathBuf::from("/no/such/also-gone"));
    }
}
