//! `rip empty` (docs/design.md §8, design.md §13.3 "tests/empty.rs"). Every
//! test runs `rip` inside the bwrap sandbox (tests/common), never against a
//! real trash.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use common::{Body, Sandbox};
use rustix::fs::FlockOperation;
use rustix::process::getuid;

/// Writes `files/NAME` directly on the host, with no matching `.trashinfo`:
/// a genuine Orphan (docs/design.md §4), dated by its own (real) ctime.
/// `Sandbox::plant` always writes an info file too, so it cannot produce
/// this state on its own.
fn plant_orphan(sandbox: &Sandbox, trash: &str, name: &[u8], content: &[u8]) {
    let files_dir = sandbox.host(trash).join("files");
    fs::create_dir_all(&files_dir).unwrap();
    fs::write(files_dir.join(OsStr::from_bytes(name)), content).unwrap();
}

/// Writes a leftover box directly under `trash`'s `.rip-staging`, as a crash
/// would leave one (docs/design.md §8.3): `put.*` for an unfinished
/// copy-fallback box, `del.*` for a tombstone `empty` renamed but never
/// finished removing.
fn plant_staging_box(sandbox: &Sandbox, trash: &str, name: &str, content: &[u8]) {
    let box_dir = sandbox.host(trash).join(".rip-staging").join(name);
    fs::create_dir_all(&box_dir).unwrap();
    fs::write(box_dir.join("leftover"), content).unwrap();
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn days_ago(n: i64) -> String {
    jiff::Zoned::now()
        .checked_sub(jiff::Span::new().days(n))
        .unwrap()
        .datetime()
        .strftime("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

fn file_of(size: u64) -> Vec<u8> {
    vec![b'x'; size as usize]
}

#[test]
fn no_tty_refuses() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip("/", &["empty"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("refusing to delete without confirmation (use -y)"),
        "{}",
        stderr(&out)
    );

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(
        trash_host.join("files/x").is_file(),
        "nothing must be deleted"
    );
    assert!(trash_host.join("info/x.trashinfo").is_file());
}

#[test]
fn plain_empty_everything() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();

    // Home trash: an item, a plain orphan, a malformed-info orphan, a
    // dangling info, and a stale staging box.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"item",
        b"/home/u/Downloads/item",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    plant_orphan(&sandbox, "/home/u/.local/share/Trash", b"orphan", b"data");
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"bad-info",
        b"", // an empty Path= is malformed (info::original rejects it)
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dangling",
        b"/home/u/Downloads/dangling",
        "2026-01-01T00:00:00",
        Body::Missing,
    );
    plant_staging_box(&sandbox, "/home/u/.local/share/Trash", "put.999.0", b"x");

    // A topdir trash (.Trash-<uid> on another filesystem): one item.
    let other_trash = format!("/mnt/other/.Trash-{uid}");
    sandbox.plant(
        &other_trash,
        b"topdir-item",
        b"sub/topdir-item",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    // A stray bind-root trash (yazi-style, at a mount point that is not the
    // home trash's own filesystem): one item.
    let stray_trash = format!("/home/u/Downloads/.Trash-{uid}");
    sandbox.plant(
        &stray_trash,
        b"stray-item",
        b"stray-item",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("rip: deleted"), "{}", stderr(&out));

    let home_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(!home_host.join("files/item").exists());
    assert!(!home_host.join("info/item.trashinfo").exists());
    assert!(!home_host.join("files/orphan").exists());
    assert!(!home_host.join("files/bad-info").exists());
    assert!(!home_host.join("info/bad-info.trashinfo").exists());
    assert!(!home_host.join("info/dangling.trashinfo").exists());
    let staging = fs::read_dir(home_host.join(".rip-staging"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(staging, 0, "staging must end up empty");

    let other_host = sandbox.host(&other_trash);
    assert!(!other_host.join("files/topdir-item").exists());
    assert!(!other_host.join("info/topdir-item.trashinfo").exists());

    let stray_host = sandbox.host(&stray_trash);
    assert!(!stray_host.join("files/stray-item").exists());
    assert!(!stray_host.join("info/stray-item.trashinfo").exists());
}

#[test]
fn older_than() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"old",
        b"/home/u/Downloads/old",
        &days_ago(40),
        Body::File(b"data".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"new",
        b"/home/u/Downloads/new",
        &days_ago(10),
        Body::File(b"data".to_vec()),
    );
    plant_orphan(
        &sandbox,
        "/home/u/.local/share/Trash",
        b"fresh-orphan",
        b"data",
    );

    let out = sandbox.rip("/", &["empty", "--older-than", "30d", "-y"]);
    assert_ok(&out);

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(
        !trash_host.join("files/old").exists(),
        "an item 40 days old must go under --older-than 30d"
    );
    assert!(
        trash_host.join("files/new").is_file(),
        "an item 10 days old must survive --older-than 30d"
    );
    assert!(
        trash_host.join("files/fresh-orphan").is_file(),
        "a fresh orphan must survive --older-than 30d"
    );

    let out = sandbox.rip("/", &["empty", "--older-than", "0s", "-y"]);
    assert_ok(&out);
    assert!(
        !trash_host.join("files/fresh-orphan").exists(),
        "even a fresh orphan is older than --older-than 0s"
    );
}

#[test]
fn max_size() {
    let sandbox = Sandbox::artemis();
    let sizes: [(&str, &str, u64); 4] = [
        ("a", "2026-01-04T00:00:00", 1024),
        ("b", "2026-01-03T00:00:00", 1024),
        ("c", "2026-01-02T00:00:00", 2048),
        ("d", "2026-01-01T00:00:00", 1024),
    ];
    for (name, date, size) in sizes {
        sandbox.plant(
            "/home/u/.local/share/Trash",
            name.as_bytes(),
            format!("/home/u/Downloads/{name}").as_bytes(),
            date,
            Body::File(file_of(size)),
        );
    }

    // a(1024) + b(1024) = 2048 <= 3K; + c(2048) = 4096 > 3K: c overflows,
    // and c and d (the older, smaller one) both go.
    let out = sandbox.rip("/", &["empty", "--max-size", "3K", "-y"]);
    assert_ok(&out);

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(trash_host.join("files/a").is_file(), "the newest must stay");
    assert!(
        trash_host.join("files/b").is_file(),
        "the second newest must stay"
    );
    assert!(
        !trash_host.join("files/c").exists(),
        "the item that overflows the budget must go"
    );
    assert!(
        !trash_host.join("files/d").exists(),
        "an older item, even a smaller one, must go once the budget overflowed"
    );
}

#[test]
fn max_size_newest_too_big() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"huge",
        b"/home/u/Downloads/huge",
        "2026-01-01T00:00:00",
        Body::File(file_of(4096)),
    );

    let out = sandbox.rip(
        "/",
        &["empty", "--max-size", "1K", "--older-than", "0s", "-y"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(
        trash_host.join("files/huge").is_file(),
        "nothing may be deleted when the newest item alone exceeds --max-size"
    );
    assert!(trash_host.join("info/huge.trashinfo").is_file());
}

#[test]
fn union_of_filters() {
    let sandbox = Sandbox::artemis();
    // "a" survives both filters. "b" is old enough for --older-than alone,
    // but --max-size alone would not reach it (it only starts dooming at
    // "c"): the union must still catch it. Dates are relative to now, since
    // --older-than's cutoff is computed from the real current time.
    let items: [(&str, i64, u64); 4] =
        [("a", 1, 512), ("b", 6, 512), ("c", 7, 4096), ("d", 8, 512)];
    for (name, age_days, size) in items {
        sandbox.plant(
            "/home/u/.local/share/Trash",
            name.as_bytes(),
            format!("/home/u/Downloads/{name}").as_bytes(),
            &days_ago(age_days),
            Body::File(file_of(size)),
        );
    }

    let out = sandbox.rip(
        "/",
        &["empty", "--older-than", "4d", "--max-size", "3K", "-y"],
    );
    assert_ok(&out);

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(trash_host.join("files/a").is_file(), "kept by both filters");
    assert!(
        !trash_host.join("files/b").exists(),
        "caught by --older-than alone"
    );
    assert!(!trash_host.join("files/c").exists());
    assert!(!trash_host.join("files/d").exists());
}

#[test]
fn dangling_always_removed() {
    let sandbox = Sandbox::artemis();
    // Kept under this filter (it is not old enough)...
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"kept",
        b"/home/u/Downloads/kept",
        &days_ago(1),
        Body::File(b"data".to_vec()),
    );
    // ...but the dangling info must still go, unconditionally.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dangling",
        b"/home/u/Downloads/dangling",
        "2026-01-01T00:00:00",
        Body::Missing,
    );

    let out = sandbox.rip("/", &["empty", "--older-than", "30d", "-y"]);
    assert_ok(&out);

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(
        trash_host.join("files/kept").is_file(),
        "the filter must still be respected for real items"
    );
    assert!(
        !trash_host.join("info/dangling.trashinfo").exists(),
        "a dangling info is removed by every empty, regardless of the filter"
    );
}

#[test]
fn never_crosses_mounts() {
    let mut sandbox = Sandbox::artemis();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"still here").unwrap();
    sandbox.bind(outside.path(), "/home/u/.local/share/Trash/files/dir/m");

    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dir",
        b"/home/u/Downloads/dir",
        "2026-01-01T00:00:00",
        Body::Dir,
    );

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "an item containing a mount point must be skipped, not deleted: {}",
        stderr(&out)
    );

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(
        trash_host.join("files/dir").is_dir(),
        "the trashed directory must survive"
    );
    assert!(trash_host.join("info/dir.trashinfo").is_file());
    assert_eq!(
        fs::read(outside.path().join("keep")).unwrap(),
        b"still here"
    );
}

