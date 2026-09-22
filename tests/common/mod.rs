//! The bwrap sandbox harness (docs/design.md §3.2). Every integration test
//! that runs `rip` runs it inside `Sandbox`, which gives it a fresh user, PID
//! and mount namespace and only the listed binds: it cannot see or change the
//! real home trash, `/mnt/Mumei` or the stray `.Trash-*` dirs on the host that
//! runs the tests.
//!
//! This file has no test binary of its own (`tests/common/mod.rs` is the
//! standard way to share code between the `tests/*.rs` binaries, each of
//! which pulls it in with `mod common;`). Only `tests/harness.rs` exists so
//! far, so most of this API is unused from its point of view; the rest is
//! for `tests/list.rs`, `tests/put.rs`, `tests/restore.rs`, `tests/empty.rs`
//! and `tests/completions.rs`, added by later checkpoints.
#![allow(dead_code)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};

// Matches info.rs's PATH_SET exactly: plant() writes
// .trashinfo bytes itself, because integration tests cannot call the
// binary's modules, but the encoding must still match what rip will read.
const PATH_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// Where the fake `fzf` records its argv, a path
/// inside the sandbox that resolves onto a host-backed bind (`/home`), so
/// the harness can still read it back after the sandboxed process exits.
const FAKE_FZF_ARGS_INSIDE: &str = "/home/u/.fake-fzf-args";

const BTRFS_SUPER_MAGIC: u32 = 0x9123_683E;

/// A fresh bwrap sandbox laid out like artemis (docs/design.md §3.2).
pub struct Sandbox {
    base: PathBuf,
    shm: PathBuf,
    /// (host path, path inside the sandbox), applied to `bwrap` as `--bind`
    /// in this order: a later bind at the same inside path covers an
    /// earlier one, exactly as a real mount would.
    binds: Vec<(PathBuf, String)>,
    fakebin: PathBuf,
    fzf_pick: Option<String>,
    fzf_exit: Option<i32>,
}

/// What `plant` puts at `files/NAME` (docs/design.md §4: Item/Orphan/
/// Dangling). `Missing` plants only the `.trashinfo`, giving a Dangling
/// entry.
pub enum Body {
    File(Vec<u8>),
    Dir,
    Symlink(PathBuf),
    Missing,
}

impl Sandbox {
    /// The artemis-shaped layout (docs/design.md §3.2): a real btrfs
    /// subvolume for `/persist`, the ephemeral root, binds of `/persist` for
    /// Downloads/Documents/the home trash, a same-home-fs/other-subvolume
    /// bind root, a trash-subvolume bind root, and two tmpfs topdirs (one
    /// unwritable). Never skips: a missing `bwrap`, a failed namespace
    /// preflight or no btrfs directory panics with a fix hint.
    pub fn artemis() -> Self {
        preflight();
        let btrfs_dir = btrfs_dir();
        let base = mkdtemp(&btrfs_dir, "rip-base-");
        let shm = mkdtemp(Path::new("/dev/shm"), "rip-shm-");

        let p = base.join("P");
        btrfs_subvolume_create(&p);
        let side = base.join("side");
        btrfs_subvolume_create(&side);

        let root = base.join("root");
        mkdir(&root);

        let p_downloads = p.join("u/Downloads");
        mkdir_all(&p_downloads);
        let p_documents = p.join("u/Documents");
        mkdir_all(&p_documents);
        let p_trash = p.join("u/.local/share/Trash");
        mkdir_all(&p_trash);
        let p_pside = p.join("pside");
        mkdir(&p_pside);

        let shm_other = shm.join("other");
        mkdir(&shm_other);
        let shm_ro = shm.join("ro");
        mkdir(&shm_ro);
        let shm_ro_work = shm_ro.join("work");
        mkdir(&shm_ro_work);
        chmod(&shm_ro_work, 0o755);
        chmod(&shm_ro, 0o555);

        let fakebin = base.join("fakebin");
        write_fake_fzf(&fakebin);

        let binds = vec![
            (p, "/persist".to_string()),
            (root, "/home".to_string()),
            (p_downloads, "/home/u/Downloads".to_string()),
            (p_documents, "/home/u/Documents".to_string()),
            (p_trash, "/home/u/.local/share/Trash".to_string()),
            (side, "/mnt/side".to_string()),
            (p_pside, "/mnt/pside".to_string()),
            (shm_other, "/mnt/other".to_string()),
            (shm_ro, "/mnt/ro".to_string()),
        ];

        Sandbox {
            base,
            shm,
            binds,
            fakebin,
            fzf_pick: None,
            fzf_exit: None,
        }
    }

