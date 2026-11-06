//! The hand-written bash, zsh and fish completions (docs/design.md §2.1),
//! exercised for real inside the sandbox: bash and fish run `_rip`/rip.fish's
//! own completion machinery directly, and zsh drives the real `_rip` under
//! `compinit` from a real interactive session (`zsh/zpty`). All three mix
//! subcommands with rip's own `rip list -0` output, so only the real shells
//! can prove they work.

mod common;

use common::{Body, Sandbox};

/// Runs `fish --no-config -c "source /run/rip/completions/rip.fish; complete -C 'CMDLINE'"`
/// inside the sandbox and returns each candidate's completion word (the part
/// before fish's `\t` description, when it has one).
fn complete_fish(sandbox: &Sandbox, cwd: &str, cmdline: &str) -> Vec<String> {
    let script = format!(
        "source /run/rip/completions/rip.fish; complete -C {}",
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

/// A sandbox with `completions/` (this crate's, not a copy) bound read-only
/// at `/run/rip/completions`, for the fish, bash and zsh harnesses below.
fn sandbox_with_shell_completions() -> Sandbox {
    let mut sandbox = Sandbox::artemis();
    sandbox.bind_ro(
        concat!(env!("CARGO_MANIFEST_DIR"), "/completions"),
        "/run/rip/completions",
    );
    sandbox
}

/// Splits `cmdline` the way real bash computes `COMP_WORDS` for a
/// registered `complete -F` function: readline splits not just on
/// whitespace but at every run of a non-whitespace `COMP_WORDBREAKS`
/// character (default includes `=` and `:`, verified interactively against
/// a real bash -- see `completions/rip.bash`'s `_rip_reassemble`), so
/// `--config=c.toml` arrives as three words and a colon-bearing path splits
/// mid-word too. A shell word that opens a quote or ends in a backslash
/// escape is exempt (quoting is verified interactively to suppress
/// COMP_WORDBREAKS splitting for its own span); rip's own test commands
/// only ever carry one in their last (cursor) word, so this only needs to
/// recognize "some quote/backslash appears in this whitespace-delimited
/// word", not parse it. A trailing space in `cmdline` yields a trailing
/// empty word, the word under the cursor.
fn comp_words(cmdline: &str) -> Vec<String> {
    const BREAK: &[char] = &['"', '\'', '@', '>', '<', '=', ';', '|', '&', '(', ':'];

    let mut shell_words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmdline.chars();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '\\' => {
                cur.push(c);
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() => shell_words.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    shell_words.push(cur);

    let mut words = Vec::new();
    for w in shell_words {
        if w.contains(['\'', '"', '\\']) {
            words.push(w);
            continue;
        }
        let mut piece = String::new();
        let mut piece_is_break = false;
        for c in w.chars() {
            let is_break = BREAK.contains(&c);
            if !piece.is_empty() && is_break != piece_is_break {
                words.push(std::mem::take(&mut piece));
            }
            piece.push(c);
            piece_is_break = is_break;
        }
        if !piece.is_empty() {
            words.push(piece);
        } else if w.is_empty() {
            words.push(w);
        }
    }
    words
}

/// Drives `_rip` (from `completions/rip.bash`, sourced fresh) the way real
/// bash completion would: `cmdline` is split into `COMP_WORDS` the way
/// readline would split it (`comp_words`), `COMP_CWORD`/`COMP_LINE`/
/// `COMP_POINT` are set to match, and the resulting `COMPREPLY` is
/// returned. `compopt -o filenames` always fails here (bash only allows it
/// from inside real readline completion; see `_rip` in
/// `completions/rip.bash`), so that one message is tolerated on stderr and
/// anything else fails the test outright.
fn complete_bash(sandbox: &Sandbox, cwd: &str, cmdline: &str) -> Vec<String> {
    let words: Vec<String> = comp_words(cmdline)
        .iter()
        .map(|w| single_quote(w))
        .collect();
    let cword = words.len() - 1;
    let script = format!(
        "source /run/rip/completions/rip.bash\n\
         COMP_WORDS=({words})\n\
         COMP_CWORD={cword}\n\
         COMP_LINE={line}\n\
         COMP_POINT={point}\n\
         _rip\n\
         ((${{#COMPREPLY[@]}})) && printf '%s\\n' \"${{COMPREPLY[@]}}\"\n",
        words = words.join(" "),
        line = single_quote(cmdline),
        point = cmdline.len(),
    );
    let out = sandbox.exec(
        cwd,
        &["bash", "--noprofile", "--norc", "-c", script.as_str()],
    );
    assert!(
        out.status.success(),
        "bash exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.contains("compopt"),
            "unexpected bash stderr: {line:?} (full: {stderr:?})"
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Overrides the `compadd` builtin (as a shell function, which zsh prefers
/// over the builtin unless called via `builtin compadd`) to also record
/// every match it is given, then still calls the real builtin so completion
/// behaves exactly as it would for a person. `compadd`'s own option syntax
/// (`man zshcompwid`) is reimplemented far enough to cover what `_files`,
/// `_describe` and this crate's own `_rip_trashed`/`_rip` actually pass:
/// boolean short options (which may be bundled, e.g. `-Qf`), the arg-taking
/// options, `-a`/`-k` (candidates are the *values* of the array/assoc-array
/// NAMED by the remaining words, not the words themselves), and `-d` (the
/// per-candidate DISPLAY string shown in the listing in place of the match,
/// by the same name-array indirection -- not merely a "description": see
/// `_rip_trashed` in `completions/_rip`). Writes `CANDIDATE<TAB>DISPLAY`
/// records, one per candidate, to `$RIP_CAPFILE` (set by the caller before
/// sourcing this).
const ZSH_COMPADD_OVERRIDE: &str = r#"
compadd() {
    local -a bool_chars=(a k q Q f e n U l C 1 2)
    local -a arg_opts=(F P S p s i I W d J X x V r R D O A E M o)
    local -a args=("$@")
    local i=1 tok c mode=literal dispref= is_bool idx
    while (( i <= $#args )); do
        tok=$args[i]
        if [[ $tok == -- || $tok == - ]]; then
            (( i++ ))
            break
        fi
        if [[ $tok == -?* && $tok != --* ]]; then
            is_bool=1
            for (( c = 2; c <= $#tok; c++ )); do
                [[ ${bool_chars[(Ie)${tok[c]}]} -eq 0 ]] && is_bool=0 && break
            done
            if (( is_bool )); then
                [[ $tok == *a* ]] && mode=array
                [[ $tok == *k* ]] && mode=assoc
                (( i++ ))
                continue
            fi
            if [[ $#tok -eq 2 && ${arg_opts[(Ie)${tok[2]}]} -ne 0 ]]; then
                [[ $tok[2] == d ]] && dispref=$args[i+1]
                i=$((i + 2))
                continue
            fi
        fi
        break
    done
    local -a words=("${(@)args[i,-1]}")
    local -a cands=()
    case $mode in
    array) for tok in $words; do cands+=("${(@P)tok}"); done ;;
    assoc) for tok in $words; do cands+=("${(@k)${(P)tok}}"); done ;;
    *) cands=("$words[@]") ;;
    esac
    local -a disps=()
    [[ -n $dispref ]] && disps=("${(@P)dispref}")
    for (( idx = 1; idx <= $#cands; idx++ )); do
        print -r -- "${cands[idx]}"$'\t'"${disps[idx]:-}"
    done >> $RIP_CAPFILE
    builtin compadd "$@"
}
"#;

/// The sandbox's own `PATH` (`/run/rip/fakebin`, `/run/rip/bin`, then the
/// host's `/nix/store` entries -- see `Sandbox::command`'s comment on its
/// own `path()`), recomputed here because nixpkgs' zsh replaces `$PATH`
/// with its own default at startup -- even non-interactively, even with
/// `-f` -- before the driver script below runs a single line of its own,
/// so the zsh under test would otherwise lose the sandbox's PATH (and with
/// it, `rip` on it) the moment it starts.
fn zsh_safe_path() -> String {
    let mut entries = vec!["/run/rip/fakebin".to_string(), "/run/rip/bin".to_string()];
    if let Some(host_path) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&host_path) {
            if p.starts_with("/nix/store") {
                entries.push(p.to_string_lossy().into_owned());
            }
        }
    }
    entries.join(":")
}

/// Drives the real `_rip` (autoloaded from `/run/rip/completions` under a
/// real `compinit`) the way a person would: types `cmdline` into a real
/// interactive zsh in a pty (`zsh/zpty`, docs/design.md §2.1) and presses
/// Tab. `ZSH_COMPADD_OVERRIDE` is sourced first so every `compadd` call
/// along the way (`_rip`'s own, `_describe`'s, `_files`') is captured;
/// `LISTMAX=0` stops zsh from ever pausing to ask "do you wish to see all N
/// possibilities?". Returns (candidate, description) pairs; a candidate
/// with no description pairs with an empty string.
fn complete_zsh(sandbox: &Sandbox, cwd: &str, cmdline: &str) -> Vec<(String, String)> {
    let home = sandbox.host("/home/u");
    std::fs::create_dir_all(&home).expect("create the sandbox's $HOME on the host");
    std::fs::write(home.join(".rip-zsh-capture.zsh"), ZSH_COMPADD_OVERRIDE)
        .expect("write the zsh capture helper");

    let driver = format!(
        "export PATH={path}\n\
         zmodload zsh/zpty\n\
         CAPFILE=/tmp/rip-capture\n\
         : > $CAPFILE\n\
         zpty -b ripc zsh -f -i\n\
         zpty -w ripc 'export PATH={path}; PS1=; RPROMPT=; LISTMAX=0'\n\
         zpty -w ripc 'fpath=(/run/rip/completions $fpath)'\n\
         zpty -w ripc 'autoload -Uz compinit && compinit -u -d /tmp/zcompdump'\n\
         zpty -w ripc 'RIP_CAPFILE=/tmp/rip-capture; source /home/u/.rip-zsh-capture.zsh'\n\
         sleep 1.2\n\
         zpty -r -t ripc buf 2>/dev/null\n\
         zpty -w -n ripc {cmdline}\n\
         zpty -w -n ripc $'\\t'\n\
         sleep 1\n\
         zpty -r -t ripc buf 2>/dev/null\n\
         zpty -d ripc 2>/dev/null\n\
         cat $CAPFILE\n",
        path = zsh_safe_path(),
        cmdline = single_quote(cmdline),
    );
    let out = sandbox.exec(cwd, &["zsh", "-f", "-c", driver.as_str()]);
    assert!(
        out.status.success(),
        "zsh capture driver exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(w, d)| (w.to_string(), d.to_string()))
        .collect()
}

fn zsh_words(got: &[(String, String)]) -> Vec<String> {
    got.iter().map(|(w, _)| w.clone()).collect()
}

const SUBCOMMANDS: [&str; 5] = ["undo", "list", "restore", "empty", "purge"];

/// The working directory every scenario completes in.
const CWD: &str = "/home/u/Downloads";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shell {
    Fish,
    Bash,
    Zsh,
}

/// The candidate words `shell`'s real completion offers for `cmdline` in
/// `CWD` (fish: the word before its `\t` description; zsh: every word given
/// to `compadd`, without its display string).
fn candidates(shell: Shell, sandbox: &Sandbox, cmdline: &str) -> Vec<String> {
    match shell {
        Shell::Fish => complete_fish(sandbox, CWD, cmdline),
        Shell::Bash => complete_bash(sandbox, CWD, cmdline),
        Shell::Zsh => zsh_words(&complete_zsh(sandbox, CWD, cmdline)),
    }
}

fn write_cwd_file(sandbox: &Sandbox, name: &str) {
    std::fs::write(sandbox.host(CWD).join(name), b"x").unwrap();
}

fn plant_trashed(sandbox: &Sandbox, name: &[u8], original: &[u8], date: &str) {
    sandbox.plant(
        "/home/u/.local/share/Trash",
        name,
        original,
        date,
        Body::File(b"data".to_vec()),
    );
}

fn assert_all_subcommands(got: &[String], ctx: &str) {
    for sub in SUBCOMMANDS {
        assert!(
            got.iter().any(|w| w == sub),
            "missing subcommand {sub} {ctx}: {got:?}"
        );
    }
}

fn assert_no_subcommands(got: &[String], ctx: &str) {
    for sub in SUBCOMMANDS {
        assert!(
            !got.iter().any(|w| w == sub),
            "unexpected subcommand {sub} {ctx}: {got:?}"
        );
    }
}

fn assert_has(got: &[String], word: &str, ctx: &str) {
    assert!(
        got.iter().any(|w| w == word),
        "missing {word:?} {ctx}: {got:?}"
    );
}

// ---------------------------------------------------------------------------
// Rules every script must follow (docs/design.md §2.1): one scenario per
// rule, run by a thin `#[test]` per shell (separate tests, so the slow zsh
// pty runs stay parallel).
// ---------------------------------------------------------------------------

fn rip_gives_subcommands_and_files(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "fixture.txt");

    let got = candidates(shell, &sandbox, "rip ");
    assert_all_subcommands(&got, "at the first word");
    assert_has(&got, "fixture.txt", "at the first word");
}

fn double_dash_gives_no_subcommands(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "fixture.txt");

    let got = candidates(shell, &sandbox, "rip -- ");
    assert_no_subcommands(&got, "after --");
    assert_has(&got, "fixture.txt", "after -- (files must still complete)");
}

fn a_file_argument_gives_no_subcommands(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "somefile");

    let got = candidates(shell, &sandbox, "rip somefile ");
    assert_no_subcommands(&got, "after a file argument");
}

fn restore_gives_planted_trashed_paths(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    plant_trashed(
        &sandbox,
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
    );

    let got = candidates(shell, &sandbox, "rip restore ");
    assert_has(&got, "x", "(the planted trashed path)");
}

// c29: fish 4's qmark-noglob made `-?*` a dead pattern, so ANY option word
// (not just `--config=PATH`) fell to `case '*'`, and every one of them --
// including `--config=PATH` -- was treated as a non-option, ending the scan
// and reporting "files".
fn config_equals_form_still_gives_subcommands_at_the_first_word(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "c.toml");

    let got = candidates(shell, &sandbox, "rip --config=c.toml ");
    assert_all_subcommands(&got, "after --config=PATH");
}

fn config_equals_form_then_restore_gives_trashed_paths_not_cwd_files(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "c.toml");
    plant_trashed(
        &sandbox,
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
    );

    let got = candidates(shell, &sandbox, "rip --config=c.toml restore ");
    assert_has(&got, "x", "after --config=PATH restore");
    assert!(
        !got.iter().any(|w| w == "c.toml"),
        "must not fall back to cwd files after --config=PATH restore: {got:?}"
    );
}

// The value of `--config=` completes as a file. Real readline splits
// COMP_WORDS at `=` (a COMP_WORDBREAKS character), so bash sees
// `--config=c.toml` as three words; zsh keeps it as one word, so `_rip`
// strips the flag itself; fish completes the whole word, flag included.
fn config_equals_form_completes_files_for_the_value(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "c.toml");

    let want = match shell {
        Shell::Fish => "--config=c.toml",
        Shell::Bash | Shell::Zsh => "c.toml",
    };
    for line in ["rip --config=", "rip --config=c"] {
        let got = candidates(shell, &sandbox, line);
        assert_has(&got, want, &format!("for {line}<TAB>"));
    }
}

fn rm_style_flag_before_a_subcommand_gives_files_only(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "fixture.txt");

    let got = candidates(shell, &sandbox, "rip -f ");
    assert_no_subcommands(&got, "after an rm-style flag");
    assert_has(
        &got,
        "fixture.txt",
        "after an rm-style flag (files must still complete)",
    );
}

// -h is neither an rm-style flag nor a --config/--completions value
// (src/main.rs's first_word() leaves the scan state unchanged for it), so it
// must not be treated like `-f` and lock the state to "files".
fn help_flag_does_not_lock_out_subcommands(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    let got = candidates(shell, &sandbox, "rip -h ");
    assert_has(&got, "list", "after -h");
}

// c28: piping `string split0`'s output further (into `string replace`, then
// again through `complete`'s own `(__rip_trashed)` command substitution)
// re-splits on any newline embedded in a trashed path, so a hostile or
// merely oddly named file forged extra candidates out of whatever text
// followed the newline.
fn restore_skips_a_trashed_path_with_an_embedded_newline(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    // A trashed item whose original path holds a literal newline (e.g. from
    // an extracted archive) -- no planted/hostile topdir needed.
    plant_trashed(
        &sandbox,
        b"evil",
        b"/home/u/Downloads/a\n/etc/attacker-controlled",
        "2026-01-01T00:00:00",
    );
    // An unaffected item must still complete normally.
    plant_trashed(
        &sandbox,
        b"good",
        b"/home/u/Downloads/good",
        "2026-01-02T00:00:00",
    );

    let got = candidates(shell, &sandbox, "rip restore ");
    assert!(
        !got.iter().any(|w| w == "/etc/attacker-controlled"),
        "a newline in a trashed path must not forge a completion candidate: {got:?}"
    );
    assert!(
        !got.iter().any(|w| w == "a"),
        "the newline-bearing record must be skipped whole, not split: {got:?}"
    );
    assert_has(&got, "good", "(an item unaffected by the newline)");
}

// c30: bash's `COMPREPLY+=($(compgen ...))` word-split its output, so a file
// or a trashed path with a space was offered as two separate candidates
// instead of one; `_rip_add` (completions/rip.bash) reads compgen's matches
// one per line with `mapfile` instead. Every shell must offer each as one
// whole candidate.
fn candidates_with_a_space_are_not_split(shell: Shell) {
    let sandbox = sandbox_with_shell_completions();
    write_cwd_file(&sandbox, "has space");
    plant_trashed(
        &sandbox,
        b"x",
        b"/home/u/Downloads/has space too",
        "2026-01-01T00:00:00",
    );

    // zsh's own `_files` pre-escapes a match's special characters itself
    // (its compadd call passes `-Q`, "don't re-quote, I already did"), so
    // there the captured file candidate is "has\ space"; either way it must
    // be the file's whole name in one candidate, not two.
    let file = match shell {
        Shell::Zsh => "has\\ space",
        Shell::Fish | Shell::Bash => "has space",
    };
    let files = candidates(shell, &sandbox, "rip ");
    assert_has(&files, file, "(the file with a space, as one candidate)");

    let trashed = candidates(shell, &sandbox, "rip restore ");
    assert_has(
        &trashed,
        "has space too",
        "(the trashed path with a space, as one candidate)",
    );
}

#[test]
fn fish_rip_gives_subcommands_and_files() {
    rip_gives_subcommands_and_files(Shell::Fish);
}

#[test]
fn bash_rip_gives_subcommands_and_files() {
    rip_gives_subcommands_and_files(Shell::Bash);
}

#[test]
fn zsh_rip_gives_subcommands_and_files() {
    rip_gives_subcommands_and_files(Shell::Zsh);
}

#[test]
fn fish_double_dash_gives_no_subcommands() {
    double_dash_gives_no_subcommands(Shell::Fish);
}

#[test]
fn bash_double_dash_gives_no_subcommands() {
    double_dash_gives_no_subcommands(Shell::Bash);
}

#[test]
fn zsh_double_dash_gives_no_subcommands() {
    double_dash_gives_no_subcommands(Shell::Zsh);
}

#[test]
fn fish_a_file_argument_gives_no_subcommands() {
    a_file_argument_gives_no_subcommands(Shell::Fish);
}

#[test]
fn bash_a_file_argument_gives_no_subcommands() {
    a_file_argument_gives_no_subcommands(Shell::Bash);
}

#[test]
fn zsh_a_file_argument_gives_no_subcommands() {
    a_file_argument_gives_no_subcommands(Shell::Zsh);
}

#[test]
fn fish_restore_gives_planted_trashed_paths() {
    restore_gives_planted_trashed_paths(Shell::Fish);
}

#[test]
fn bash_restore_gives_planted_trashed_paths() {
    restore_gives_planted_trashed_paths(Shell::Bash);
}

#[test]
fn zsh_restore_gives_planted_trashed_paths() {
    restore_gives_planted_trashed_paths(Shell::Zsh);
}

#[test]
fn fish_config_equals_form_still_gives_subcommands_at_the_first_word() {
    config_equals_form_still_gives_subcommands_at_the_first_word(Shell::Fish);
}

#[test]
fn bash_config_equals_form_still_gives_subcommands_at_the_first_word() {
    config_equals_form_still_gives_subcommands_at_the_first_word(Shell::Bash);
}

#[test]
fn zsh_config_equals_form_still_gives_subcommands_at_the_first_word() {
    config_equals_form_still_gives_subcommands_at_the_first_word(Shell::Zsh);
}

#[test]
fn fish_config_equals_form_then_restore_gives_trashed_paths_not_cwd_files() {
    config_equals_form_then_restore_gives_trashed_paths_not_cwd_files(Shell::Fish);
}

#[test]
fn bash_config_equals_form_then_restore_gives_trashed_paths_not_cwd_files() {
    config_equals_form_then_restore_gives_trashed_paths_not_cwd_files(Shell::Bash);
}

#[test]
fn zsh_config_equals_form_then_restore_gives_trashed_paths_not_cwd_files() {
    config_equals_form_then_restore_gives_trashed_paths_not_cwd_files(Shell::Zsh);
}

#[test]
fn fish_config_equals_form_completes_files_for_the_value() {
    config_equals_form_completes_files_for_the_value(Shell::Fish);
}

#[test]
fn bash_config_equals_form_completes_files_for_the_value() {
    config_equals_form_completes_files_for_the_value(Shell::Bash);
}

#[test]
fn zsh_config_equals_form_completes_files_for_the_value() {
    config_equals_form_completes_files_for_the_value(Shell::Zsh);
}

#[test]
fn fish_rm_style_flag_before_a_subcommand_gives_files_only() {
    rm_style_flag_before_a_subcommand_gives_files_only(Shell::Fish);
}

#[test]
fn bash_rm_style_flag_before_a_subcommand_gives_files_only() {
    rm_style_flag_before_a_subcommand_gives_files_only(Shell::Bash);
}

#[test]
fn zsh_rm_style_flag_before_a_subcommand_gives_files_only() {
    rm_style_flag_before_a_subcommand_gives_files_only(Shell::Zsh);
}

#[test]
fn fish_help_flag_does_not_lock_out_subcommands() {
    help_flag_does_not_lock_out_subcommands(Shell::Fish);
}

#[test]
fn bash_help_flag_does_not_lock_out_subcommands() {
    help_flag_does_not_lock_out_subcommands(Shell::Bash);
}

#[test]
fn zsh_help_flag_does_not_lock_out_subcommands() {
    help_flag_does_not_lock_out_subcommands(Shell::Zsh);
}

#[test]
fn fish_restore_skips_a_trashed_path_with_an_embedded_newline() {
    restore_skips_a_trashed_path_with_an_embedded_newline(Shell::Fish);
}

#[test]
fn bash_restore_skips_a_trashed_path_with_an_embedded_newline() {
    restore_skips_a_trashed_path_with_an_embedded_newline(Shell::Bash);
}

#[test]
fn zsh_restore_skips_a_trashed_path_with_an_embedded_newline() {
    restore_skips_a_trashed_path_with_an_embedded_newline(Shell::Zsh);
}

#[test]
fn fish_candidates_with_a_space_are_not_split() {
    candidates_with_a_space_are_not_split(Shell::Fish);
}

#[test]
fn bash_candidates_with_a_space_are_not_split() {
    candidates_with_a_space_are_not_split(Shell::Bash);
}

#[test]
fn zsh_candidates_with_a_space_are_not_split() {
    candidates_with_a_space_are_not_split(Shell::Zsh);
}

// ---------------------------------------------------------------------------
// bash only: readline's COMP_WORDBREAKS splitting and backslash escapes.
// ---------------------------------------------------------------------------

// `:` is also a COMP_WORDBREAKS character, so a colon-bearing trashed path
// (e.g. a screenshot's default name, "...at 10:00:00.png") splits mid-word
// too: the word under the cursor is only the part after the last colon.
// Without trimming a match back down to that same tail, the already-typed
// part of the prefix would be inserted a second time.
#[test]
fn bash_restore_completes_a_colon_bearing_trashed_path_without_duplicating_it() {
    let sandbox = sandbox_with_shell_completions();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"shot",
        b"/home/u/Downloads/10:00:00.png",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let got = complete_bash(&sandbox, "/home/u/Downloads", "rip restore 10:00:0");
    assert!(
        got.iter().any(|w| w == "00.png"),
        "must complete to only the part after the last colon, not the whole \
         path again: {got:?}"
    );
}

// After readline inserts a backslash escape for a space (or a person types
// one directly), COMP_WORDS[COMP_CWORD] still carries the backslash, so a
// literal-string match against a real (unescaped) name fails once any
// prefix is typed. compgen -f has the same problem matching a raw escaped
// prefix against real file names.
#[test]
fn bash_restore_completes_an_escaped_partial_prefix_with_a_space() {
    let sandbox = sandbox_with_shell_completions();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/has space too",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let got = complete_bash(&sandbox, "/home/u/Downloads", "rip restore has\\ spa");
    assert!(
        got.iter().any(|w| w == "has space too"),
        "an escaped partial prefix must still match the trashed path: {got:?}"
    );
}

#[test]
fn bash_completes_an_escaped_partial_prefix_with_a_space_for_a_file() {
    let sandbox = sandbox_with_shell_completions();
    std::fs::write(sandbox.host("/home/u/Downloads").join("has space"), b"x").unwrap();

    let got = complete_bash(&sandbox, "/home/u/Downloads", "rip has\\ spa");
    assert!(
        got.iter().any(|w| w == "has space"),
        "an escaped partial prefix must still match the file: {got:?}"
    );
}

// ---------------------------------------------------------------------------
// zsh only: the restore menu's display strings.
// ---------------------------------------------------------------------------

#[test]
fn zsh_restore_menu_shows_each_path_and_its_date() {
    let sandbox = sandbox_with_shell_completions();
    sandbox.plant(
        "/home/u/.local/share/Trash",
        b"x",
        b"/home/u/Downloads/x",
        "2026-01-01T00:00:00",
        Body::File(b"data".to_vec()),
    );

    let got = complete_zsh(&sandbox, "/home/u/Downloads", "rip restore ");
    let entry = got.iter().find(|(w, _)| w == "x");
    assert!(
        entry.is_some(),
        "expected the planted trashed path 'x': {got:?}"
    );
    let display = &entry.unwrap().1;
    // `compadd -d dates -a paths` (without `-l`) makes each date the
    // display string that *replaces* the match in the listing, so a person
    // restoring would see only dates and no paths at all.
    assert!(
        display.contains('x'),
        "the restore menu's display for 'x' must still show its path: {got:?}"
    );
    assert!(
        display.contains("2026-01-01 00:00:00"),
        "the restore menu's display for 'x' must also show its deletion date: {got:?}"
    );
}