#[test]
fn never_follows_symlinks() {
    let sandbox = Sandbox::artemis();
    let target_dir = tempfile::tempdir().unwrap();
    fs::write(target_dir.path().join("secret"), b"precious").unwrap();

    // A trashed item that is itself a symlink to a directory outside the
    // trash: removing the link must never touch what it points to.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"link-item",
        b"/home/u/Downloads/link-item",
        "2026-01-01T00:00:00",
        Body::Symlink(target_dir.path().to_path_buf()),
    );

    // A trashed directory containing a symlink that points outward.
    let target_file = tempfile::NamedTempFile::new().unwrap();
    fs::write(target_file.path(), b"also precious").unwrap();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"outdir",
        b"/home/u/Downloads/outdir",
        "2026-01-02T00:00:00",
        Body::Dir,
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    std::os::unix::fs::symlink(target_file.path(), trash_host.join("files/outdir/outward"))
        .unwrap();

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);

    assert!(!trash_host.join("files/link-item").exists());
    assert!(!trash_host.join("files/outdir").exists());
    assert_eq!(
        fs::read(target_dir.path().join("secret")).unwrap(),
        b"precious"
    );
    assert_eq!(fs::read(target_file.path()).unwrap(), b"also precious");
}

#[test]
fn readonly_dirs_deleted() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"stubborn",
        b"/home/u/Downloads/stubborn",
        "2026-01-01T00:00:00",
        Body::Dir,
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    let item = trash_host.join("files/stubborn");
    let ro = item.join("ro");
    let wo = item.join("wo");
    fs::create_dir(&ro).unwrap();
    fs::write(ro.join("f"), b"x").unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    fs::create_dir(&wo).unwrap();
    fs::write(wo.join("f"), b"x").unwrap();
    fs::set_permissions(&wo, fs::Permissions::from_mode(0o311)).unwrap();

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);
    assert!(
        !item.exists(),
        "the item and its restrictive-mode subdirectories must be fully removed"
    );
}

