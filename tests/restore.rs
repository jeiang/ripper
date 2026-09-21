//! `rip restore`, `rip undo` and `rip purge` (docs/design.md §7, §13.3
//! "tests/restore.rs"). Every test runs `rip` inside the bwrap sandbox
//! (tests/common), never against a real trash. Fixtures are planted with
//! `Sandbox::plant` throughout (put.rs lands in a parallel checkpoint and is
//! not available here).

mod common;

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{Body, Sandbox};
use rustix::process::getuid;

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

fn assert_ok(out: &Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{ctx}: exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_fails(out: &Output, ctx: &str) {
    assert!(
        !out.status.success(),
        "{ctx}: expected failure, got exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `(dev, ino)`: inode numbers on two filesystems can be equal by chance.
fn ino(path: &Path) -> (u64, u64) {
    let m =
        std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    (m.dev(), m.ino())
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn uid_string() -> String {
    getuid().as_raw().to_string()
}

/// The item's on-disk trash path (`TRASHDIR/files/NAME`) for a trash rooted
/// at `trash_inside` (an inside path as `Sandbox::plant`'s first argument
/// takes it, e.g. `/home/u/.local/share/Trash`), already resolved to its
/// HOST path via `Sandbox::host` (`trash_inside` and every `Sandbox`-known
/// bind prefix are plain UTF-8; only a trashed *name* is ever non-UTF-8).
fn trash_host_path(sandbox: &Sandbox, trash_inside: &str, name: &[u8]) -> PathBuf {
    sandbox
        .host(trash_inside)
        .join("files")
        .join(OsStr::from_bytes(name))
}

// ---------------------------------------------------------------------------
// A size-limited tmpfs for a real ENOSPC (design §13.3
// "copy_back_enospc_keeps_item"), which `Sandbox`'s own `bind`/`without`
// cannot express (they only add host-directory `--bind`s; bubblewrap's own
// `--size BYTES --tmpfs DEST` has no equivalent there, and tests/restore.rs
// may not edit tests/common/mod.rs). This reconstructs the handful of fixed
// bwrap flags `Sandbox::command` also uses, plus the *default* artemis
// binds by their well-known inside paths (read back through `Sandbox`'s own
// public `host()`), so it only works against a freshly built, unmodified
// `Sandbox::artemis()` -- never after `.bind()`/`.without()` have changed
// the layout. The `rip` invocation and a follow-up `ls` both run inside ONE
// bwrap session (via a small bash script), so the size-limited tmpfs is
// still mounted when the listing runs, even though it (and anything the
// failed copy left inside it) vanishes the moment this process exits.
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

/// The scaffolding common to every custom launcher below: the same fixed
/// bwrap flags and binds `Sandbox::command` uses, `rip`'s own binary bound
/// at the usual inside path, and a `PATH` of just `/run/rip/bin` plus the
/// host's own `/nix/store` entries -- deliberately WITHOUT
/// `/run/rip/fakebin` (the fake `fzf`), which is the one thing no
/// `Sandbox` method can omit. Callers finish the invocation themselves
/// (`--chdir`, `--`, argv).
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
    c
}

/// Runs, in one bwrap session: `rip` with `rip_args`, then `find
/// /mnt/small` so the test can see what a failed copy left behind before
/// the size-limited tmpfs disappears. `rip`'s own stdout/stderr are not
/// separated from the trailing shell's (both go to this process's
/// stdout/stderr, combined with the `find` output), so assertions look for
/// substrings rather than exact equality.
fn rip_small_tmpfs_session(sandbox: &Sandbox, tmpfs_size: u64, rip_args: &str) -> Output {
    let mut c = base_bwrap(sandbox);
    c.arg("--size")
        .arg(tmpfs_size.to_string())
        .arg("--tmpfs")
        .arg("/mnt/small");
    let script = format!(
        "/run/rip/bin/rip {rip_args}; echo RIP_EXIT=$?; echo SMALL_LISTING:; find /mnt/small"
    );
    c.args(["--chdir", "/", "--", "bash", "-c", &script]);
    c.stdin(Stdio::null());
    c.output().expect("spawn bwrap for rip_small_tmpfs_session")
}

/// Single-quotes `s` for a POSIX shell command line (the only quoting
/// `rip_without_fzf_tty` needs: every argument it is ever given is a plain
/// path or flag, with no embedded single quotes).
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A `bash` under `/nix/store` on the host's own `PATH` (NixOS has no
/// `/bin/sh`), for `script -qec`'s `SHELL` (mirrors
/// tests/common/mod.rs's own `host_bash`, which is private to that module).
fn host_bash() -> PathBuf {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("bash");
            if let Ok(real) = candidate.canonicalize() {
                if real.starts_with("/nix/store") && real.is_file() {
                    return real;
                }
            }
        }
    }
    panic!("rip_without_fzf_tty: no `bash` under /nix/store on PATH; run inside `nix develop`");
}

/// Runs `rip` on a real pty (so `pick()`'s own terminal check passes and it
/// actually tries to run fzf), in a bwrap session whose `PATH` has no `fzf`
/// at all -- the one environment `Sandbox::rip_tty` can never produce,
/// since its bwrap invocation always binds the fake `fzf` (design §13.4
/// "An irreversible action without a terminal" is `Sandbox::rip`'s job; a
/// genuinely missing `fzf` needs this).
fn rip_without_fzf_tty(sandbox: &Sandbox, cwd: &str, args: &[&str]) -> Output {
    let c = base_bwrap(sandbox);
    let raw: Vec<&OsStr> = c.get_args().collect();
    let mut quoted = String::from("bwrap");
    let mut i = 0;
    while i < raw.len() {
        // `base_bwrap`'s forwarded PATH always includes the dev shell's
        // OWN /nix/store entries, which (unlike the fake fzf in
        // /run/rip/fakebin) includes a REAL fzf -- exactly the thing this
        // launcher exists to make unavailable. Rewrite that one `--setenv
        // PATH <value>` triple with a filtered value instead of forwarding
        // it verbatim.
        if i + 2 < raw.len() && raw[i] == OsStr::new("--setenv") && raw[i + 1] == OsStr::new("PATH")
        {
            let filtered: Vec<PathBuf> = std::env::split_paths(&raw[i + 2])
                .filter(|p| !p.to_string_lossy().contains("fzf"))
                .collect();
            let value = std::env::join_paths(filtered).unwrap();
            quoted.push_str(" --setenv PATH ");
            quoted.push_str(&sq(&value.to_string_lossy()));
            i += 3;
            continue;
        }
        quoted.push(' ');
        quoted.push_str(&sq(&raw[i].to_string_lossy()));
        i += 1;
    }
    quoted.push_str(" --chdir ");
    quoted.push_str(&sq(cwd));
    quoted.push_str(" -- /run/rip/bin/rip");
    for a in args {
        quoted.push(' ');
        quoted.push_str(&sq(a));
    }
    Command::new("script")
        .arg("-qec")
        .arg(&quoted)
        .arg("/dev/null")
        .env("SHELL", host_bash())
        .stdin(Stdio::null())
        .output()
        .expect("spawn `script` for rip_without_fzf_tty")
}

// ---------------------------------------------------------------------------
// by_original_path (design §13.3, §13.4 "Restore overwrites a file" table)
// ---------------------------------------------------------------------------

#[test]
fn by_original_path() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"relative".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"y",
        b"/home/u/Downloads/y",
        "2026-01-01T00:00:00",
        Body::File(b"absolute".to_vec()),
    );
    let trash_x = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"x");
    let trash_y = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"y");
    let ino_x = ino(&trash_x);
    let ino_y = ino(&trash_y);

    // Relative PATH.
    let out = sandbox.rip("/home/u/Downloads", &["restore", "x"]);
    assert_ok(&out, "restore x (relative)");
    assert_eq!(stdout_str(&out), "x\n", "printed relative to cwd");
    let dest_x = sandbox.host("/home/u/Downloads").join("x");
    assert_eq!(std::fs::read(&dest_x).unwrap(), b"relative");
    assert_eq!(ino(&dest_x), ino_x, "must be a rename, not a copy");
    assert!(!&trash_x.exists());
    assert!(
        !sandbox
            .host("/home/u/.local/share/Trash")
            .join("info/x.trashinfo")
            .exists()
    );

    // Absolute PATH, cwd elsewhere.
    let out = sandbox.rip("/", &["restore", "/home/u/Downloads/y"]);
    assert_ok(&out, "restore y (absolute)");
    assert_eq!(stdout_str(&out), "home/u/Downloads/y\n");
    let dest_y = sandbox.host("/home/u/Downloads").join("y");
    assert_eq!(std::fs::read(&dest_y).unwrap(), b"absolute");
    assert_eq!(ino(&dest_y), ino_y);
    assert!(!&trash_y.exists());
}

