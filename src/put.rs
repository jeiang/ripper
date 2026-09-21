//! Stub for C1. Real implementation (placement, refusals, rename and copy
//! fallback for `rip FILE...`) lands in C4a. See docs/design.md §5-§6.
#![allow(dead_code, unused_variables)]

use crate::{Cli, Cx};

pub fn run(cx: &Cx, cli: &Cli) -> Result<bool, String> {
    Err("put: not implemented yet".into())
}
