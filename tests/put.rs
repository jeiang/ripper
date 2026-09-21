//! `rip FILE...` (docs/design.md §13.3 "tests/put.rs"). Every test runs
//! `rip` inside the bwrap sandbox (tests/common), never against a real trash.
//! "Same inode" (`inode()` equal) proves a rename; a different inode proves
//! a copy.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Output;

use common::Sandbox;
use rustix::process::getuid;

fn inode(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .ino()
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
// Placement: the rename path, through a containing mount or a topdir trash
// (docs/design.md §3.2, §13.3). The copy fallback lands, with its own
// tests, in the next commit.
// ---------------------------------------------------------------------------

#[test]
fn same_bind_renames() {
    // A layout variant where the trash and the file being trashed share one
    // single bind, so the rename needs no routing through another mount at
    // all (docs/design.md §13.3).
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

// ---------------------------------------------------------------------------
// Collisions and symlinks (docs/design.md §4, §6.1, §13.3)
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
// Refusals, resolution order, force/verbose, prompts (docs/design.md §5.3,
// §6.1, §6.2)
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