// ---------------------------------------------------------------------------
// via_persist: the destination is reached through a DIFFERENT bind of the
// trash's own subvolume (design §5.2's route(), reversed).
// ---------------------------------------------------------------------------

#[test]
fn via_persist() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"z",
        b"/mnt/pside/z",
        "2026-01-01T00:00:00",
        Body::File(b"pside".to_vec()),
    );
    let trash_z = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"z");
    let original_ino = ino(&trash_z);

    let out = sandbox.rip("/", &["restore", "/mnt/pside/z"]);
    assert_ok(&out, "restore via /mnt/pside");
    let dest = sandbox.host("/mnt/pside").join("z");
    assert_eq!(std::fs::read(&dest).unwrap(), b"pside");
    assert_eq!(ino(&dest), original_ino, "must be a rename, not a copy");
    assert!(!&trash_z.exists());
}

// ---------------------------------------------------------------------------
// copy_back: an ephemeral-root round trip (no common mount with the trash,
// so it must copy).
// ---------------------------------------------------------------------------

#[test]
fn copy_back() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"w",
        b"/home/u/w",
        "2026-01-01T00:00:00",
        Body::File(b"ephemeral".to_vec()),
    );
    let trash_w = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"w");
    let original_ino = ino(&trash_w);

    let out = sandbox.rip("/", &["restore", "-y", "/home/u/w"]);
    assert_ok(&out, "copy_back restore");
    let dest = sandbox.host("/home/u").join("w");
    assert_eq!(std::fs::read(&dest).unwrap(), b"ephemeral");
    assert_ne!(ino(&dest), original_ino, "must be a copy, not a rename");
    assert!(!&trash_w.exists());
    assert!(
        !sandbox
            .host("/home/u/.local/share/Trash")
            .join("info/w.trashinfo")
            .exists()
    );
    // No stray .rip-restore.* box left next to the destination.
    let leftovers: Vec<_> = std::fs::read_dir(sandbox.host("/home/u"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".rip-restore"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// ---------------------------------------------------------------------------
// copy_back_prompt: terminal y/n, -y, and no terminal (design §7.2, §13.4
// "An irreversible action without a terminal").
// ---------------------------------------------------------------------------

#[test]
fn copy_back_prompt() {
    let sandbox = Sandbox::artemis();
    sandbox.config("copy_threshold = \"1\"\n");
    for name in ["a", "b", "c", "d"] {
        sandbox.plant(
            "/home/u/.local/share/Trash",
            name.as_bytes(),
            format!("/home/u/{name}").as_bytes(),
            "2026-01-01T00:00:00",
            Body::File(b"content".to_vec()),
        );
    }

    // No terminal: refuses, item stays.
    let out = sandbox.rip("/", &["restore", "/home/u/a"]);
    assert_fails(&out, "no-tty copy prompt");
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"a").exists());
    assert!(!sandbox.host("/home/u").join("a").exists());

    // -y: skips the prompt.
    let out = sandbox.rip("/", &["restore", "-y", "/home/u/b"]);
    assert_ok(&out, "-y copy prompt");
    assert!(sandbox.host("/home/u").join("b").exists());

    // Terminal, "n": declines, exit 0, item stays.
    let out = sandbox.rip_tty("/", &["restore", "/home/u/c"], "n\n");
    assert!(out.status.success(), "declined prompt must exit 0: {out:?}");
    assert!(!sandbox.host("/home/u").join("c").exists());
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"c").exists());

    // Terminal, "y": confirms, item restored.
    let out = sandbox.rip_tty("/", &["restore", "/home/u/d"], "y\n");
    assert!(
        out.status.success(),
        "confirmed prompt must exit 0: {out:?}"
    );
    assert!(sandbox.host("/home/u").join("d").exists());
}

