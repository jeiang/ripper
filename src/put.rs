//! Stub for C1. Real implementation (placement, refusals, rename and copy
//! fallback for `rip FILE...`) lands in C4a. See docs/design.md §3 (placement
//! for put) and §5.1-§5.2 (put crash consistency).
#![allow(dead_code, unused_variables)]

use crate::{Cli, Cx};

pub fn run(cx: &Cx, cli: &Cli) -> Result<bool, String> {
    Err("put: not implemented yet".into())
}
