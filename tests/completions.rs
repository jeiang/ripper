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

// ---- c29: fish 4's qmark-noglob made `-?*` a dead pattern, so ANY option
// word (not just `--config=PATH`) fell to `case '*'`, and every one of them
// -- including `--config=PATH` -- was treated as a non-option, ending the
// scan and reporting "files". ----

#[test]
fn config_equals_form_still_gives_subcommands_at_the_first_word() {
    let sandbox = Sandbox::artemis();
    std::fs::write(sandbox.host("/home/u/Downloads").join("c.toml"), b"").unwrap();

    let got = complete(&sandbox, "/home/u/Downloads", "rip --config=c.toml ");
    for sub in SUBCOMMANDS {
        assert!(
            got.iter().any(|w| w == sub),
            "missing subcommand {sub} after --config=PATH: {got:?}"
        );
    }
}

#[test]
fn config_equals_form_then_restore_gives_trashed_paths_not_cwd_files() {
    let sandbox = Sandbox::artemis();
    std::fs::write(sandbox.host("/home/u/Downloads").join("c.toml"), b"").unwrap();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let got = complete(
        &sandbox,
        "/home/u/Downloads",
        "rip --config=c.toml restore ",
    );
    assert!(
        got.iter().any(|w| w == "x"),
        "expected the planted trashed path 'x' after --config=PATH restore: {got:?}"
    );
    assert!(
        !got.iter().any(|w| w == "c.toml"),
        "must not fall back to cwd files after --config=PATH restore: {got:?}"
    );
}

#[test]
fn rm_style_flag_before_a_subcommand_gives_files_only() {
    let sandbox = Sandbox::artemis();
    std::fs::write(sandbox.host("/home/u/Downloads").join("fixture.txt"), b"x").unwrap();

    let got = complete(&sandbox, "/home/u/Downloads", "rip -f ");
    for sub in SUBCOMMANDS {
        assert!(
            !got.iter().any(|w| w == sub),
            "unexpected subcommand {sub} after an rm-style flag: {got:?}"
        );
    }
    assert!(
        got.iter().any(|w| w == "fixture.txt"),
        "files must still complete after an rm-style flag: {got:?}"
    );
}

#[test]
fn help_flag_does_not_lock_out_subcommands() {
    // -h is neither an rm-style flag nor a --config/--completions value
    // (src/main.rs's first_word() leaves the scan state unchanged for it),
    // so it must not be treated like `-f` and lock the state to "files".
    let sandbox = Sandbox::artemis();
    let got = complete(&sandbox, "/home/u/Downloads", "rip -h ");
    assert!(
        got.iter().any(|w| w == "list"),
        "-h must not change the completion state: {got:?}"
    );
}

// ---- c28: piping `string split0`'s output further (into `string replace`,
// then again through `complete`'s own `(__rip_trashed)` command
// substitution) re-splits on any newline embedded in a trashed path, so a
// hostile or merely oddly named file forged extra candidates out of
// whatever text followed the newline. ----

#[test]
fn restore_skips_a_trashed_path_with_an_embedded_newline() {
    let sandbox = Sandbox::artemis();
    // A trashed item whose original path holds a literal newline (e.g. from
    // an extracted archive) -- no planted/hostile topdir needed.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"evil",
        b"/home/u/Downloads/a\n/etc/attacker-controlled",
        "2026-01-01T00:00:00",
        Body::File(b"x".to_vec()),
    );
    // An unaffected item must still complete normally.
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"good",
        b"/home/u/Downloads/good",
        "2026-01-02T00:00:00",
        Body::File(b"y".to_vec()),
    );

    let got = complete(&sandbox, "/home/u/Downloads", "rip restore ");
    assert!(
        !got.iter().any(|w| w == "/etc/attacker-controlled"),
        "a newline in a trashed path must not forge a completion candidate: {got:?}"
    );
    assert!(
        !got.iter().any(|w| w == "a"),
        "the newline-bearing record must be skipped whole, not split: {got:?}"
    );
    assert!(
        got.iter().any(|w| w == "good"),
        "an item unaffected by the newline must still complete: {got:?}"
    );
}