// ---------------------------------------------------------------------------
// copy_back_enospc_keeps_item / created_parents_rolled_back: a real ENOSPC
// partway through the copy, via the size-limited tmpfs helper above.
// ---------------------------------------------------------------------------

#[test]
fn copy_back_enospc_keeps_item() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"big",
        b"/mnt/small/big",
        "2026-01-01T00:00:00",
        Body::File(vec![b'x'; 64 * 1024]),
    );

    let out = rip_small_tmpfs_session(&sandbox, 4096, "restore -y /mnt/small/big");
    let out_text = stdout_str(&out);
    assert!(
        out_text.contains("RIP_EXIT=1"),
        "expected a controlled failure (exit 1), not a crash or hang: {out_text}\nstderr: {}",
        stderr_str(&out)
    );
    // The size-limited tmpfs is gone with the process, so this is the only
    // point at which its contents (or lack of them) can still be observed:
    // nothing survived the failed copy inside it.
    assert!(
        !out_text.contains("big") && !out_text.contains(".rip-restore"),
        "no box or destination should remain in the size-limited tmpfs: {out_text}"
    );

    // The trashed item itself, on the real (host-backed) home trash, must
    // be completely untouched.
    let trash_big = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"big");
    assert!(&trash_big.exists());
    assert_eq!(std::fs::metadata(&trash_big).unwrap().len(), 64 * 1024);
    let out = sandbox.rip("/", &["list", "--all"]);
    assert_ok(&out, "list after enospc restore");
    assert!(
        stdout_str(&out).contains("mnt/small/big"),
        "{}",
        stdout_str(&out)
    );
}

