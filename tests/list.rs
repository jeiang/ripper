//! `rip list` (docs/design.md §11, design.md §13.3 "tests/list.rs"). Every
//! test runs `rip` inside the bwrap sandbox (tests/common), never against a
//! real trash.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::{Body, Sandbox};
use rustix::fs::{CWD, Mode, mkfifoat};
use rustix::process::getuid;

/// Splits a `-0` `rip list` record stream into `(date, raw path bytes)`
/// pairs, in the order they appear (`list` already writes oldest first).
fn parse_null_records(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    bytes
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            let tab = r
                .iter()
                .position(|&b| b == b'\t')
                .expect("record has a tab");
            let date = String::from_utf8(r[..tab].to_vec()).expect("date is ASCII");
            (date, r[tab + 1..].to_vec())
        })
        .collect()
}

/// Runs `f` on a background thread and waits at most `secs` for it,
/// panicking with a clear message instead of stalling the whole suite if it
/// never returns. Used only to turn a real hang (an internal bug) into a
/// fast, clear test failure.
fn with_timeout<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("operation timed out (a hang)")
}

#[test]
fn cwd_scope_and_all() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"y",
        b"/home/u/Documents/y",
        "2026-02-02T00:00:00",
        Body::File(b"data".to_vec()),
    );

    // cwd scope: only the item under /home/u/Downloads, shown relative.
    let out = sandbox.rip("/home/u/Downloads", &["list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        b"2026-01-01 00:00:00  x\n",
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // --all: both items, oldest first; the one outside cwd shown absolute.
    let out = sandbox.rip("/home/u/Downloads", &["list", "--all", "-0"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(
        records,
        vec![
            ("2026-01-01 00:00:00".to_string(), b"x".to_vec()),
            (
                "2026-02-02 00:00:00".to_string(),
                b"/home/u/Documents/y".to_vec()
            ),
        ]
    );
}

#[test]
fn dedup_bind_aliases() {
    let uid = getuid().as_raw();
    let mut sandbox = Sandbox::artemis();
    let other_host = sandbox.host("/mnt/other");
    sandbox.bind(other_host, "/mnt/other2");

    sandbox.plant(
        &format!("/mnt/other/.Trash-{uid}"),
        b"x",
        b"sub/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip("/", &["list", "--all", "-0"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    // Which of the two aliases discovery happens to see first decides
    // whether the listed path reads through /mnt/other or /mnt/other2 (both
    // name the exact same on-disk trash dir); only the count is load-bearing.
    assert_eq!(
        records.len(),
        1,
        "the same trash dir must be listed once: {records:?}"
    );
    // cwd is "/", so every absolute path is technically "under" it: the
    // path is shown relative (no leading slash), per design §11.
    let path = String::from_utf8_lossy(&records[0].1);
    assert!(
        path == "mnt/other/sub/x" || path == "mnt/other2/sub/x",
        "{path}"
    );
}

#[test]
fn stray_topdir_trash_found() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        &format!("/home/u/Downloads/.Trash-{uid}"),
        b"a-b",
        b"a b",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip("/", &["list", "--all", "-0"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(records.len(), 1, "{records:?}");
    // cwd is "/", so this is shown relative to it (no leading slash).
    assert_eq!(records[0].1, b"home/u/Downloads/a b");
}

#[test]
fn skips_invalid_dirs() {
    let uid = getuid().as_raw();
    let mut sandbox = Sandbox::artemis();

    // A symlinked trash root: open_trash's O_NOFOLLOW must refuse it
    // outright (docs/design.md §0 invariant 5).
    let side_host = sandbox.host("/mnt/side");
    std::os::unix::fs::symlink("/tmp", side_host.join(format!(".Trash-{uid}"))).unwrap();

    // A foreign owner: the real, root-owned /etc bound exactly where rip
    // looks for this mount's per-uid trash.
    sandbox.bind("/etc", &format!("/mnt/pside/.Trash-{uid}"));

    // A `files` subdirectory that is a symlink instead of a real directory:
    // must never be followed, or `empty` could later delete through it.
    let other_host = sandbox.host("/mnt/other");
    let broken = other_host.join(format!(".Trash-{uid}"));
    std::fs::create_dir_all(broken.join("info")).unwrap();
    std::os::unix::fs::symlink("/tmp", broken.join("files")).unwrap();

    // A non-sticky admin `.Trash`: discover() must never look inside it, no
    // matter what it holds.
    sandbox.plant(
        &format!("/home/u/Downloads/.Trash/{uid}"),
        b"bad",
        b"bad",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );

    // One real, valid item, so this also proves the broken candidates do
    // not stop discovery of the rest.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"good",
        b"/home/u/Downloads/good",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = with_timeout(20, move || sandbox.rip("/", &["list", "--all", "-0"]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(records.len(), 1, "{records:?}");
    // cwd is "/", so this is shown relative to it (no leading slash).
    assert_eq!(records[0].1, b"home/u/Downloads/good");
}

#[test]
fn home_trash_missing_one_of_files_info_warns() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();

    // Corrupt the home trash: `files/` exists with a real trashed entry,
    // but `info/` is missing (e.g. a crash between ensure_home()'s two
    // mkdirat calls, or an accidental deletion). open_home must not treat
    // this like "no home trash yet" -- that silent skip is only for *both*
    // files/ and info/ being absent (the impermanence pre-mount case) -- so
    // it must warn instead of silently hiding the trashed data.
    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    std::fs::create_dir_all(trash_host.join("files")).unwrap();
    std::fs::write(trash_host.join("files/secret"), b"data").unwrap();

    // A real item in a topdir trash, so this also proves discovery keeps
    // going past the broken home trash instead of aborting the command.
    sandbox.plant(
        &format!("/mnt/other/.Trash-{uid}"),
        b"good",
        b"/mnt/other/good",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = with_timeout(20, move || sandbox.rip("/", &["list", "--all", "-0"]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("info/ subdirectory is missing"),
        "expected a warning about the home trash's missing info/, got: {stderr:?}"
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].1, b"mnt/other/good");
}

#[test]
fn unreadable_mountpoint_skipped() {
    let mut sandbox = Sandbox::artemis();
    let locked = tempfile::tempdir().unwrap();
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    sandbox.bind(locked.path(), "/mnt/locked");

    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"good",
        b"/home/u/Downloads/good",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip("/", &["list", "--all", "-0"]);
    // Restore permissions before any assertion can early-return and skip
    // this, so `locked`'s own Drop can still remove it.
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(records.len(), 1, "{records:?}");
}

#[test]
fn malformed_and_fifo_infos() {
    let sandbox = Sandbox::artemis();
    let trash_host = sandbox.host("/home/u/.local/share/Trash");

    // Malformed: an empty Path= (info::original rejects it) but files/bad
    // is real, so it must become an orphan with a warning, not an item and
    // not silently dropped.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"bad",
        b"",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    // A FIFO in place of an info file: opening it (O_NONBLOCK) must never
    // block, and it must be treated as malformed, not read.
    std::fs::create_dir_all(trash_host.join("files")).unwrap();
    std::fs::create_dir_all(trash_host.join("info")).unwrap();
    std::fs::write(trash_host.join("files/fifo-item"), b"x").unwrap();
    mkfifoat(
        CWD,
        trash_host.join("info/fifo-item.trashinfo"),
        Mode::from_raw_mode(0o600),
    )
    .unwrap();

    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"good",
        b"/home/u/Downloads/good",
        "2026-02-02T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = with_timeout(20, move || sandbox.rip("/", &["list", "--all", "-0"]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(records.len(), 1, "{records:?}");
    // cwd is "/", so this is shown relative to it (no leading slash).
    assert_eq!(records[0].1, b"home/u/Downloads/good");
    assert!(
        !out.stderr.is_empty(),
        "expected a warning about the malformed .trashinfo files"
    );
}

#[test]
fn undashed_date() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "20260321T10:30:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip("/home/u/Downloads", &["list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        b"2026-03-21 10:30:00  x\n",
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn escape_on_terminal() {
    let sandbox = Sandbox::artemis();
    let mut original = b"/home/u/Downloads/".to_vec();
    original.push(0x1b); // a raw ESC byte in the original filename
    original.extend_from_slice(b"evil");
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        &original,
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip_tty("/home/u/Downloads", &["list"], "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !out.stdout.contains(&0x1b),
        "a raw ESC byte reached the terminal: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stdout.windows(4).any(|w| w == b"\\x1b"),
        "expected the escaped form \\x1b in the output: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// c23 (review round, fix-empty.json): a relative Path= in the home trash
// must resolve against $XDG_DATA_HOME, not against a symlinked Trash's
// target's parent.
#[test]
fn relative_path_in_symlinked_home_trash_resolves_against_xdg_data_home() {
    let mut sandbox = Sandbox::artemis();
    sandbox.without("/home/u/.local/share/Trash");
    let home_dir = sandbox.host("/home/u/.local/share");
    std::fs::create_dir_all(&home_dir).unwrap();
    let target_host = sandbox.host("/persist").join("u/.local/share/Trash");
    std::fs::create_dir_all(target_host.join("files")).unwrap();
    std::fs::create_dir_all(target_host.join("info")).unwrap();
    // The home trash itself is a symlink to another subvolume (e.g.
    // impermanence's "symlink" method), pointing at an *inside* path --
    // unlike tests/common's own artemis layout, which bind-mounts it.
    std::os::unix::fs::symlink("/persist/u/.local/share/Trash", home_dir.join("Trash")).unwrap();
    std::fs::write(target_host.join("files/note.txt"), b"data").unwrap();
    std::fs::write(
        target_host.join("info/note.txt.trashinfo"),
        b"[Trash Info]\nPath=docs/note.txt\nDeletionDate=2026-01-01T00:00:00\n",
    )
    .unwrap();

    let out = sandbox.rip("/", &["list", "--all", "-0"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records = parse_null_records(&out.stdout);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].1,
        b"home/u/.local/share/docs/note.txt",
        "a relative Path= in the home trash must resolve against $XDG_DATA_HOME \
         ($HOME/.local/share), not the symlinked Trash's target's parent: {:?}",
        String::from_utf8_lossy(&records[0].1)
    );
}
