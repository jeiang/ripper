//! `rip FILE...` (docs/design.md §3, §5.1, §5.2). Every test runs
//! `rip` inside the bwrap sandbox (tests/common), never against a real trash.
//! "Same inode" (`inode()` equal) proves a rename; a different inode proves
//! a copy. `inode()` includes the device: inode numbers on two different
//! filesystems can be equal by chance.

mod common;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{Body, Sandbox};
use rustix::process::getuid;

fn inode(path: &Path) -> (u64, u64) {
    let m =
        std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    (m.dev(), m.ino())
}

fn read_info(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "expected success, exit={:?} stdout={:?} stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_fail(out: &Output) {
    assert!(
        !out.status.success(),
        "expected failure, stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn empty_dir(path: &Path) -> bool {
    !path.exists() || std::fs::read_dir(path).unwrap().next().is_none()
}

// ---------------------------------------------------------------------------
// Placement: rename vs. copy, per source (docs/design.md §3.2)
// ---------------------------------------------------------------------------

#[test]
fn same_bind_renames() {
    // A layout variant where the trash and the file being trashed share one
    // single bind, so the rename needs no routing through another mount at
    // all.
    let mut sandbox = Sandbox::artemis();
    let local_share_host = sandbox.host("/persist").join("u/.local/share");
    sandbox.without("/home/u/.local/share/Trash");
    sandbox.bind(local_share_host, "/home/u/.local/share");

    let target = sandbox.host("/home/u/.local/share").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/home/u/.local/share", &["x"]);
    assert_ok(&out);

    let trash = sandbox.host("/home/u/.local/share/Trash");
    let trashed = trash.join("files/x");
    assert!(trashed.is_file());
    assert_eq!(inode(&trashed), before, "must be a rename, not a copy");
    assert!(!target.exists());

    let info = read_info(&trash.join("info/x.trashinfo"));
    assert!(info.contains("Path=/home/u/.local/share/x"), "{info}");
    let mode = std::fs::metadata(trash.join("info/x.trashinfo"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn downloads_via_persist() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let target = sandbox.host("/home/u/Downloads").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/home/u/Downloads", &["x"]);
    assert_ok(&out);

    let trash = sandbox.host("/home/u/.local/share/Trash");
    let trashed = trash.join("files/x");
    assert!(trashed.is_file());
    assert_eq!(inode(&trashed), before);
    assert!(!target.exists());
    assert!(
        !sandbox
            .host("/home/u/Downloads")
            .join(format!(".Trash-{uid}"))
            .exists(),
        "no topdir trash must be created on the home trash's own filesystem"
    );
    let info = read_info(&trash.join("info/x.trashinfo"));
    assert!(info.contains("Path=/home/u/Downloads/x"), "{info}");
}

#[test]
fn pside_bind_root_uses_home_trash() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let target = sandbox.host("/mnt/pside").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/mnt/pside", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file());
    assert_eq!(
        inode(&trashed),
        before,
        "same subvolume as the home trash: must rename"
    );
    assert!(
        !sandbox
            .host("/mnt/pside")
            .join(format!(".Trash-{uid}"))
            .exists()
    );
}

#[test]
fn side_bind_root_copies_no_topdir_trash() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let target = sandbox.host("/mnt/side").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/mnt/side", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file());
    assert_ne!(
        inode(&trashed),
        before,
        "another subvolume: must copy, not rename"
    );
    assert!(!target.exists());
    assert!(
        !sandbox
            .host("/mnt/side")
            .join(format!(".Trash-{uid}"))
            .exists(),
        "no topdir trash on the home trash's own filesystem"
    );
}

#[test]
fn ephemeral_root_copies() {
    let sandbox = Sandbox::artemis();
    let tree = sandbox.host("/home/u").join("foo");
    // create_dir_all: unlike Downloads/Documents/Trash, nothing pre-creates
    // the ephemeral root's "u" directory on the host side before the first
    // bwrap invocation runs.
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("exec"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(tree.join("exec"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("exec", tree.join("link")).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        tree.join("fifo"),
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .unwrap();
    let before_mtime = std::fs::symlink_metadata(tree.join("exec"))
        .unwrap()
        .mtime();

    let out = sandbox.rip("/home/u", &["foo"]);
    assert_ok(&out);

    assert!(!tree.exists());
    let trash = sandbox.host("/home/u/.local/share/Trash");
    let copied = trash.join("files/foo");
    assert!(copied.is_dir());
    let exec_meta = std::fs::metadata(copied.join("exec")).unwrap();
    assert_eq!(exec_meta.permissions().mode() & 0o777, 0o755);
    assert_eq!(exec_meta.mtime(), before_mtime);
    assert_eq!(
        std::fs::read_link(copied.join("link")).unwrap(),
        Path::new("exec")
    );
    assert!(
        std::fs::symlink_metadata(copied.join("fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert!(empty_dir(&trash.join(".rip-staging")), "no box left behind");

    let info = read_info(&trash.join("info/foo.trashinfo"));
    assert!(info.contains("Path=/home/u/foo"), "{info}");
}

#[test]
fn copy_hardlinked_tree() {
    let sandbox = Sandbox::artemis();
    let dir = sandbox.host("/home/u").join("hl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a"), b"data").unwrap();
    std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();

    let out = sandbox.rip("/home/u", &["hl"]);
    assert_ok(&out);
    assert!(!dir.exists());

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/hl");
    assert_eq!(std::fs::read(trashed.join("a")).unwrap(), b"data");
    assert_eq!(std::fs::read(trashed.join("b")).unwrap(), b"data");
    assert_eq!(inode(&trashed.join("a")), inode(&trashed.join("b")));
}

#[test]
fn no_common_mount_copies() {
    let mut sandbox = Sandbox::artemis();
    sandbox.without("/persist");
    let target = sandbox.host("/home/u/Downloads").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/home/u/Downloads", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file());
    assert_ne!(
        inode(&trashed),
        before,
        "no mount shows both paths: must copy"
    );
    assert!(!target.exists());
}

#[test]
fn covered_persist_not_used() {
    let mut sandbox = Sandbox::artemis();
    // Isolate the case: no other route candidate should exist besides
    // /persist itself.
    sandbox.without("/mnt/side");
    sandbox.without("/mnt/pside");
    let cover = tempfile::tempdir().unwrap();
    std::fs::write(cover.path().join("marker"), b"cover").unwrap();
    sandbox.bind(cover.path(), "/persist");

    let target = sandbox.host("/home/u/Downloads").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/home/u/Downloads", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file());
    assert_ne!(inode(&trashed), before, "/persist is covered: must copy");
    assert!(!target.exists());
    assert!(
        cover.path().join("marker").is_file(),
        "the covering dir must be untouched"
    );
}

#[test]
fn other_fs_topdir_trash() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let sub = sandbox.host("/mnt/other").join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("x"), b"hello").unwrap();
    let before = inode(&sub.join("x"));

    let out = sandbox.rip("/mnt/other/sub", &["x"]);
    assert_ok(&out);

    let trash_dir = sandbox.host("/mnt/other").join(format!(".Trash-{uid}"));
    let mode = std::fs::metadata(&trash_dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    let trashed = trash_dir.join("files/x");
    assert_eq!(inode(&trashed), before);
    let info = read_info(&trash_dir.join("info/x.trashinfo"));
    assert!(info.contains("Path=sub/x"), "{info}");
}

#[test]
fn skipped_half_trash_is_still_refused_not_repaired_and_reused() {
    // docs/design.md §1: discovery skips a `.Trash-$uid` that is missing
    // its info/ subdirectory (it warns and moves on), but topdir_trash's own
    // placement logic would happily repair and reuse that very directory.
    // An operand already inside it must still be refused, not silently
    // re-trashed into the trash that holds it.
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let trash_dir = sandbox.host("/mnt/other").join(format!(".Trash-{uid}"));
    std::fs::create_dir_all(trash_dir.join("files")).unwrap();
    std::fs::write(trash_dir.join("files/foo"), b"trashed earlier").unwrap();
    // Deliberately no info/: this is what discovery skips with a warning.

    let out = sandbox.rip("/mnt/other", &["-v", &format!(".Trash-{uid}/files/foo")]);
    assert_fail(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("it is inside the trash at") && stderr.contains("rip purge"),
        "{stderr}"
    );
    assert!(
        !trash_dir.join("info").exists(),
        "must not have repaired the missing info/ while refusing"
    );
    assert!(
        trash_dir.join("files/foo").is_file(),
        "the original entry must be left exactly where it was"
    );
    assert!(
        !trash_dir.join("files/foo~1").exists(),
        "must not have been re-trashed into the same directory"
    );
}

#[test]
fn admin_trash_sticky_dir_is_used() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let admin = sandbox.host("/mnt/other").join(".Trash");
    std::fs::create_dir(&admin).unwrap();
    std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o1777)).unwrap();
    std::fs::write(sandbox.host("/mnt/other").join("x"), b"hello").unwrap();

    let out = sandbox.rip("/mnt/other", &["x"]);
    assert_ok(&out);

    assert!(admin.join(uid.to_string()).join("files/x").is_file());
    assert!(
        !sandbox
            .host("/mnt/other")
            .join(format!(".Trash-{uid}"))
            .exists()
    );
}

#[test]
fn admin_trash_non_sticky_falls_back_with_warning() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    // 0755: exists, but not sticky.
    std::fs::create_dir(sandbox.host("/mnt/other").join(".Trash")).unwrap();
    std::fs::write(sandbox.host("/mnt/other").join("x"), b"hello").unwrap();

    let out = sandbox.rip("/mnt/other", &["x"]);
    assert_ok(&out);

    let trashed = sandbox
        .host("/mnt/other")
        .join(format!(".Trash-{uid}/files/x"));
    assert!(trashed.is_file());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a sticky directory"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn invalid_user_trash_symlink_falls_back() {
    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let real_target = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        real_target.path(),
        sandbox.host("/mnt/other").join(format!(".Trash-{uid}")),
    )
    .unwrap();
    let target = sandbox.host("/mnt/other").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/mnt/other", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file());
    assert_ne!(inode(&trashed), before);
    assert!(!target.exists());
    assert!(
        std::fs::read_dir(real_target.path())
            .unwrap()
            .next()
            .is_none(),
        "a symlinked .Trash-U must never be followed into"
    );
}