#[test]
fn created_parents_rolled_back() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"deep",
        b"/mnt/small/newdir/deep",
        "2026-01-01T00:00:00",
        Body::File(vec![b'y'; 64 * 1024]),
    );

    let out = rip_small_tmpfs_session(&sandbox, 4096, "restore -y /mnt/small/newdir/deep");
    let out_text = stdout_str(&out);
    assert!(out_text.contains("RIP_EXIT=1"), "{out_text}");
    // `find /mnt/small` after the failed restore must show only the tmpfs
    // root itself: the "newdir" ensure_parent created must have been
    // rmdir'd again, not left behind empty.
    let listing = out_text.split("SMALL_LISTING:").nth(1).unwrap_or("").trim();
    assert_eq!(
        listing, "/mnt/small",
        "created parent directories must be rolled back: {out_text}"
    );

    let trash_deep = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"deep");
    assert!(&trash_deep.exists());
}

// ---------------------------------------------------------------------------
// creates_parents (the success path: missing parents are created).
// ---------------------------------------------------------------------------

#[test]
fn creates_parents() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/newdir/sub/x",
        "2026-01-01T00:00:00",
        Body::File(b"nested".to_vec()),
    );

    let out = sandbox.rip("/", &["restore", "/home/u/Downloads/newdir/sub/x"]);
    assert_ok(&out, "creates_parents");
    let dest = sandbox.host("/home/u/Downloads").join("newdir/sub/x");
    assert_eq!(std::fs::read(&dest).unwrap(), b"nested");
}

// ---------------------------------------------------------------------------
// conflict_refused_and_rename
// ---------------------------------------------------------------------------

#[test]
fn conflict_refused_and_rename() {
    let sandbox = Sandbox::artemis();
    std::fs::write(sandbox.host("/home/u/Downloads").join("x"), b"original").unwrap();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"item1",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"first".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"item2",
        b"/home/u/Downloads/x",
        "2026-01-02T00:00:00",
        Body::File(b"second".to_vec()),
    );

    // Plain restore of a variant, by trash path, refused: the conflicting
    // file must be untouched.
    let out = sandbox.rip("/", &["restore", "/home/u/.local/share/Trash/files/item1"]);
    assert_fails(&out, "conflict without --rename");
    assert!(
        stderr_str(&out).contains("--rename"),
        "{}",
        stderr_str(&out)
    );
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("x")).unwrap(),
        b"original"
    );
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"item1").exists());

    // --rename: x~1.
    let out = sandbox.rip(
        "/",
        &[
            "restore",
            "--rename",
            "/home/u/.local/share/Trash/files/item1",
        ],
    );
    assert_ok(&out, "restore --rename item1");
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("x~1")).unwrap(),
        b"first"
    );

    // --rename again: x~2 (x and x~1 both exist now).
    let out = sandbox.rip(
        "/",
        &[
            "restore",
            "--rename",
            "/home/u/.local/share/Trash/files/item2",
        ],
    );
    assert_ok(&out, "restore --rename item2");
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("x~2")).unwrap(),
        b"second"
    );
}

