//! Stub for C1. Real implementation (selection by age/size, deletion) lands
//! in C4c. See docs/design.md §5.4 (empty and purge / delete_batch).
#![allow(dead_code, unused_variables)]

use crate::Cx;

pub fn run(
    cx: &Cx,
    older_than: Option<jiff::Span>,
    max_size: Option<u64>,
    yes: bool,
) -> Result<bool, String> {
    Err("empty: not implemented yet".into())
}