#[test]
fn waits_for_lock() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");

    // A shared lock taken directly on the host path: the sandbox's own
    // /home/u/.local/share/Trash is a bind of this same host directory, so
    // this is visible to (and blocks) the sandboxed rip's exclusive lock.
    let lock_file = fs::File::open(&trash_host).unwrap();
    rustix::fs::flock(&lock_file, FlockOperation::LockShared).unwrap();

    let handle = std::thread::spawn(move || sandbox.rip("/", &["empty", "-y"]));

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !handle.is_finished(),
        "empty should still be waiting for the lock"
    );

    rustix::fs::flock(&lock_file, FlockOperation::Unlock).unwrap();
    drop(lock_file);

    let out = handle.join().expect("the empty thread panicked");
    assert_ok(&out);
    assert!(
        stderr(&out).contains("waiting"),
        "expected a 'waiting for another rip' message: {}",
        stderr(&out)
    );
    assert!(!trash_host.join("files/x").exists());
}

#[test]
fn crash_states() {
    let sandbox = Sandbox::artemis();

    // A dangling info: no matching files/ entry.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dangling",
        b"/home/u/Downloads/dangling",
        "2026-01-01T00:00:00",
        Body::Missing,
    );

    // A stale copy-fallback box, as a crash mid-copy would leave one.
    plant_staging_box(
        &sandbox,
        "/home/u/.local/share/Trash",
        "put.111.0",
        b"unfinished copy",
    );

    // A stale tombstone, as a crash between the rename and the removal of a
    // previous `empty` would leave one.
    plant_staging_box(
        &sandbox,
        "/home/u/.local/share/Trash",
        "del.222.0",
        b"half-removed",
    );

    // A completed item whose original path already independently has a
    // file, as a crash right after restore's rename-back but before its
    // info was unlinked would leave (docs/design.md §5.3 "Restore crash
    // consistency"). `empty` must delete the trash copy exactly like any
    // other item, and must never touch the original path.
    let original_host = sandbox.host("/home/u/Downloads").join("already-restored");
    fs::write(&original_host, b"the restored copy").unwrap();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"already-restored",
        b"/home/u/Downloads/already-restored",
        "2026-01-01T00:00:00",
        Body::File(b"the trash copy".to_vec()),
    );

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(!trash_host.join("info/dangling.trashinfo").exists());
    let staging = fs::read_dir(trash_host.join(".rip-staging"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(staging, 0, "both stale boxes must be cleaned up");
    assert!(!trash_host.join("files/already-restored").exists());
    assert!(!trash_host.join("info/already-restored.trashinfo").exists());
    assert_eq!(
        fs::read(&original_host).unwrap(),
        b"the restored copy",
        "empty must never touch a file at an original path"
    );
}

/// c6 (review round, fix-empty.json): tombstone naming must not restart at
/// `del.<pid>.0` for every item in a batch (quadratic `renameat2` retries
/// under `LOCK_EX`). With the bug, N items cost N(N+1)/2 renames; fixed,
/// the cost is linear. 4000 items take tens of seconds under the bug and
/// well under a second fixed.
#[test]
fn empty_of_many_items_is_not_quadratic() {
    let sandbox = Sandbox::artemis();
    let n = 4000;
    for i in 0..n {
        let name = format!("f{i:05}");
        sandbox.plant(
            "/home/u/.local/share/Trash",
            name.as_bytes(),
            format!("/home/u/Downloads/{name}").as_bytes(),
            "2026-01-01T00:00:00",
            Body::File(b"x".to_vec()),
        );
    }

    let start = std::time::Instant::now();
    let out = sandbox.rip("/", &["empty", "-y"]);
    let elapsed = start.elapsed();
    assert_ok(&out);
    assert!(
        elapsed < Duration::from_secs(5),
        "empty of {n} items in one trash dir took {elapsed:?}: tombstone naming must \
         not restart at 0 for every item (review round, finding c6)"
    );

    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(!trash_host.join("files/f00000").exists());
    assert!(!trash_host.join(format!("files/f{:05}", n - 1)).exists());
}

/// c19 (review round, fix-empty.json): a top-level directory item without
/// owner write permission must still be removable -- unlike renaming any
/// other entry kind, renaming a directory to a new parent needs write
/// permission on the directory itself, to update "..".
#[test]
fn readonly_top_level_dir_deleted() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"ro",
        b"/home/u/Downloads/ro",
        "2026-01-01T00:00:00",
        Body::Dir,
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    let item = trash_host.join("files/ro");
    fs::write(item.join("f"), b"x").unwrap();
    fs::set_permissions(&item, fs::Permissions::from_mode(0o555)).unwrap();

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);
    assert!(
        !item.exists(),
        "a top-level item without owner write permission must still be removable"
    );
}

