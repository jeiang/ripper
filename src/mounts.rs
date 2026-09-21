//! Stub for C1. Real implementation (mountinfo parsing, path translation, mount
//! refusals) lands in C2a. See docs/design.md §3 (placement for put).
#![allow(dead_code, unused_variables)]

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FsId(pub u32, pub u32);

#[derive(Debug)]
pub struct Mount {
    pub id: u64,
    pub parent: u64,
    pub fs: FsId,
    /// The mount's root inside its filesystem (btrfs: from the top-level subvolume).
    pub root: PathBuf,
    /// The mount point in this namespace.
    pub point: PathBuf,
    pub fstype: OsString,
    /// Where the mount point lies: the parent's filesystem and the path inside it.
    pub under: Option<(FsId, PathBuf)>,
}

#[derive(Debug, Default)]
pub struct Mounts(Vec<Mount>);

impl Mounts {
    pub fn read() -> io::Result<Self> {
        Err(io::Error::other(
            "mounts::Mounts::read: not implemented yet",
        ))
    }

    pub fn parse(text: &[u8]) -> Self {
        unimplemented!("mounts::Mounts::parse: not implemented yet")
    }

    pub fn by_id(&self, id: u64) -> Option<&Mount> {
        unimplemented!("mounts::Mounts::by_id: not implemented yet")
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Mount> {
        self.0.iter()
    }
}

pub fn inside(m: &Mount, path: &Path) -> Option<PathBuf> {
    unimplemented!("mounts::inside: not implemented yet")
}

pub fn through(own: &Mount, via: &Mount, path: &Path) -> Option<PathBuf> {
    unimplemented!("mounts::through: not implemented yet")
}

pub fn route_candidates(
    ms: &Mounts,
    am: u64,
    a: &Path,
    bm: u64,
    b: &Path,
) -> Vec<(u64, PathBuf, PathBuf)> {
    unimplemented!("mounts::route_candidates: not implemented yet")
}

pub fn mount_conflict(
    ms: &Mounts,
    own: &Mount,
    path: &Path,
    is_mount_root: bool,
) -> Option<String> {
    unimplemented!("mounts::mount_conflict: not implemented yet")
}