    /// Appends a bind, after the layout's own binds: a bind at an inside
    /// path the layout already uses covers it, the way a real mount would.
    pub fn bind(&mut self, host: impl Into<PathBuf>, inside: &str) -> &mut Self {
        self.binds.push((host.into(), inside.to_string()));
        self
    }

    /// Removes the layout's bind at `inside` (e.g. `without("/persist")`),
    /// so nothing is mounted there at all.
    pub fn without(&mut self, inside: &str) -> &mut Self {
        self.binds.retain(|(_, i)| i != inside);
        self
    }

    /// The host path backing `inside`, by the longest matching bind prefix
    /// (ties go to the most recently added bind, which is what is actually
    /// visible inside the sandbox). Panics if nothing covers `inside`.
    pub fn host(&self, inside: &str) -> PathBuf {
        let mut best: Option<(&str, &Path)> = None;
        for (host, i) in &self.binds {
            let covers = inside == i.as_str() || inside.starts_with(&format!("{i}/"));
            if !covers {
                continue;
            }
            if best.is_none_or(|(bi, _)| i.len() >= bi.len()) {
                best = Some((i.as_str(), host.as_path()));
            }
        }
        let (bind_inside, bind_host) =
            best.unwrap_or_else(|| panic!("Sandbox::host: no bind covers {inside:?}"));
        let rel = inside[bind_inside.len()..].trim_start_matches('/');
        if rel.is_empty() {
            bind_host.to_path_buf()
        } else {
            bind_host.join(rel)
        }
    }

    /// Runs `rip` inside the sandbox with stdin `/dev/null` (never a
    /// terminal), and returns its output. `args` are the words after `rip`.
    pub fn rip(&self, cwd: &str, args: &[impl AsRef<OsStr>]) -> Output {
        let mut argv: Vec<&OsStr> = Vec::with_capacity(args.len() + 1);
        argv.push(OsStr::new("/run/rip/bin/rip"));
        argv.extend(args.iter().map(|a| a.as_ref()));
        self.command(cwd, &argv)
            .stdin(Stdio::null())
            .output()
            .expect("spawn bwrap for `rip`")
    }

    /// Runs `rip` inside the sandbox on a real pty (via `script -qec`, with
    /// `SHELL` forced to the host's own bash), feeding `input` on stdin.
    /// Because a pty has a single combined stream, stdout and stderr are not
    /// separable: `Output::stdout` carries everything the terminal saw, and
    /// `Output::stderr` is always empty.
    pub fn rip_tty(&self, cwd: &str, args: &[&str], input: &str) -> Output {
        let mut argv: Vec<&OsStr> = Vec::with_capacity(args.len() + 1);
        argv.push(OsStr::new("/run/rip/bin/rip"));
        argv.extend(args.iter().map(OsStr::new));
        let cmd = self.command(cwd, &argv);

        let mut quoted = shell_quote(cmd.get_program());
        for arg in cmd.get_args() {
            quoted.push(b' ');
            quoted.extend(shell_quote(arg));
        }
        let script_cmd = OsString::from_vec(quoted);

        let mut child = Command::new("script")
            .arg("-qec")
            .arg(&script_cmd)
            .arg("/dev/null")
            .env("SHELL", host_bash())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `script` for rip_tty");
        child
            .stdin
            .take()
            .expect("script's stdin")
            .write_all(input.as_bytes())
            .expect("write rip_tty input");
        child.wait_with_output().expect("wait for rip_tty")
    }

    /// Runs an arbitrary program inside the sandbox (`fish`, `date`, `mv`,
    /// ...), found via the sandbox's `PATH`. Used to check the sandbox's own
    /// mount topology and to drive tools other than `rip`.
    pub fn exec(&self, cwd: &str, argv: &[&str]) -> Output {
        let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
        self.command(cwd, &argv)
            .stdin(Stdio::null())
            .output()
            .expect("spawn bwrap for exec")
    }