/// c17 (review round, fix-empty.json): an info file named `...trashinfo`
/// strips to `..`. As a directory-entry name, `..` would resolve to the
/// trash root itself: this must be treated as garbage `empty` can unlink
/// outright, never resolved against `files/`, so it does not keep failing
/// every `empty` forever.
#[test]
fn dotdot_named_info_is_garbage_not_the_trash_root() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"keep",
        b"/home/u/Downloads/keep",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    fs::write(
        trash_host.join("info/...trashinfo"),
        b"[Trash Info]\nPath=phantom\nDeletionDate=2026-01-02T00:00:00\n",
    )
    .unwrap();

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);
    assert!(
        !trash_host.join("info/...trashinfo").exists(),
        "the malformed info must be removed as garbage"
    );
    assert!(!trash_host.join("files/keep").exists());

    // A second empty must not keep failing on the same phantom entry.
    let out2 = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out2);
}

/// c18 (review round, fix-empty.json): `files/NAME` exists, but its info
/// cannot be opened at all (mode `000`; not a race, since `files/NAME` is
/// present). This must still be removable -- as an orphan with a malformed
/// info -- instead of an entry nothing can ever delete.
#[test]
fn unopenable_info_with_existing_files_entry_is_still_removable() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"ok",
        b"/home/u/Downloads/ok",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"stuck",
        b"/home/u/Downloads/stuck",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");
    fs::set_permissions(
        trash_host.join("info/stuck.trashinfo"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();

    let list_out = sandbox.rip("/", &["list", "--all"]);
    assert!(
        stderr(&list_out).contains("malformed .trashinfo"),
        "expected a warning about the unopenable info: {}",
        stderr(&list_out)
    );

    let out = sandbox.rip("/", &["empty", "-y"]);
    assert_ok(&out);
    assert!(!trash_host.join("files/ok").exists());
    assert!(
        !trash_host.join("files/stuck").exists(),
        "an entry whose info exists but cannot be opened must still be removable: {}",
        stderr(&out)
    );
    assert!(!trash_host.join("info/stuck.trashinfo").exists());
}

/// c21 (review round, fix-empty.json): `empty`'s unlocked load can see a
/// concurrent put's reserved info before its rename lands, classifying it
/// Dangling; once `empty` gets `LOCK_EX`, the put has finished. This must
/// be a silent no-op, not a reported failed delete (exit 1) about the file
/// the user just trashed.
#[test]
fn dangling_completed_by_concurrent_put_is_not_a_failure() {
    let sandbox = Sandbox::artemis();
    // An info-only entry, as `put`'s reserve() leaves one while its rename
    // into files/ is still pending (docs/design.md §5.4): from the outside
    // this looks dangling.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"inflight",
        b"/home/u/Downloads/inflight",
        &days_ago(0),
        Body::Missing,
    );
    let trash_host = sandbox.host("/home/u/.local/share/Trash");

    // Hold LOCK_SH on the trash dir, exactly as put.rs's move_in does while
    // its info is reserved but not yet renamed into files/: empty's own
    // unlocked load then sees "inflight" as dangling.
    let lock_file = fs::File::open(&trash_host).unwrap();
    rustix::fs::flock(&lock_file, FlockOperation::LockShared).unwrap();

    // `thread::scope` (not `thread::spawn(move || ...)`): `sandbox` must
    // outlive this whole test, including the assertions below, or its own
    // `Drop` (which removes its temp dirs) would run as soon as the
    // spawned closure returns, wiping the trash out from under those
    // assertions before they run -- masking the very outcome this test
    // checks for.
    let out = std::thread::scope(|s| {
        let handle = s.spawn(|| sandbox.rip("/", &["empty", "--older-than", "30d", "-y"]));

        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !handle.is_finished(),
            "empty should still be waiting for the lock"
        );

        // The concurrent put finishes: the rename lands.
        fs::write(trash_host.join("files/inflight"), b"user data").unwrap();
        rustix::fs::flock(&lock_file, FlockOperation::Unlock).unwrap();

        handle.join().expect("the empty thread panicked")
    });
    drop(lock_file);

    assert_ok(&out);
    assert!(
        !stderr(&out).contains("could not delete"),
        "a dangling info a concurrent put completed must not be reported as a \
         failed delete: {}",
        stderr(&out)
    );
    assert!(
        trash_host.join("files/inflight").is_file(),
        "the just-trashed item must survive intact"
    );
    assert!(trash_host.join("info/inflight.trashinfo").is_file());
}