// ---------------------------------------------------------------------------
// by_trash_path
// ---------------------------------------------------------------------------

#[test]
fn by_trash_path() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x~1",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"exact".to_vec()),
    );
    // An orphan (malformed info: empty Path=) cannot be restored, only purged.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"orphan1",
        b"",
        "2026-01-01T00:00:00",
        Body::File(b"orphan".to_vec()),
    );

    let out = sandbox.rip("/", &["restore", "/home/u/.local/share/Trash/files/x~1"]);
    assert_ok(&out, "restore by exact trash path");
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("x")).unwrap(),
        b"exact"
    );

    let out = sandbox.rip(
        "/",
        &["restore", "/home/u/.local/share/Trash/files/orphan1"],
    );
    assert_fails(&out, "restore an orphan by trash path");
    let err = stderr_str(&out);
    assert!(err.contains("trashinfo") || err.contains("purge"), "{err}");
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"orphan1").exists());
}

// ---------------------------------------------------------------------------
// variants: several trashed items share one original path.
// ---------------------------------------------------------------------------

#[test]
fn variants_no_tty_lists() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"v1",
        b"/home/u/Downloads/dup",
        "2026-01-01T00:00:00",
        Body::File(b"one".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"v2",
        b"/home/u/Downloads/dup",
        "2026-01-02T00:00:00",
        Body::File(b"two".to_vec()),
    );

    let out = sandbox.rip("/", &["restore", "/home/u/Downloads/dup"]);
    assert_fails(&out, "ambiguous original path, no tty");
    let err = stderr_str(&out);
    assert!(
        err.contains("files/v1") && err.contains("files/v2"),
        "{err}"
    );
}

#[test]
fn variants_picker() {
    let mut sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"v1",
        b"/home/u/Downloads/dup",
        "2026-01-01T00:00:00",
        Body::File(b"one".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"v2",
        b"/home/u/Downloads/dup",
        "2026-01-02T00:00:00",
        Body::File(b"two".to_vec()),
    );
    sandbox.fzf_pick("2026-01-02");

    let out = sandbox.rip_tty("/", &["restore", "/home/u/Downloads/dup"], "");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("dup")).unwrap(),
        b"two"
    );
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"v1").exists());
    assert!(!trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"v2").exists());
    let args = sandbox.fake_fzf_args();
    assert!(args.iter().any(|a| a == "--prompt=restore> "), "{args:?}");
}

// ---------------------------------------------------------------------------
// The no-PATH picker: cwd scope, --all, and cancelling.
// ---------------------------------------------------------------------------

#[test]
fn picker_scope_cwd_and_all() {
    let mut sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dl",
        b"/home/u/Downloads/dl",
        "2026-01-01T00:00:00",
        Body::File(b"dl".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"doc",
        b"/home/u/Documents/doc",
        "2026-01-02T00:00:00",
        Body::File(b"doc".to_vec()),
    );
    // Matches every fzf record: the fake fzf treats an EMPTY
    // FAKE_FZF_PICK as "select nothing" (`[ -n "$pick" ] && ...`), so this
    // uses a substring both items' dates share instead.
    sandbox.fzf_pick("2026-0");

    let out = sandbox.rip_tty("/home/u/Downloads", &["restore"], "");
    assert!(out.status.success(), "{out:?}");
    assert!(sandbox.host("/home/u/Downloads").join("dl").exists());
    assert!(
        !sandbox.host("/home/u/Documents").join("doc").exists(),
        "doc must not have been in the cwd-scoped pool at all"
    );
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"doc").exists());

    let out2 = sandbox.rip_tty("/home/u/Downloads", &["restore", "--all"], "");
    assert!(out2.status.success(), "{out2:?}");
    assert!(sandbox.host("/home/u/Documents").join("doc").exists());
}

#[test]
fn picker_cancel_exit0() {
    let mut sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );
    sandbox.fzf_exit(1); // no match / cancelled

    let out = sandbox.rip_tty("/home/u/Downloads", &["restore"], "");
    assert!(
        out.status.success(),
        "cancelled picker must exit 0: {out:?}"
    );
    assert!(!sandbox.host("/home/u/Downloads").join("x").exists());
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"x").exists());
}