#[test]
fn topdir_trash_swap_race_never_creates_outside_the_trash() {
    // docs/design.md §3.1: topdir_trash must open/create `.Trash-$uid` (and
    // files/, info/) fd-relative, never by re-resolving a path, so a symlink
    // a concurrent writer swaps in for it is refused by O_NOFOLLOW on the
    // reopen, never followed. A host thread races a tight
    // renameat2(RENAME_EXCHANGE) loop, swapping a real (used) `.Trash-$uid`
    // for a symlink to `victim`, against many `rip` invocations; whichever
    // way each one lands, `victim` (reachable only through the symlink) must
    // never receive a files/info subdirectory.
    use std::sync::atomic::{AtomicBool, Ordering};

    let uid = getuid().as_raw();
    let sandbox = Sandbox::artemis();
    let other = sandbox.host("/mnt/other");
    let real_trash = other.join(format!(".Trash-{uid}"));
    std::fs::create_dir_all(real_trash.join("files")).unwrap();
    std::fs::create_dir_all(real_trash.join("info")).unwrap();
    let victim = other.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    let swap_name = other.join(format!(".Trash-{uid}.swap"));
    // A relative target ("victim", a sibling of `swap_name` in the same
    // directory): it resolves the same way whether dereferenced from the
    // host (this process, running the swap loop) or from inside the bwrap
    // sandbox (where `rip` actually runs), unlike an absolute host path,
    // which would not exist inside the sandbox's own mount namespace at all.
    std::os::unix::fs::symlink("victim", &swap_name).unwrap();

    const ROUNDS: usize = 400;
    for i in 0..ROUNDS {
        std::fs::write(other.join(format!("x{i}")), b"hello").unwrap();
    }

    let racing = AtomicBool::new(true);
    std::thread::scope(|s| {
        s.spawn(|| {
            while racing.load(Ordering::Relaxed) {
                let _ = rustix::fs::renameat_with(
                    rustix::fs::CWD,
                    &real_trash,
                    rustix::fs::CWD,
                    &swap_name,
                    rustix::fs::RenameFlags::EXCHANGE,
                );
            }
        });
        for i in 0..ROUNDS {
            let _ = sandbox.rip("/mnt/other", &["-v", &format!("x{i}")]);
        }
        racing.store(false, Ordering::Relaxed);
    });

    assert!(
        std::fs::read_dir(&victim).unwrap().next().is_none(),
        "a directory reached only through the swapped-in symlink must never \
         receive files/info: topdir_trash must never re-resolve a path after \
         checking it"
    );
}

