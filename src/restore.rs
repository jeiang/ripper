//! `list`, plus stubs for selection, the fzf picker, restore, undo and purge
//! (the rest lands in C4b). See docs/design.md §5.3 (restore crash
//! consistency) and the brief's `rip list` output rules (design §11).
#![allow(dead_code, unused_variables)]

use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::trash::{self, Item};
use crate::{Cx, escape};

pub fn list(cx: &Cx, all: bool, null: bool) -> Result<bool, String> {
    let (contents, discovery_warnings) = load_contents(cx)?;
    for w in discovery_warnings.iter().chain(&contents.warnings) {
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

/// Opens the home trash read-only (a missing one is not an error: read-only
/// commands simply have no home trash to show) and discovers every topdir
/// trash, returning discovery's own warnings alongside the loaded contents.
fn load_contents(cx: &Cx) -> Result<(trash::Contents, Vec<String>), String> {
    let home_path = trash::home_path()?;
    let home = trash::open_home(&home_path, false, cx.uid).map_err(|e| e.to_string())?;
    let mut warn = Vec::new();
    let trashes = trash::discover(home, &cx.mounts, cx.uid, &mut warn);
    Ok((trash::load(&trashes), warn))
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

pub fn restore(
    cx: &Cx,
    paths: &[PathBuf],
    all: bool,
    rename: bool,
    yes: bool,
) -> Result<bool, String> {
    Err("restore: not implemented yet".into())
}

pub fn undo(cx: &Cx, yes: bool) -> Result<bool, String> {
    Err("undo: not implemented yet".into())
}

pub fn purge(cx: &Cx, paths: &[PathBuf], all: bool, yes: bool) -> Result<bool, String> {
    Err("purge: not implemented yet".into())
}