    /// Writes `files/NAME` (per `body`) and `info/NAME.trashinfo` directly
    /// to the host directory backing `trash` (an inside path, e.g.
    /// `/home/u/.local/share/Trash` or `/mnt/other/.Trash-1000`), creating
    /// `files/` and `info/` if needed. `path_field` is percent-encoded the
    /// way `info::encode` does; `date` is written verbatim after
    /// `DeletionDate=`, so callers can plant malformed or undashed dates.
    /// Integration tests cannot call the binary's own `info::encode`.
    pub fn plant(&self, trash: &str, name: &[u8], path_field: &[u8], date: &str, body: Body) {
        let root = self.host(trash);
        let files_dir = root.join("files");
        let info_dir = root.join("info");
        fs::create_dir_all(&files_dir)
            .unwrap_or_else(|e| panic!("plant: mkdir {}: {e}", files_dir.display()));
        fs::create_dir_all(&info_dir)
            .unwrap_or_else(|e| panic!("plant: mkdir {}: {e}", info_dir.display()));

        let name_os = OsStr::from_bytes(name);
        let entry = files_dir.join(name_os);
        match body {
            Body::File(content) => fs::write(&entry, content)
                .unwrap_or_else(|e| panic!("plant: write {}: {e}", entry.display())),
            Body::Dir => fs::create_dir(&entry)
                .unwrap_or_else(|e| panic!("plant: mkdir {}: {e}", entry.display())),
            Body::Symlink(target) => std::os::unix::fs::symlink(&target, &entry)
                .unwrap_or_else(|e| panic!("plant: symlink {}: {e}", entry.display())),
            Body::Missing => {}
        }

        let mut text = Vec::new();
        text.extend_from_slice(b"[Trash Info]\nPath=");
        text.extend(percent_encoding::percent_encode(path_field, PATH_SET).flat_map(|s| s.bytes()));
        text.extend_from_slice(b"\nDeletionDate=");
        text.extend_from_slice(date.as_bytes());
        text.push(b'\n');

        let mut info_name = name.to_vec();
        info_name.extend_from_slice(b".trashinfo");
        let info_path = info_dir.join(OsStr::from_bytes(&info_name));
        fs::write(&info_path, text)
            .unwrap_or_else(|e| panic!("plant: write {}: {e}", info_path.display()));
    }

    /// Writes `$XDG_CONFIG_HOME/ripper/config.toml` inside the sandbox.
    pub fn config(&self, toml: &str) {
        let dir = self.host("/home/u/.config").join("ripper");
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("config: mkdir {}: {e}", dir.display()));
        let path = dir.join("config.toml");
        fs::write(&path, toml).unwrap_or_else(|e| panic!("config: write {}: {e}", path.display()));
    }

    /// Makes the fake `fzf` select every NUL-
    /// delimited stdin record containing `substring`, printed back NUL-
    /// terminated. Cleared by default (a picker that sees no selection
    /// behaves like a cancel). For `tests/restore.rs`'s picker tests.
    pub fn fzf_pick(&mut self, substring: &str) -> &mut Self {
        self.fzf_pick = Some(substring.to_string());
        self
    }

    /// Makes the fake `fzf` exit with `code` instead of 0 (e.g. to simulate
    /// a cancelled or failed picker). For `tests/restore.rs`.
    pub fn fzf_exit(&mut self, code: i32) -> &mut Self {
        self.fzf_exit = Some(code);
        self
    }

    /// The argv the fake `fzf` was last invoked with, one entry per line, or
    /// empty if it was never run. For `tests/restore.rs`.
    pub fn fake_fzf_args(&self) -> Vec<String> {
        let path = self.host(FAKE_FZF_ARGS_INSIDE);
        match fs::read_to_string(&path) {
            Ok(s) => s.lines().map(str::to_owned).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("read {}: {e}", path.display()),
        }
    }

    fn command(&self, cwd: &str, argv: &[&OsStr]) -> Command {
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
        c.arg("--ro-bind")
            .arg(&self.fakebin)
            .arg("/run/rip/fakebin");
        c.arg("--ro-bind")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/completions/rip.fish"))
            .arg("/run/rip/rip.fish");
        for (host, inside) in &self.binds {
            c.arg("--bind").arg(host).arg(inside);
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
        c.arg("--setenv").arg("PATH").arg(self.path());
        c.arg("--setenv")
            .arg("FAKE_FZF_ARGS")
            .arg(FAKE_FZF_ARGS_INSIDE);
        if let Some(pick) = &self.fzf_pick {
            c.arg("--setenv").arg("FAKE_FZF_PICK").arg(pick);
        }
        if let Some(code) = self.fzf_exit {
            c.arg("--setenv").arg("FAKE_FZF_EXIT").arg(code.to_string());
        }
        c.args(["--chdir", cwd, "--"]);
        c.args(argv);
        c
    }

    /// `/run/rip/fakebin:/run/rip/bin`, plus the `/nix/store` entries of the
    /// host's own `PATH` (so `rip` can find `cp`, and `fzf`'s fake can find
    /// nothing it should not: only Nix-provided tools are forwarded).
    fn path(&self) -> OsString {
        let mut entries = vec![
            PathBuf::from("/run/rip/fakebin"),
            PathBuf::from("/run/rip/bin"),
        ];
        if let Some(host_path) = env::var_os("PATH") {
            entries.extend(env::split_paths(&host_path).filter(|p| p.starts_with("/nix/store")));
        }
        env::join_paths(entries).expect("PATH entries must not contain ':' or NUL")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // btrfs subvolumes (`P`, `side`) rmdir like any other empty
        // directory once their contents are gone, so
        // a plain recursive removal handles them; it only needs every mode
        // to allow write+search first, since a test may have left behind a
        // read-only directory (e.g. an unwritable topdir fixture).
        for dir in [&self.base, &self.shm] {
            if let Err(e) = chmod_recursive(dir) {
                eprintln!("sandbox cleanup: chmod -R {}: {e}", dir.display());
            }
            if let Err(e) = fs::remove_dir_all(dir) {
                eprintln!("sandbox cleanup: rm -r {}: {e}", dir.display());
            }
        }
    }
}

