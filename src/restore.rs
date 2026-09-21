//! Stub for C1. Real implementation (list, selection, fzf picker, restore,
//! undo, purge) lands in C3 (list) and C4b (the rest). See docs/design.md §7, §9.
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;

use crate::Cx;

pub fn list(cx: &Cx, all: bool, null: bool) -> Result<bool, String> {
    Err("list: not implemented yet".into())
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