#[test]
fn unwritable_topdir_copies() {
    let sandbox = Sandbox::artemis();
    let work = sandbox.host("/mnt/ro").join("work");
    let target = work.join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);
    let ro_root = sandbox.host("/mnt/ro");
    let before_mode = std::fs::metadata(&ro_root).unwrap().permissions().mode() & 0o777;

    let out = sandbox.rip("/mnt/ro/work", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file());
    assert_ne!(inode(&trashed), before);
    assert!(!target.exists());
    let after_mode = std::fs::metadata(&ro_root).unwrap().permissions().mode() & 0o777;
    assert_eq!(before_mode, after_mode);
    assert_eq!(before_mode, 0o555);
}

#[test]
fn bind_over_topdir_trash_exdev() {
    let uid = getuid().as_raw();
    let mut sandbox = Sandbox::artemis();
    std::fs::write(sandbox.host("/mnt/other").join("seed"), b"seed").unwrap();
    let out = sandbox.rip("/mnt/other", &["seed"]);
    assert_ok(&out);
    let trash_dir = sandbox.host("/mnt/other").join(format!(".Trash-{uid}"));
    assert!(trash_dir.join("files/seed").is_file());

    // Cover the just-created topdir trash with a bind from elsewhere: a
    // later put must re-verify its mount identity (step 4) and fall back
    // to a copy instead of renaming into a directory that changed
    // underneath it.
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(elsewhere.path().join("files")).unwrap();
    std::fs::create_dir_all(elsewhere.path().join("info")).unwrap();
    sandbox.bind(elsewhere.path(), &format!("/mnt/other/.Trash-{uid}"));

    let target = sandbox.host("/mnt/other").join("x");
    std::fs::write(&target, b"hello").unwrap();
    let before = inode(&target);

    let out = sandbox.rip("/mnt/other", &["x"]);
    assert_ok(&out);

    let trashed = sandbox.host("/home/u/.local/share/Trash").join("files/x");
    assert!(trashed.is_file(), "expected the home trash to hold a copy");
    assert_ne!(inode(&trashed), before);
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------
// Fallback config, copy threshold, copy rollback (docs/design.md §3.1, §5.2)
// ---------------------------------------------------------------------------

#[test]
fn fallback_refuse() {
    let sandbox = Sandbox::artemis();
    sandbox.config("fallback = \"refuse\"\n");
    let target = sandbox.host("/home/u").join("x");
    std::fs::write(&target, b"hello").unwrap();

    let out = sandbox.rip("/home/u", &["x"]);
    assert_fail(&out);
    assert!(target.is_file(), "the source must be left untouched");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("fallback"), "{stderr}");
    assert!(empty_dir(&sandbox.host("/home/u/.local/share/Trash/files")));
}