/// Runs once per test binary (`OnceLock`, so every `Sandbox::artemis()`
/// after the first reuses the cached result instead of spawning `bwrap`
/// again). Panics with a fix hint on failure; the harness never silently
/// skips a test.
fn preflight() {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = RESULT.get_or_init(|| {
        match Command::new("bwrap")
            // `--tmpfs /x -- true` makes
            // bwrap fail at `execvp` (nothing is bound, so `true` can't be
            // found), which looks identical to a namespace failure from the
            // exit code alone [verified on artemis]. Binding the real root
            // read-only instead gives `true` something to resolve through
            // the inherited PATH, so a non-zero exit here means namespace
            // creation itself failed.
            .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "bwrap cannot create a user namespace (`bwrap --unshare-user --ro-bind / / -- \
                 true` failed, {}):\n{}\n\
                 fix: allow unprivileged user namespaces, e.g. on a systemd/AppArmor host:\n  \
                 sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\n\
                 (see tests/common/mod.rs's `preflight`)",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(
                "`bwrap` was not found on PATH. Install bubblewrap (the flake's devShell \
                 provides it on Linux: run tests through `nix develop --command just test`, \
                 or `just artemis test` from zakkart)."
                    .to_string(),
            ),
            Err(e) => Err(format!("failed to run the `bwrap` preflight: {e}")),
        }
    });
    if let Err(msg) = result {
        panic!("{msg}");
    }
}

/// `$RIP_TEST_BTRFS`, or the system temp dir when it is btrfs (artemis's
/// `/tmp`). Panics with a fix hint if neither is available; never silently
/// falls back to a non-btrfs directory (the whole point of the sandbox is
/// exercising real subvolume behavior).
fn btrfs_dir() -> PathBuf {
    if let Some(dir) = env::var_os("RIP_TEST_BTRFS") {
        let dir = PathBuf::from(dir);
        if !is_btrfs(&dir) {
            panic!(
                "RIP_TEST_BTRFS={} is set but is not a btrfs directory",
                dir.display()
            );
        }
        return dir;
    }
    let tmp = env::temp_dir();
    if is_btrfs(&tmp) {
        return tmp;
    }
    panic!(
        "sandbox tests need a btrfs directory: set RIP_TEST_BTRFS=/path/on/btrfs (CI loop-mounts \
         one), or run where the system temp dir ({}) is btrfs, as it is on artemis's /tmp. \
         See `Sandbox::artemis` above.",
        tmp.display()
    );
}

