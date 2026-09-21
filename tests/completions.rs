//! The hand-written fish completions (docs/design.md §11, design.md §13.3
//! "tests/completions.rs"), exercised for real with fish's `complete -C`
//! inside the sandbox: `rip.fish` mixes subcommands with rip's own
//! `rip list -0` output, so only a real fish binary can prove it works.

mod common;

use common::{Body, Sandbox};

/// Runs `fish --no-config -c "source /run/rip/rip.fish; complete -C 'CMDLINE'"`
/// inside the sandbox and returns each candidate's completion word (the part
/// before fish's `\t` description, when it has one).
fn complete(sandbox: &Sandbox, cwd: &str, cmdline: &str) -> Vec<String> {
    let script = format!(
        "source /run/rip/rip.fish; complete -C {}",
        single_quote(cmdline)
    );
    let out = sandbox.exec(cwd, &["fish", "--no-config", "-c", script.as_str()]);
    assert!(
        out.status.success(),
        "fish failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.split('\t').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

const SUBCOMMANDS: [&str; 5] = ["undo", "list", "restore", "empty", "purge"];

#[test]
fn rip_gives_subcommands_and_files() {
    let sandbox = Sandbox::artemis();
    let downloads_host = sandbox.host("/home/u/Downloads");
    std::fs::write(downloads_host.join("fixture.txt"), b"x").unwrap();

    let got = complete(&sandbox, "/home/u/Downloads", "rip ");
    for sub in SUBCOMMANDS {
        assert!(
            got.iter().any(|w| w == sub),
            "missing subcommand {sub}: {got:?}"
        );
    }
    assert!(
        got.iter().any(|w| w == "fixture.txt"),
        "missing the fixture file: {got:?}"
    );
}

#[test]
fn double_dash_gives_no_subcommands() {
    let sandbox = Sandbox::artemis();
    let downloads_host = sandbox.host("/home/u/Downloads");
    std::fs::write(downloads_host.join("fixture.txt"), b"x").unwrap();

    let got = complete(&sandbox, "/home/u/Downloads", "rip -- ");
    for sub in SUBCOMMANDS {
        assert!(
            !got.iter().any(|w| w == sub),
            "unexpected subcommand {sub} after --: {got:?}"
        );
    }
    assert!(
        got.iter().any(|w| w == "fixture.txt"),
        "files must still complete after --: {got:?}"
    );
}

#[test]
fn a_file_argument_gives_no_subcommands() {
    let sandbox = Sandbox::artemis();
    let downloads_host = sandbox.host("/home/u/Downloads");
    std::fs::write(downloads_host.join("somefile"), b"x").unwrap();

    let got = complete(&sandbox, "/home/u/Downloads", "rip somefile ");
    for sub in SUBCOMMANDS {
        assert!(
            !got.iter().any(|w| w == sub),
            "unexpected subcommand {sub} after a file argument: {got:?}"
        );
    }
}

#[test]
fn restore_gives_planted_trashed_paths() {
    let sandbox = Sandbox::artemis();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let got = complete(&sandbox, "/home/u/Downloads", "rip restore ");
    assert!(
        got.iter().any(|w| w == "x"),
        "expected the planted trashed path 'x': {got:?}"
    );
}