#[test]
fn copy_threshold() {
    let sandbox = Sandbox::artemis();
    sandbox.config("copy_threshold = \"1K\"\n");
    let base = sandbox.host("/home/u");
    let trash_files = sandbox.host("/home/u/.local/share/Trash/files");

    // No terminal: fails, untouched.
    std::fs::write(base.join("a"), vec![b'x'; 4096]).unwrap();
    let out = sandbox.rip("/home/u", &["a"]);
    assert_fail(&out);
    assert!(base.join("a").is_file());

    // -f: copies without asking.
    let out = sandbox.rip("/home/u", &["-f", "a"]);
    assert_ok(&out);
    assert!(!base.join("a").exists());
    assert!(trash_files.join("a").is_file());

    // Terminal, "n": declined, untouched, exit 0.
    std::fs::write(base.join("b"), vec![b'x'; 4096]).unwrap();
    let out = sandbox.rip_tty("/home/u", &["b"], "n\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(base.join("b").is_file());

    // Terminal, "y": copies.
    let out = sandbox.rip_tty("/home/u", &["b"], "y\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!base.join("b").exists());
    assert!(trash_files.join("b").is_file());
}

#[test]
fn copy_rollback_unreadable_file() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u");
    let dir = base.join("d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("secret"), b"x").unwrap();
    std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = sandbox.rip("/home/u", &["d"]);
    std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0o600)).unwrap();

    assert_fail(&out);
    assert!(dir.is_dir());
    assert!(dir.join("secret").is_file());
    let trash = sandbox.host("/home/u/.local/share/Trash");
    assert!(!trash.join("files/d").exists());
    assert!(!trash.join("info/d.trashinfo").exists());
    assert!(empty_dir(&trash.join(".rip-staging")));
}

#[test]
fn copy_refused_unwritable_subdir() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u");
    let dir = base.join("d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/f"), b"x").unwrap();
    std::fs::set_permissions(dir.join("sub"), std::fs::Permissions::from_mode(0o555)).unwrap();

    let out = sandbox.rip("/home/u", &["d"]);
    std::fs::set_permissions(dir.join("sub"), std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_fail(&out);
    assert!(dir.is_dir());
    assert!(dir.join("sub/f").is_file());
    assert!(
        !sandbox
            .host("/home/u/.local/share/Trash/files")
            .join("d")
            .exists()
    );
}

// ---------------------------------------------------------------------------
// Collisions, non-UTF-8, symlinks (docs/design.md §4)
// ---------------------------------------------------------------------------

#[test]
fn collisions_and_name_max() {
    let sandbox = Sandbox::artemis();
    let trash = sandbox.host("/home/u/.local/share/Trash");
    std::fs::create_dir_all(trash.join("files")).unwrap();
    std::fs::create_dir_all(trash.join("info")).unwrap();
    // A planted orphan at x~2: reserve() must skip it (no info) when
    // choosing the next free name for a real collision.
    std::fs::write(trash.join("files/x~2"), b"orphan").unwrap();

    let base = sandbox.host("/home/u/Downloads");
    for _ in 0..3 {
        std::fs::write(base.join("x"), b"data").unwrap();
        let out = sandbox.rip("/home/u/Downloads", &["x"]);
        assert_ok(&out);
    }

    for n in ["x", "x~1", "x~3"] {
        assert!(trash.join("files").join(n).is_file(), "{n}");
        assert!(
            trash.join("info").join(format!("{n}.trashinfo")).is_file(),
            "{n}"
        );
    }
    assert_eq!(
        std::fs::read(trash.join("files/x~2")).unwrap(),
        b"orphan",
        "the planted orphan must survive untouched"
    );
    assert!(!trash.join("info/x~2.trashinfo").exists());

    // A 255-byte name collides twice and stays within NAME_MAX including
    // the .trashinfo suffix.
    let long_name = "a".repeat(255);
    for _ in 0..2 {
        std::fs::write(base.join(&long_name), b"1").unwrap();
        let out = sandbox.rip("/home/u/Downloads", &[long_name.as_str()]);
        assert_ok(&out);
    }
    let entries: Vec<String> = std::fs::read_dir(trash.join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('a'))
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");
    for n in &entries {
        assert!(n.len() + ".trashinfo".len() <= 255, "{n}: {}", n.len());
    }
}