fn is_btrfs(path: &Path) -> bool {
    match rustix::fs::statfs(path) {
        Ok(s) => (s.f_type as u32) == BTRFS_SUPER_MAGIC,
        Err(_) => false,
    }
}

fn mkdtemp(parent: &Path, prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    for _ in 0..1000 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{prefix}{pid}-{n}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return candidate,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("mkdtemp in {}: {e}", parent.display()),
        }
    }
    panic!("mkdtemp in {}: too many name collisions", parent.display());
}

fn mkdir(path: &Path) {
    fs::create_dir(path).unwrap_or_else(|e| panic!("mkdir {}: {e}", path.display()));
}

fn mkdir_all(path: &Path) {
    fs::create_dir_all(path).unwrap_or_else(|e| panic!("mkdir -p {}: {e}", path.display()));
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {mode:o} {}: {e}", path.display()));
}

/// `chmod -R u+rwX`, used only by `Drop` so cleanup
/// can remove a read-only fixture directory. Best-effort: errors are
/// returned, not panicked on, since this runs during cleanup, possibly
/// while another panic is already unwinding.
fn chmod_recursive(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    let mode = perms.mode() | 0o700;
    if mode != perms.mode() {
        perms.set_mode(mode);
        fs::set_permissions(path, perms)?;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            chmod_recursive(&entry?.path())?;
        }
    }
    Ok(())
}

fn btrfs_subvolume_create(path: &Path) {
    let out = Command::new("btrfs")
        .args(["subvolume", "create"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "sandbox tests need `btrfs` on PATH (btrfs-progs; run inside `nix develop`): {e}"
            )
        });
    if !out.status.success() {
        panic!(
            "btrfs subvolume create {}: {}{}",
            path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// The bash the fake `fzf` script's shebang and `rip_tty`'s `SHELL` use. It
/// must resolve under `/nix/store`: only `/nix`, not the rest of the host's
/// `PATH` directories (e.g. `/run/current-system/sw/bin`), is bound into the
/// sandbox.
fn host_bash() -> PathBuf {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("bash");
            if let Ok(real) = candidate.canonicalize() {
                if real.starts_with("/nix/store") && real.is_file() {
                    return real;
                }
            }
        }
    }
    panic!(
        "Sandbox: no `bash` under /nix/store on PATH (needed for the fake fzf script's shebang \
         and rip_tty's SHELL); run inside `nix develop`."
    );
}

/// A bash script that records its argv to `$FAKE_FZF_ARGS`, and for every
/// NUL-delimited record on stdin containing `$FAKE_FZF_PICK`, prints it back
/// NUL-terminated. Uses only bash builtins (`read
/// -d ''`, `case`/`[[ ]]`), so it does not depend on anything else being on
/// the sandbox's `PATH`.
fn write_fake_fzf(dir: &Path) {
    mkdir_all(dir);
    let bash = host_bash();
    let script = format!(
        "#!{bash}\n\
set -u\n\
args_file=\"${{FAKE_FZF_ARGS:-/dev/null}}\"\n\
: > \"$args_file\"\n\
for a in \"$@\"; do\n\
    printf '%s\\n' \"$a\" >> \"$args_file\"\n\
done\n\
pick=\"${{FAKE_FZF_PICK:-}}\"\n\
while IFS= read -r -d '' record; do\n\
    if [ -n \"$pick\" ] && [[ \"$record\" == *\"$pick\"* ]]; then\n\
        printf '%s\\0' \"$record\"\n\
    fi\n\
done\n\
exit \"${{FAKE_FZF_EXIT:-0}}\"\n",
        bash = bash.display(),
    );
    let path = dir.join("fzf");
    fs::write(&path, &script).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    chmod(&path, 0o755);
}

/// Shell-quotes one argument for `script -c`'s command string (`rip_tty`).
/// Operates on raw bytes throughout, so a non-UTF-8 argument round-trips.
fn shell_quote(arg: &OsStr) -> Vec<u8> {
    let bytes = arg.as_bytes();
    let plain = !bytes.is_empty()
        && bytes.iter().all(|&b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':' | b'=')
        });
    if plain {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'\'');
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}
