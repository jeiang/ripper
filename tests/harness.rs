//! Verifies the sandbox itself (docs/design.md §13.3), before any test
//! trusts it to isolate `rip` from the real trash.

mod common;

use common::{Body, Sandbox};

/// `st_dev` relations are as designed: P (`/persist`) has its own `st_dev`
/// but the same FsId (mountinfo major:minor) as BASE (`/home`), because they
/// are different btrfs subvolumes of the one real filesystem. `mv --no-copy`
/// between the two separate `/persist` binds (`/home/u/Downloads` and
/// `/home/u/Documents`) gives `EXDEV`, even though both are backed by P,
/// because rename crosses mounts, not devices; the same move through
/// `/persist` -- the one mount that contains both paths -- succeeds.
#[test]
fn sandbox_works() {
    let sandbox = Sandbox::artemis();

    let out = sandbox.exec(
        "/",
        &[
            "bash",
            "-c",
            "stat -c '%d %m' /home /persist && \
             awk '$5==\"/home\"||$5==\"/persist\"{print $5, $3}' /proc/self/mountinfo",
        ],
    );
    assert!(
        out.status.success(),
        "stat/mountinfo probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("utf8 probe output");
    let mut lines = text.lines();

    let home_dev = first_field(lines.next().expect("home stat line"));
    let persist_dev = first_field(lines.next().expect("persist stat line"));
    assert_ne!(
        home_dev, persist_dev,
        "P must have its own st_dev, distinct from BASE's: {text}"
    );

    let mut fsid = std::collections::HashMap::new();
    for l in lines {
        let mut parts = l.split_whitespace();
        let point = parts.next().expect("mountpoint field");
        let id = parts.next().expect("fsid field");
        fsid.insert(point.to_string(), id.to_string());
    }
    assert_eq!(
        fsid.get("/home"),
        fsid.get("/persist"),
        "/home and /persist must share one FsId (mountinfo major:minor): {fsid:?}"
    );

    // Plant a file under one /persist bind, and try to rename it straight
    // into the other: two separate mounts of the same subvolume, so EXDEV.
    let downloads = sandbox.host("/home/u/Downloads");
    std::fs::write(downloads.join("x"), b"hello").expect("plant x");

    let out = sandbox.exec(
        "/home/u/Downloads",
        &["mv", "--no-copy", "x", "../Documents/x"],
    );
    assert!(
        !out.status.success(),
        "mv across the two separate /persist binds should fail with EXDEV: {out:?}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cross-device"),
        "expected a cross-device error, got: {out:?}"
    );
    assert!(
        downloads.join("x").exists(),
        "a failed cross-mount mv must not have moved anything"
    );
    assert!(!sandbox.host("/home/u/Documents").join("x").exists());

    // The same move, through /persist (the one mount that contains both
    // Downloads and Documents), must succeed.
    let out = sandbox.exec(
        "/persist",
        &["mv", "--no-copy", "u/Downloads/x", "u/Documents/x"],
    );
    assert!(
        out.status.success(),
        "mv through /persist should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!downloads.join("x").exists());
    assert!(sandbox.host("/home/u/Documents").join("x").exists());
}

fn first_field(line: &str) -> &str {
    line.split_whitespace().next().expect("a field")
}

/// The sandbox has no real trash, no `/mnt/Mumei` and none of the stray
/// `.Trash-*` dirs the host has: `rip list --all` must print nothing. Needs
/// `rip list`, which lands in C3.
#[test]
#[ignore = "needs rip list (C3)"]
fn no_real_trash_visible() {
    let sandbox = Sandbox::artemis();

    let out = sandbox.rip("/home/u", &["list", "--all"]);
    assert!(
        out.status.success(),
        "rip list --all failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "expected no output, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A planted trash entry is still only visible on disk, not through
    // `rip` (list isn't implemented yet); this keeps `plant` itself
    // exercised even while this test is ignored.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"trashed".to_vec()),
    );
    assert!(
        sandbox
            .host("/home/u/.local/share/Trash")
            .join("files/x")
            .is_file()
    );
}
