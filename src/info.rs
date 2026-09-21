//! Stub for C1. Real implementation (.trashinfo encode/parse, Path= codec,
//! dates, collision names) lands in C2c. See docs/design.md §3.
#![allow(dead_code, unused_variables)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use jiff::civil;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Home,
    /// `$topdir/.Trash/$uid`
    Admin,
    /// `$topdir/.Trash-$uid`
    User,
}

pub fn encode(path_field: &[u8], date: civil::DateTime) -> Vec<u8> {
    unimplemented!("info::encode: not implemented yet")
}

pub fn parse(text: &[u8]) -> Result<(Vec<u8>, civil::DateTime), &'static str> {
    Err("info::parse: not implemented yet")
}

pub fn original(kind: Kind, base: &Path, decoded: &[u8]) -> Result<PathBuf, &'static str> {
    Err("info::original: not implemented yet")
}

/// `name`, or `name~k`, cut so that it plus `reserve` bytes fits NAME_MAX (255).
pub fn candidate(name: &OsStr, k: u64, reserve: usize) -> OsString {
    unimplemented!("info::candidate: not implemented yet")
}