#[test]
fn fzf_missing() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );

    let out = rip_without_fzf_tty(&sandbox, "/home/u/Downloads", &["restore"]);
    assert!(!out.status.success(), "{out:?}");
    let text = stdout_str(&out);
    assert!(
        text.to_lowercase().contains("fzf"),
        "expected a clear fzf-missing message: {text}"
    );
}

// ---------------------------------------------------------------------------
// topdir_relative_restore, symlink_item_restored_as_link, non_utf8_round_trip
// ---------------------------------------------------------------------------

#[test]
fn topdir_relative_restore() {
    let sandbox = Sandbox::artemis();
    let uid = uid_string();
    let trash_inside = format!("/mnt/other/.Trash-{uid}");
    sandbox.plant(
        &trash_inside,
        b"x",
        b"sub/x",
        "2026-01-01T00:00:00",
        Body::File(b"topdir".to_vec()),
    );
    let trash_x = trash_host_path(&sandbox, &trash_inside, b"x");
    let original_ino = ino(&trash_x);

    let out = sandbox.rip("/", &["restore", "/mnt/other/sub/x"]);
    assert_ok(&out, "topdir relative restore");
    let dest = sandbox.host("/mnt/other").join("sub/x");
    assert_eq!(std::fs::read(&dest).unwrap(), b"topdir");
    assert_eq!(
        ino(&dest),
        original_ino,
        "must be a rename within the same topdir"
    );
    assert!(!trash_x.exists());
}

#[test]
fn symlink_item_restored_as_link() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"link",
        b"/home/u/Downloads/link",
        "2026-01-01T00:00:00",
        Body::Symlink(PathBuf::from("/some/target")),
    );

    let out = sandbox.rip("/", &["restore", "/home/u/Downloads/link"]);
    assert_ok(&out, "restore a symlink item");
    let dest = sandbox.host("/home/u/Downloads").join("link");
    let meta = std::fs::symlink_metadata(&dest).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "must stay a symlink, not be followed"
    );
    assert_eq!(
        std::fs::read_link(&dest).unwrap(),
        PathBuf::from("/some/target")
    );
}

#[test]
fn non_utf8_round_trip() {
    let sandbox = Sandbox::artemis();
    let name: &[u8] = b"caf\xE9";
    let mut original = b"/home/u/Downloads/".to_vec();
    original.extend_from_slice(name);
    sandbox.plant(
        "/home/u/.local/share/Trash",
        name,
        &original,
        "2026-01-01T00:00:00",
        Body::File(b"non-utf8".to_vec()),
    );
    let trash_entry = trash_host_path(&sandbox, "/home/u/.local/share/Trash", name);
    assert!(trash_entry.exists());

    let mut arg = OsString::from("/home/u/.local/share/Trash/files/");
    arg.push(OsStr::from_bytes(name));
    let args: Vec<OsString> = vec![OsString::from("restore"), arg];
    let out = sandbox.rip("/", &args);
    assert_ok(&out, "restore a non-utf8 trash path");

    let dest = sandbox
        .host("/home/u/Downloads")
        .join(OsStr::from_bytes(name));
    assert_eq!(std::fs::read(&dest).unwrap(), b"non-utf8");
    assert!(!trash_entry.exists());
}

// ---------------------------------------------------------------------------
// hostile_paths (design §13.4 "A hostile Path= writes outside its topdir")
// ---------------------------------------------------------------------------

