//! Verifies the sandbox itself (docs/design.md §3.1), before any test
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
/// `.Trash-*` dirs the host has: `rip list --all` must print nothing.
#[test]
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

    // A planted trash entry is only visible on disk here (list already ran
    // above, against a sandbox that had none): this keeps `plant` itself
    // exercised, and documents that it is not somehow already reflected in
    // the assertion above.
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

/// `read_only` turns a layout bind into a genuinely read-only mount: a write
/// there fails with `EROFS` (not the `EACCES` of a `0555` directory), while
/// the host directory behind it is still what `host()` reports.
#[test]
fn read_only_mount_gives_erofs() {
    let mut sandbox = Sandbox::artemis();
    sandbox.read_only("/mnt/other");
    std::fs::write(sandbox.host("/mnt/other").join("seen"), b"x").unwrap();

    let out = sandbox.exec(
        "/mnt/other",
        &["bash", "-c", "cat seen && echo y > written"],
    );
    assert!(!out.status.success(), "the write must fail: {out:?}");
    assert_eq!(out.stdout, b"x", "the bind must still show the host dir");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Read-only file system"),
        "expected EROFS: {out:?}"
    );
    assert!(!sandbox.host("/mnt/other").join("written").exists());
}

/// `tmpfs` mounts a size-limited tmpfs: writing past its size fails with
/// `ENOSPC`.
#[test]
fn tmpfs_size_gives_enospc() {
    let mut sandbox = Sandbox::artemis();
    sandbox.tmpfs("/mnt/small", 4096);

    let out = sandbox.exec(
        "/",
        &["bash", "-c", "head -c 65536 /dev/zero > /mnt/small/f"],
    );
    assert!(!out.status.success(), "the write must fail: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No space left on device"),
        "expected ENOSPC: {out:?}"
    );
}

/// A tmpfs has no host path, so `host()` refuses rather than guessing one
/// from a parent bind.
#[test]
#[should_panic(expected = "which has no host path")]
fn host_panics_on_a_tmpfs() {
    let mut sandbox = Sandbox::artemis();
    sandbox.tmpfs("/mnt/other/small", 4096);
    sandbox.host("/mnt/other/small/f");
}

/// `without_fzf` leaves no `fzf` on `PATH` at all, neither the fake one nor
/// the dev shell's real one; the default sandbox finds the fake.
#[test]
fn without_fzf_leaves_no_fzf_on_path() {
    let mut sandbox = Sandbox::artemis();
    let probe = ["bash", "-c", "command -v fzf"];

    let out = sandbox.exec("/", &probe);
    assert!(out.status.success(), "the fake fzf must be found: {out:?}");
    assert_eq!(out.stdout, b"/run/rip/fakebin/fzf\n");

    sandbox.without_fzf();
    let out = sandbox.exec("/", &probe);
    assert!(
        !out.status.success(),
        "no fzf may be on PATH: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// `spawn_tty` hands `script` the whole bwrap command line as one shell
/// string; an argument with a single quote, a space and a non-UTF-8 byte
/// must still reach `rip` byte for byte (here: name the file it trashes).
#[test]
fn spawn_tty_round_trips_a_non_utf8_argument() {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;

    let sandbox = Sandbox::artemis();
    let name = std::ffi::OsStr::from_bytes(b"q'uo te\xff");
    let src = sandbox.host("/home/u/Downloads").join(name);
    std::fs::write(&src, b"x").unwrap();

    let mut child = sandbox.spawn_tty("/home/u/Downloads", &[name]);
    drop(child.stdin.take());
    let mut tty = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut tty).unwrap();
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "rip failed: {}",
        String::from_utf8_lossy(&tty)
    );
    assert!(!src.exists(), "the file must have been trashed");
    assert!(
        sandbox
            .host("/home/u/.local/share/Trash/files")
            .join(name)
            .exists(),
        "the trashed name must be byte-identical: {}",
        String::from_utf8_lossy(&tty)
    );
}