#[test]
fn non_utf8() {
    let sandbox = Sandbox::artemis();
    let bad = OsStr::from_bytes(&[b'c', b'a', b'f', 0xE9]);
    let mut info_name = bad.as_bytes().to_vec();
    info_name.extend_from_slice(b".trashinfo");
    let info_name = OsStr::from_bytes(&info_name).to_owned();

    // Rename path.
    let downloads = sandbox.host("/home/u/Downloads");
    std::fs::write(downloads.join(bad), b"1").unwrap();
    let out = sandbox.rip("/home/u/Downloads", &[bad]);
    assert_ok(&out);
    let trash = sandbox.host("/home/u/.local/share/Trash");
    assert!(trash.join("files").join(bad).exists());
    let info = std::fs::read(trash.join("info").join(&info_name)).unwrap();
    let text = String::from_utf8_lossy(&info);
    assert!(text.contains("caf%E9"), "{text}");

    // Copy path.
    let root = sandbox.host("/home/u");
    std::fs::write(root.join(bad), b"2").unwrap();
    let out = sandbox.rip("/home/u", &[bad]);
    assert_ok(&out);
}

#[test]
fn symlinks() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    let real_dir = base.join("realdir");
    std::fs::create_dir(&real_dir).unwrap();
    std::fs::write(real_dir.join("keep"), b"data").unwrap();
    std::os::unix::fs::symlink("realdir", base.join("link")).unwrap();
    std::os::unix::fs::symlink("nowhere", base.join("dangling")).unwrap();
    let trash = sandbox.host("/home/u/.local/share/Trash");

    let out = sandbox.rip("/home/u/Downloads", &["link"]);
    assert_ok(&out);
    assert!(!base.join("link").exists());
    assert!(real_dir.is_dir());
    assert!(real_dir.join("keep").is_file());
    assert!(
        std::fs::symlink_metadata(trash.join("files/link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let out = sandbox.rip("/home/u/Downloads", &["dangling"]);
    assert_ok(&out);
    assert!(!base.join("dangling").exists());
    assert!(std::fs::symlink_metadata(trash.join("files/dangling")).is_ok());

    // `link/`: refused.
    std::os::unix::fs::symlink("realdir", base.join("link2")).unwrap();
    let out = sandbox.rip("/home/u/Downloads", &["link2/"]);
    assert_fail(&out);
    assert!(base.join("link2").exists());
    assert!(real_dir.is_dir());
}

// ---------------------------------------------------------------------------
// Refusals, resolution order, force/verbose, prompts (docs/design.md §1)
// ---------------------------------------------------------------------------

#[test]
fn refusals() {
    let sandbox = Sandbox::artemis();
    let trash = sandbox.host("/home/u/.local/share/Trash");
    std::fs::create_dir_all(trash.join("files")).unwrap();
    std::fs::create_dir_all(trash.join("info")).unwrap();
    std::fs::write(trash.join("files/x"), b"trashed").unwrap();
    std::fs::write(
        trash.join("info/x.trashinfo"),
        b"[Trash Info]\nPath=/home/u/Downloads/x\nDeletionDate=2026-01-01T00:00:00\n",
    )
    .unwrap();

    let cases: &[(&str, &str)] = &[
        ("/home/u", "/"),
        ("/home/u", "."),
        ("/home/u", ".."),
        ("/home/u/Downloads", "realdir/."),
        ("/", "/home/u/Downloads"),
        ("/", "/home/u"),
        ("/", "/persist/u/Downloads"),
        ("/", "/home/u/.local/share/Trash/files/x"),
        ("/", "/persist/u/.local/share/Trash/files/x"),
        ("/", "/home/u/.local/share/Trash"),
        ("/", "/home/u/.local/share"),
    ];
    std::fs::create_dir_all(sandbox.host("/home/u/Downloads").join("realdir")).unwrap();

    for (cwd, arg) in cases {
        let out = sandbox.rip(cwd, &[*arg]);
        assert_fail(&out);
    }
    assert_eq!(
        std::fs::read(trash.join("files/x")).unwrap(),
        b"trashed",
        "the host tree must stay byte-identical"
    );
    assert!(sandbox.host("/home/u/Downloads").join("realdir").is_dir());
}

#[test]
fn parent_then_child() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    std::fs::create_dir(base.join("dir")).unwrap();
    std::fs::write(base.join("dir/f"), b"data").unwrap();

    let out = sandbox.rip("/home/u/Downloads", &["dir", "dir/f"]);
    assert_fail(&out); // the second operand fails: exit 1 overall

    let trash = sandbox.host("/home/u/.local/share/Trash");
    assert!(trash.join("files/dir/f").is_file());
    assert!(!base.join("dir").exists());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("dir/f"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn missing_and_force() {
    let sandbox = Sandbox::artemis();
    let no_args: [&str; 0] = [];

    let out = sandbox.rip("/home/u", &["does-not-exist"]);
    assert_fail(&out);

    let out = sandbox.rip("/home/u", &["-f", "does-not-exist"]);
    assert_ok(&out);
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = sandbox.rip("/home/u", &["-f"]);
    assert_eq!(out.status.code(), Some(0));

    let out = sandbox.rip("/home/u", &no_args);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn force_ignores_enotdir_like_rm_but_still_refuses_trailing_slash() {
    // docs/design.md §1: `rip -f f/x`, where `f` is a regular file, fails
    // to resolve with ENOTDIR. GNU `rm -f` treats that the same as a
    // missing path (its own nonexistent_file_errno list includes ENOTDIR)
    // and exits 0 silently; `-f` must do the same. The trailing-slash
    // refusal (`f/`, a deliberate rip-specific extra refusal, docs/design.md
    // §1) is a different case and must still fail even under `-f`.
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    std::fs::write(base.join("f"), b"x").unwrap();

    let out = sandbox.rip("/home/u/Downloads", &["-f", "f/x"]);
    assert_ok(&out);
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(base.join("f").is_file(), "f itself must be untouched");

    let out = sandbox.rip("/home/u/Downloads", &["-f", "f/x/y"]);
    assert_ok(&out);

    let out = sandbox.rip("/home/u/Downloads", &["-f", "f/"]);
    assert_fail(&out);
    assert!(
        base.join("f").is_file(),
        "the trailing-slash refusal must still leave f untouched"
    );
}

#[test]
fn interactive() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    for n in ["a", "b"] {
        std::fs::write(base.join(n), b"x").unwrap();
    }
    let trash = sandbox.host("/home/u/.local/share/Trash");

    // -i: one prompt per item, "y" then "n".
    let out = sandbox.rip_tty("/home/u/Downloads", &["-i", "a", "b"], "y\nn\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(trash.join("files/a").exists());
    assert!(base.join("b").exists());
    assert!(!trash.join("files/b").exists());

    // -I: more than 3 items, one prompt for the whole batch.
    for n in ["c", "d", "e"] {
        std::fs::write(base.join(n), b"x").unwrap();
    }
    let out = sandbox.rip_tty("/home/u/Downloads", &["-I", "b", "c", "d", "e"], "y\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    for n in ["b", "c", "d", "e"] {
        assert!(!base.join(n).exists(), "{n}");
    }

    // -I with no terminal: nothing trashed, exit 1.
    for n in ["f", "g", "h", "i"] {
        std::fs::write(base.join(n), b"x").unwrap();
    }
    let out = sandbox.rip("/home/u/Downloads", &["-I", "f", "g", "h", "i"]);
    assert_fail(&out);
    for n in ["f", "g", "h", "i"] {
        assert!(base.join(n).exists(), "{n} must be untouched");
    }

    // -i -f and -f -i never prompt.
    let out = sandbox.rip("/home/u/Downloads", &["-i", "-f", "f"]);
    assert_ok(&out);
    let out = sandbox.rip("/home/u/Downloads", &["-f", "-i", "g"]);
    assert_ok(&out);
}

#[test]
fn one_date_per_invocation() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    std::fs::write(base.join("a"), b"1").unwrap();
    std::fs::write(base.join("b"), b"2").unwrap();

    let before = sandbox.exec("/", &["date", "+%Y-%m-%dT%H:%M:%S"]);
    assert!(before.status.success());
    let out = sandbox.rip("/home/u/Downloads", &["a", "b"]);
    assert_ok(&out);
    let after = sandbox.exec("/", &["date", "+%Y-%m-%dT%H:%M:%S"]);
    assert!(after.status.success());

    let trash = sandbox.host("/home/u/.local/share/Trash");
    let extract = |text: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix("DeletionDate="))
            .unwrap()
            .to_string()
    };
    let da = extract(&read_info(&trash.join("info/a.trashinfo")));
    let db = extract(&read_info(&trash.join("info/b.trashinfo")));
    assert_eq!(da, db, "one DeletionDate per invocation");

    let before = String::from_utf8_lossy(&before.stdout).trim().to_string();
    let after = String::from_utf8_lossy(&after.stdout).trim().to_string();
    assert!(
        da.as_str() >= before.as_str() && da.as_str() <= after.as_str(),
        "DeletionDate {da} not within [{before}, {after}]"
    );
}

#[test]
fn parallel_puts_same_name() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    for i in 0..8 {
        let dir = base.join(format!("src{i}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("x"), format!("data{i}")).unwrap();
    }

    let outs: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let sandbox = &sandbox;
                let cwd = format!("/home/u/Downloads/src{i}");
                scope.spawn(move || sandbox.rip(&cwd, &["x"]))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for out in &outs {
        assert_ok(out);
    }

    let trash = sandbox.host("/home/u/.local/share/Trash");
    let mut names: Vec<String> = std::fs::read_dir(trash.join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        8,
        "8 concurrent puts of 'x' must get 8 unique names: {names:?}"
    );
    for n in &names {
        assert!(
            trash.join("info").join(format!("{n}.trashinfo")).is_file(),
            "{n} has no matching .trashinfo"
        );
    }
}

#[test]
fn argv_rules() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    let trash = sandbox.host("/home/u/.local/share/Trash");

    std::fs::write(base.join("empty"), b"x").unwrap();
    let out = sandbox.rip("/home/u/Downloads", &["-rf", "empty"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(base.join("empty").exists());
    assert!(empty_dir(&trash.join("files")));

    let out = sandbox.rip("/home/u/Downloads", &["--", "empty"]);
    assert_ok(&out);
    assert!(!base.join("empty").exists());
    assert!(trash.join("files/empty").exists());

    std::fs::write(base.join("foo"), b"x").unwrap();
    std::fs::write(base.join("empty"), b"x").unwrap();
    let out = sandbox.rip("/home/u/Downloads", &["foo", "-f", "empty"]);
    assert_ok(&out);
    assert!(!base.join("foo").exists());
    assert!(!base.join("empty").exists());
    assert!(trash.join("files/foo").exists());

    let before: Vec<String> = std::fs::read_dir(trash.join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    std::fs::write(base.join("foo2"), b"x").unwrap();
    let out = sandbox.rip(
        "/home/u/Downloads",
        &["foo2", "--config", "c", "empty", "-y"],
    );
    assert_eq!(out.status.code(), Some(2), "{:?}", out);
    assert!(
        base.join("foo2").exists(),
        "-y is not a root flag: nothing must be trashed"
    );
    let after: Vec<String> = std::fs::read_dir(trash.join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(before.len(), after.len(), "the trash must be unchanged");
}

// ---------------------------------------------------------------------------
// Read-only mounts (docs/design.md §1): a real read-only MOUNT (mountinfo's
// `ro` option), not a permission-based restriction, which `Sandbox`'s own
// `bind`/`without` API cannot express. This mirrors tests/restore.rs's own
// `base_bwrap`, built locally rather than by editing tests/common/mod.rs.
// ---------------------------------------------------------------------------

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

/// The same fixed bwrap flags and binds `Sandbox::command` uses, plus `rip`'s
/// own binary bound at the usual inside path. Callers add any extra binds
/// (e.g. a genuine `--ro-bind`) and finish the invocation themselves.
fn base_bwrap(sandbox: &Sandbox) -> Command {
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
        c.arg("--bind").arg(sandbox.host(inside)).arg(inside);
    }
    c
}

/// Finishes a `base_bwrap` command (env, cwd, argv) and runs it with stdin
/// `/dev/null`, never a terminal.
fn run_bwrap(mut c: Command, cwd: &str, args: &[&str]) -> Output {
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
    c.arg("--chdir").arg(cwd);
    c.arg("--").arg("/run/rip/bin/rip").args(args);
    c.stdin(Stdio::null());
    c.output().expect("spawn bwrap")
}

#[test]
fn read_only_mount_is_refused_not_routed_around() {
    // A directory on the home trash's own subvolume, exposed through a
    // genuinely read-only MOUNT (not just permission bits), must be refused
    // like `rm` would -- not bypassed by renaming through /persist, a
    // writable alias of the same subvolume (docs/design.md §1).
    let sandbox = Sandbox::artemis();
    let ro_host = sandbox.host("/persist").join("u/roview");
    std::fs::create_dir_all(&ro_host).unwrap();
    std::fs::write(ro_host.join("x"), b"protected").unwrap();

    let mut c = base_bwrap(&sandbox);
    c.arg("--ro-bind").arg(&ro_host).arg("/home/u/roview");
    let out = run_bwrap(c, "/home/u/roview", &["-v", "x"]);

    assert_fail(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("its filesystem is read-only"), "{stderr}");
    assert!(
        ro_host.join("x").exists(),
        "the source must be left in place"
    );
    assert!(
        !sandbox
            .host("/home/u/.local/share/Trash/files")
            .join("x")
            .exists(),
        "must not have been routed around the read-only view into the trash"
    );
}

// ---------------------------------------------------------------------------
// Hostile output (docs/design.md §0 invariant 10): a name with terminal escapes must
// never reach a real terminal raw, in any human-facing message put.rs
// builds. Run on a real pty (`rip_tty`) the same way `list`'s own escaping
// test does, since a plain pipe does not exercise terminal semantics.
// ---------------------------------------------------------------------------

#[test]
fn verbose_rename_destination_escapes_a_hostile_name() {
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u/Downloads");
    let name = "x\x1b]0;PWNED\x07\x1b[2J";
    std::fs::write(base.join(name), b"hello").unwrap();

    let out = sandbox.rip_tty("/home/u/Downloads", &["-v", name], "");
    assert!(out.status.success());
    let tty = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tty.contains('\x1b'),
        "raw ESC reached the terminal: {tty:?}"
    );
    assert!(tty.contains("\\x1b"), "{tty:?}");
}

#[test]
fn copy_fallback_notice_and_verbose_destination_escape_a_hostile_name() {
    // Exercises both the "has no usable trash on its filesystem" notice and
    // -v's "copied 'X' -> DEST" line, both printed from copy_to_home.
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u");
    std::fs::create_dir_all(&base).unwrap();
    let name = "y\x1b]0;PWNED\x07";
    std::fs::write(base.join(name), b"data").unwrap();

    let out = sandbox.rip_tty("/home/u", &["-v", name], "");
    assert!(out.status.success());
    let tty = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tty.contains('\x1b'),
        "raw ESC reached the terminal: {tty:?}"
    );
    assert!(tty.contains("\\x1b"), "{tty:?}");
}

#[test]
fn walk_problem_message_escapes_a_hostile_entry_name() {
    // sys.rs's walk-problem messages (an unreadable entry inside a tree
    // taking the copy fallback) embed the entry's own name too.
    let sandbox = Sandbox::artemis();
    let base = sandbox.host("/home/u");
    std::fs::create_dir_all(base.join("top")).unwrap();
    let name = "bad\x1b]0;PWNED\x07";
    let secret = base.join("top").join(name);
    std::fs::write(&secret, b"x").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = sandbox.rip_tty("/home/u", &["top"], "");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert!(!out.status.success());
    let tty = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tty.contains('\x1b'),
        "raw ESC reached the terminal: {tty:?}"
    );
    assert!(tty.contains("\\x1b"), "{tty:?}");
}

// ---------------------------------------------------------------------------
// A deleted cwd (docs/design.md §2.2): each `Sandbox::rip` call starts a
// fresh process with a fresh, valid cwd, so reproducing this needs one bwrap
// session that runs several `rip` invocations from a single shell, the
// first of which trashes the directory that shell is sitting in.
// ---------------------------------------------------------------------------

/// Like `run_bwrap`, but runs `bash -c script` instead of a single `rip`
/// invocation: every command in `script` shares this one process's cwd, so
/// once the script itself trashes that directory (through the copy
/// fallback, which genuinely unlinks it rather than just renaming it
/// elsewhere), every later command in the script runs with a cwd `getcwd`
/// can no longer resolve at all.
fn run_bwrap_shell(sandbox: &Sandbox, cwd: &str, script: &str) -> Output {
    let mut c = base_bwrap(sandbox);
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
    c.args(["--chdir", cwd, "--", "bash", "-c", script]);
    c.stdin(Stdio::null());
    c.output().expect("spawn bwrap for run_bwrap_shell")
}

#[test]
fn commands_run_from_a_deleted_cwd_do_not_exit_2() {
    let sandbox = Sandbox::artemis();
    // An older item, distinguishable from D by DeletionDate, restorable by
    // an absolute PATH once the shell's cwd is gone.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"old",
        b"/home/u/Documents/old",
        "2020-01-01T00:00:00",
        Body::File(b"OLD ITEM".to_vec()),
    );

    // "/mnt/side" is another subvolume of the home trash's own filesystem
    // (docs/design.md §3.2): trashing a directory there takes the
    // copy fallback, which `remove_tree`s the original outright, instead of
    // a same-subvolume rename that would just leave it reachable under a
    // new name in files/. A shell sitting in that directory then loses
    // `getcwd()` entirely, not just its displayed path.
    let d = sandbox.host("/mnt/side").join("D");
    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("f"), b"D's own file").unwrap();

    let script = "\
/run/rip/bin/rip ../D; echo TRASH_EXIT=$?
/run/rip/bin/rip list --all; echo LIST_EXIT=$?
/run/rip/bin/rip undo -y; echo UNDO_EXIT=$?
/run/rip/bin/rip restore -y /home/u/Documents/old; echo RESTORE_EXIT=$?
";
    let out = run_bwrap_shell(&sandbox, "/mnt/side/D", script);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the session's own shell failed: {text} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    for marker in [
        "TRASH_EXIT=0",
        "LIST_EXIT=0",
        "UNDO_EXIT=0",
        "RESTORE_EXIT=0",
    ] {
        assert!(text.contains(marker), "{marker} missing from: {text}");
    }
    assert!(
        !text.contains("EXIT=2"),
        "a command run from the deleted cwd exited 2: {text}"
    );

    // D itself came back through `undo` (it is the newest DeletionDate:
    // just trashed by this same script, versus "old"'s 2020 date).
    assert_eq!(std::fs::read(d.join("f")).unwrap(), b"D's own file");

    // "old" came back through the absolute-path restore.
    let old = sandbox.host("/home/u/Documents/old");
    assert_eq!(std::fs::read(&old).unwrap(), b"OLD ITEM");
}