// ---------------------------------------------------------------------------
// c20 (review round, fix-empty.json)
// ---------------------------------------------------------------------------

/// The inside paths `Sandbox::command` binds by default, in the order it
/// binds them (mirrors tests/restore.rs's own `DEFAULT_BINDS`).
const DEFAULT_BINDS: &[&str] = &[
    "/persist",
    "/home",
    "/home/u/Downloads",
    "/home/u/Documents",
    "/home/u/.local/share/Trash",
    "/mnt/side",
    "/mnt/pside",
    "/mnt/other",
    "/mnt/ro",
];

/// Like `Sandbox::rip`, but mounts `ro_inside` (one of `DEFAULT_BINDS`) as a
/// genuinely read-only bind (bubblewrap `--ro-bind`, so a write there gives
/// `EROFS`) instead of read-write. `Sandbox`'s own `bind`/`without` cannot
/// express this -- unlike the sandbox's own always-present `/mnt/ro`, whose
/// read-only-ness is only a `0555` directory mode (`EACCES`, not `EROFS`) --
/// and tests/empty.rs may not edit tests/common/mod.rs (review round,
/// finding c20; pattern copied from tests/restore.rs's own `base_bwrap`).
fn rip_with_one_bind_readonly(
    sandbox: &Sandbox,
    ro_inside: &str,
    cwd: &str,
    args: &[&str],
) -> Output {
    let mut c = Command::new("bwrap");
    c.args([
        "--unshare-user",
        "--unshare-pid",
        "--die-with-parent",
        "--new-session",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--ro-bind",
        "/nix",
        "/nix",
        "--ro-bind",
        "/etc",
        "/etc",
    ]);
    for p in ["/usr", "/bin", "/lib", "/lib64"] {
        c.args(["--ro-bind-try", p, p]);
    }
    c.arg("--ro-bind")
        .arg(env!("CARGO_BIN_EXE_rip"))
        .arg("/run/rip/bin/rip");
    for inside in DEFAULT_BINDS {
        let flag = if *inside == ro_inside {
            "--ro-bind"
        } else {
            "--bind"
        };
        c.arg(flag).arg(sandbox.host(inside)).arg(inside);
    }
    c.args([
        "--clearenv",
        "--setenv",
        "HOME",
        "/home/u",
        "--setenv",
        "XDG_CONFIG_HOME",
        "/home/u/.config",
    ]);
    let mut path_entries = vec![PathBuf::from("/run/rip/bin")];
    if let Some(host_path) = std::env::var_os("PATH") {
        path_entries
            .extend(std::env::split_paths(&host_path).filter(|p| p.starts_with("/nix/store")));
    }
    c.arg("--setenv")
        .arg("PATH")
        .arg(std::env::join_paths(path_entries).expect("PATH entries must not contain ':' or NUL"));
    c.args(["--chdir", cwd, "--", "/run/rip/bin/rip"]);
    c.args(args);
    c.stdin(Stdio::null());
    c.output()
        .expect("spawn bwrap for rip_with_one_bind_readonly")
}

/// A trash dir on a genuinely read-only mount, with nothing selected in it,
/// must not fail `empty` merely because `.rip-staging` cannot be created
/// there.
#[test]
fn readonly_topdir_trash_with_nothing_selected_does_not_fail() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"old",
        b"/home/u/Downloads/old",
        &days_ago(40),
        Body::File(b"data".to_vec()),
    );
    let ro_trash = format!("/mnt/other/.Trash-{uid}");
    sandbox.plant(
        &ro_trash,
        b"fresh",
        b"sub/fresh",
        &days_ago(1),
        Body::File(b"data".to_vec()),
    );

    let out = rip_with_one_bind_readonly(
        &sandbox,
        "/mnt/other",
        "/",
        &["empty", "--older-than", "30d", "-y"],
    );
    assert_ok(&out);

    let home_host = sandbox.host("/home/u/.local/share/Trash");
    assert!(
        !home_host.join("files/old").exists(),
        "the home trash's old item must still be deleted"
    );
    let ro_host = sandbox.host(&ro_trash);
    assert!(
        ro_host.join("files/fresh").is_file(),
        "the fresh item on the read-only trash must be kept, untouched"
    );
}