#[test]
fn hostile_paths() {
    let sandbox = Sandbox::artemis();
    let uid = uid_string();
    let trash_inside = format!("/mnt/other/.Trash-{uid}");
    // "/home/u" itself (unlike its Downloads/Documents/Trash sub-binds) has
    // no host-backed directory until something creates it; precreate it so
    // `listing()` below can read it before any `rip` invocation does that
    // as a side effect of restoring into it.
    std::fs::create_dir_all(sandbox.host("/home/u")).unwrap();

    // Three malformed Path= values: each becomes an orphan, never a
    // restorable item, whatever restore is asked to do with it.
    sandbox.plant(
        &trash_inside,
        b"dotdot",
        b"../../x",
        "2026-01-01T00:00:00",
        Body::File(b"a".to_vec()),
    );
    sandbox.plant(
        &trash_inside,
        b"outside",
        b"/etc/x",
        "2026-01-01T00:00:00",
        Body::File(b"b".to_vec()),
    );
    sandbox.plant(
        &trash_inside,
        b"nul",
        &[0u8],
        "2026-01-01T00:00:00",
        Body::File(b"c".to_vec()),
    );

    // A planted symlink escaping the topdir, named by a syntactically
    // valid (no "..") relative Path=.
    let other_host = sandbox.host("/mnt/other");
    std::os::unix::fs::symlink("/home/u", other_host.join("link")).unwrap();
    sandbox.plant(
        &trash_inside,
        b"escape",
        b"link/x",
        "2026-01-01T00:00:00",
        Body::File(b"d".to_vec()),
    );

    // `/home/u/Downloads`, `.../Documents` and `.../.local` are themselves
    // bind-mount points onto the persist subvolume; bwrap must create each
    // one as an (always-empty, from the host's side) directory under
    // "/home/u" the first time a bind covers it, purely as a side effect
    // of mounting -- not evidence of anything rip did. Only an entry
    // outside that known set is evidence of an actual escape.
    let listing = |sandbox: &Sandbox| -> Vec<OsString> {
        let mut v: Vec<OsString> = std::fs::read_dir(sandbox.host("/home/u"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .filter(|n| !matches!(n.to_str(), Some(".local" | "Documents" | "Downloads")))
            .collect();
        v.sort();
        v
    };
    let before = listing(&sandbox);

    // All four together: the first orphan aborts selection before any
    // change (design §7.1 "resolve every PATH before any change"), so even
    // the syntactically valid but hostile "escape" item is never touched.
    let out = sandbox.rip(
        "/",
        &[
            "restore".to_string(),
            format!("/mnt/other/.Trash-{uid}/files/dotdot"),
            format!("/mnt/other/.Trash-{uid}/files/outside"),
            format!("/mnt/other/.Trash-{uid}/files/nul"),
            format!("/mnt/other/.Trash-{uid}/files/escape"),
        ],
    );
    assert_fails(&out, "hostile/malformed paths together");
    assert_eq!(
        before,
        listing(&sandbox),
        "nothing outside /mnt/other may be created"
    );

    // The symlink-escape item alone, selected unambiguously by its trash
    // path so this actually reaches ensure_parent's own RESOLVE_BENEATH
    // check (an original-path argument would instead be normalized away by
    // resolve()'s own parent canonicalization before selection).
    let out = sandbox.rip(
        "/",
        &["restore", &format!("/mnt/other/.Trash-{uid}/files/escape")],
    );
    assert_fails(&out, "restore through a hostile symlink");
    assert_eq!(
        before,
        listing(&sandbox),
        "nothing outside /mnt/other may be created"
    );
    assert!(
        trash_host_path(&sandbox, &trash_inside, b"escape").exists(),
        "the item must remain in the trash"
    );
}

// ---------------------------------------------------------------------------
// undo (design §7.4)
// ---------------------------------------------------------------------------

#[test]
fn undo_newest_batch() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"old",
        b"/home/u/Downloads/old",
        "2026-01-01T00:00:00",
        Body::File(b"old".to_vec()),
    );
    let uid = uid_string();
    sandbox.plant(
        &format!("/mnt/other/.Trash-{uid}"),
        b"new",
        b"new",
        "2026-02-02T00:00:00",
        Body::File(b"new".to_vec()),
    );

    let out = sandbox.rip("/", &["undo"]);
    assert_ok(&out, "undo newest batch across trash dirs");
    assert!(sandbox.host("/mnt/other").join("new").exists());
    assert!(!sandbox.host("/home/u/Downloads").join("old").exists());
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"old").exists());
}

#[test]
fn undo_parents_first() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dir",
        b"/home/u/Downloads/dir",
        "2026-01-01T00:00:00",
        Body::Dir,
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"dir_f",
        b"/home/u/Downloads/dir/f",
        "2026-01-01T00:00:00",
        Body::File(b"inside".to_vec()),
    );

    let out = sandbox.rip("/", &["undo"]);
    assert_ok(
        &out,
        "undo restores the parent before what belongs inside it",
    );
    assert!(sandbox.host("/home/u/Downloads/dir").is_dir());
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads/dir/f")).unwrap(),
        b"inside"
    );
    assert!(!trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"dir").exists());
    assert!(!trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"dir_f").exists());
}

