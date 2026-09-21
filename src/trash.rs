//! Stub for C1. Real implementation (discovery, load, reserve, staging,
//! delete_batch, discard) lands in C3. See docs/design.md §3-§4, §6.3, §8.3.
#![allow(dead_code, unused_variables)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use jiff::civil;

use crate::info::Kind;
use crate::mounts::Mounts;
use crate::sys::Ident;

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
    pub files_id: Ident,
}

pub struct Item {
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
    pub trash: usize,
    pub name: OsString,
    /// lstat ctime, local.
    pub date: civil::DateTime,
    pub entry: Ident,
    /// `Some`: malformed info.
    pub info: Option<(Ident, Vec<u8>)>,
}

pub struct Dangling {
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

/// Holds a reserved `info/NAME.trashinfo`. Dropping it without `commit()` unlinks
/// the file, so every early return, error and panic rolls back.
pub struct Reserved<'t> {
    t: &'t Trash,
    base: OsString,
    k: u64,
    name: OsString,
    text: Vec<u8>,
    live: bool,
}

impl Reserved<'_> {
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Something took `files/NAME` after the check: release it and claim the next name.
    pub fn advance(&mut self) -> io::Result<()> {
        Err(io::Error::other(
            "trash::Reserved::advance: not implemented yet",
        ))
    }

    /// The `files/` entry is in place: keep the info file.
    pub fn commit(self) -> OsString {
        unimplemented!("trash::Reserved::commit: not implemented yet")
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {}
}

pub fn home_path() -> Result<PathBuf, String> {
    Err("trash::home_path: not implemented yet".into())
}

pub fn open_home(path: &Path, create: bool, uid: u32) -> io::Result<Option<Trash>> {
    Err(io::Error::other("trash::open_home: not implemented yet"))
}

// `&mut Vec<String>`, not `&mut [String]`: the real implementation (C3) pushes
// discovery warnings onto it as it walks mountinfo.
#[allow(clippy::ptr_arg)]
pub fn discover(home: Option<Trash>, ms: &Mounts, uid: u32, warn: &mut Vec<String>) -> Vec<Trash> {
    unimplemented!("trash::discover: not implemented yet")
}

pub fn open_trash(path: &Path, kind: Kind, base: &Path, uid: u32) -> io::Result<Option<Trash>> {
    Err(io::Error::other("trash::open_trash: not implemented yet"))
}

pub fn load(ts: &[Trash]) -> Contents {
    unimplemented!("trash::load: not implemented yet")
}

pub fn reserve<'t>(t: &'t Trash, base: &OsStr, text: Vec<u8>) -> io::Result<Reserved<'t>> {
    Err(io::Error::other("trash::reserve: not implemented yet"))
}

pub fn staging(t: &Trash) -> io::Result<OwnedFd> {
    Err(io::Error::other("trash::staging: not implemented yet"))
}

pub fn open_top(t: &Trash) -> io::Result<OwnedFd> {
    Err(io::Error::other("trash::open_top: not implemented yet"))
}

pub fn still_same(t: &Trash, name: &OsStr, info: &(Ident, Vec<u8>), entry: Ident) -> bool {
    unimplemented!("trash::still_same: not implemented yet")
}

pub fn unlink_info(t: &Trash, name: &OsStr) {
    unimplemented!("trash::unlink_info: not implemented yet")
}

pub fn discard(t: &Trash, ms: &Mounts, name: &OsStr) -> io::Result<()> {
    Err(io::Error::other("trash::discard: not implemented yet"))
}

pub enum Doomed<'a> {
    Item(&'a Item),
    Orphan(&'a Orphan),
    Dangling(&'a Dangling),
}

#[derive(Default)]
pub struct Report {
    pub deleted: u64,
    pub kept: Vec<(PathBuf, String)>,
}

pub fn delete_batch(t: &Trash, ms: &Mounts, doomed: &[Doomed], clean_staging: bool) -> Report {
    unimplemented!("trash::delete_batch: not implemented yet")
}
