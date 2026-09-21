//! Stub for C1. Real implementation (fd-based rename, walk, removal, copy and
//! lock primitives) lands in C2b. See docs/design.md §3, §6.3, §6.6-§6.8.
#![allow(dead_code, unused_variables)]

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::StatxAttributes;
use rustix::path::Arg;

use crate::mounts::Mounts;

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
    pub attrs: StatxAttributes,
}

pub fn stat(p: &Path) -> io::Result<Meta> {
    Err(io::Error::other("sys::stat: not implemented yet"))
}

pub fn stat_at(d: impl AsFd, n: impl Arg) -> io::Result<Meta> {
    Err(io::Error::other("sys::stat_at: not implemented yet"))
}

pub fn ident(fd: impl AsFd) -> io::Result<Ident> {
    Err(io::Error::other("sys::ident: not implemented yet"))
}

pub fn fd_path(fd: impl AsFd) -> io::Result<PathBuf> {
    Err(io::Error::other("sys::fd_path: not implemented yet"))
}

pub fn open_dir(d: impl AsFd, n: impl Arg) -> io::Result<OwnedFd> {
    Err(io::Error::other("sys::open_dir: not implemented yet"))
}

pub fn open_path(d: impl AsFd, n: impl Arg) -> io::Result<OwnedFd> {
    Err(io::Error::other("sys::open_path: not implemented yet"))
}

pub fn read_names(fd: impl AsFd) -> io::Result<Vec<OsString>> {
    Err(io::Error::other("sys::read_names: not implemented yet"))
}

pub fn rename_noreplace(a: impl AsFd, an: &OsStr, b: impl AsFd, bn: &OsStr) -> io::Result<()> {
    Err(io::Error::other(
        "sys::rename_noreplace: not implemented yet",
    ))
}

pub enum Lock {
    Shared,
    Exclusive,
}

pub struct LockGuard {
    fd: OwnedFd,
}

/// `Ok(None)`: the filesystem does not support `flock` (e.g. some FUSE mounts).
pub fn lock(dir: &OwnedFd, l: Lock) -> io::Result<Option<LockGuard>> {
    Err(io::Error::other("sys::lock: not implemented yet"))
}

pub fn route(
    ms: &Mounts,
    a: &Path,
    a_id: Ident,
    b: &Path,
    b_id: Ident,
) -> io::Result<Option<(OwnedFd, OwnedFd)>> {
    Err(io::Error::other("sys::route: not implemented yet"))
}

pub enum Check {
    Size,
    Removable { uid: u32 },
}

pub struct Walk {
    pub size: u64,
    pub problem: Option<String>,
    pub manifest: Manifest,
}

/// ino -> (size, mtime). mtime, not ctime: unlinking one name of a hard-linked
/// inode changes the inode's ctime, so a ctime manifest would report every
/// other link in the tree as changed.
pub struct Manifest {
    dirs: HashSet<u64>,
    files: HashMap<u64, (u64, (i64, u32))>,
}

impl Manifest {
    pub fn unchanged(&self, m: &Meta) -> bool {
        self.files.get(&m.id.ino) == Some(&(m.size, m.mtime))
    }
}

pub fn walk(parent: BorrowedFd, name: &OsStr, check: Check) -> io::Result<Walk> {
    Err(io::Error::other("sys::walk: not implemented yet"))
}

pub struct Remove<'a> {
    pub mnt: u64,
    pub manifest: Option<&'a Manifest>,
    pub trash: bool,
}

#[derive(Default)]
pub struct Removal {
    pub removed_any: bool,
    pub kept: Vec<(PathBuf, String)>,
}

pub fn remove_tree(parent: BorrowedFd, name: &OsStr, o: &Remove) -> Removal {
    unimplemented!("sys::remove_tree: not implemented yet")
}

pub fn cp_archive(
    src_dir: BorrowedFd,
    src: &OsStr,
    dst_dir: BorrowedFd,
    dst: &OsStr,
) -> io::Result<()> {
    Err(io::Error::other("sys::cp_archive: not implemented yet"))
}

pub fn make_box(dir: BorrowedFd, prefix: &str) -> io::Result<(OsString, OwnedFd)> {
    Err(io::Error::other("sys::make_box: not implemented yet"))
}

pub fn syncfs(fd: impl AsFd) -> io::Result<()> {
    Err(io::Error::other("sys::syncfs: not implemented yet"))
}

/// Sets the soft `RLIMIT_NOFILE` to the hard limit, so deep trees do not hit
/// `EMFILE` early. Best effort: a failure here is not fatal.
pub fn raise_nofile() {}