#[test]
fn undo_partial_conflict() {
    let sandbox = Sandbox::artemis();
    std::fs::write(
        sandbox.host("/home/u/Downloads").join("blocked"),
        b"existing",
    )
    .unwrap();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"blocked_item",
        b"/home/u/Downloads/blocked",
        "2026-01-01T00:00:00",
        Body::File(b"trashed".to_vec()),
    );
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"ok_item",
        b"/home/u/Downloads/ok",
        "2026-01-01T00:00:00",
        Body::File(b"trashed".to_vec()),
    );

    let out = sandbox.rip("/", &["undo"]);
    assert_fails(&out, "undo with one conflicting item");
    let err = stderr_str(&out);
    assert!(err.contains("--rename"), "{err}");
    assert!(err.contains("blocked_item"), "{err}");
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("blocked")).unwrap(),
        b"existing"
    );
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"blocked_item").exists());
    // The other item, not in conflict, must still have been restored.
    assert_eq!(
        std::fs::read(sandbox.host("/home/u/Downloads").join("ok")).unwrap(),
        b"trashed"
    );
    assert!(!trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"ok_item").exists());
}

#[test]
fn undo_empty() {
    let sandbox = Sandbox::artemis();
    let out = sandbox.rip("/", &["undo"]);
    assert_fails(&out, "undo on an empty trash");
    assert!(
        stderr_str(&out).contains("nothing to undo"),
        "{}",
        stderr_str(&out)
    );
}

// ---------------------------------------------------------------------------
// purge
// ---------------------------------------------------------------------------

#[test]
fn purge_by_path_y() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"gone",
        b"/home/u/Downloads/gone",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );
    let trash_gone = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"gone");

    let out = sandbox.rip(
        "/",
        &["purge", "-y", "/home/u/.local/share/Trash/files/gone"],
    );
    assert_ok(&out, "purge -y");
    assert!(!trash_gone.exists());
    assert!(
        !sandbox
            .host("/home/u/.local/share/Trash")
            .join("info/gone.trashinfo")
            .exists()
    );
}

#[test]
fn purge_no_tty() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"stay",
        b"/home/u/Downloads/stay",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );

    let out = sandbox.rip("/", &["purge", "/home/u/.local/share/Trash/files/stay"]);
    assert_fails(&out, "purge without a terminal");
    assert!(trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"stay").exists());
}

#[test]
fn purge_picker() {
    let mut sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"pick_me",
        b"/home/u/Downloads/pick_me",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );
    sandbox.fzf_pick("pick_me");

    let out = sandbox.rip_tty("/home/u/Downloads", &["purge"], "y\n");
    assert!(out.status.success(), "{out:?}");
    assert!(!trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"pick_me").exists());
}

#[test]
fn purge_orphan_by_trash_path() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"orph",
        b"",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );
    let trash_orph = trash_host_path(&sandbox, "/home/u/.local/share/Trash", b"orph");
    assert!(trash_orph.exists());

    let out = sandbox.rip(
        "/",
        &["purge", "-y", "/home/u/.local/share/Trash/files/orph"],
    );
    assert_ok(&out, "purge orphan by trash path");
    assert!(!trash_orph.exists());
}

// ---------------------------------------------------------------------------
// fzf_args_real (design §15.2 U5): real fzf from the dev shell on the host,
// no sandbox.
// ---------------------------------------------------------------------------

#[test]
fn fzf_args_real() {
    // Mirrors restore.rs's own (private) fzf_args("restore") literally --
    // there is no lib crate here to call it from directly.
    let args: [&str; 7] = [
        "--multi",
        "--read0",
        "--print0",
        "--delimiter=\t",
        "--with-nth=2..",
        "--tiebreak=index",
        "--prompt=restore> ",
    ];
    let input = b"0\t2026-09-21 10:00:00\tdocs/a b.txt\x001\t2026-09-21 11:00:00\tother.txt\x00";
    let mut child = Command::new("fzf")
        .args(args)
        .arg("--filter=a b")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real fzf (needs the dev shell's fzf on PATH)");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .expect("write to fzf's stdin");
    let out = child.wait_with_output().expect("wait for fzf");
    assert!(
        out.status.success(),
        "fzf --filter failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout, b"0\t2026-09-21 10:00:00\tdocs/a b.txt\0",
        "fzf must return the full record, index included"
    );
}
