#[cfg(not(target_os = "linux"))]
compile_error!("rip supports Linux only");

mod empty;
mod info;
mod mounts;
mod put;
mod restore;
mod sys;
mod trash;

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, ValueHint};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    match parse(args) {
        Ok(cli) => ExitCode::from(dispatch(cli)),
        Err(e) => e.exit(),
    }
}

/// Dispatches a successfully parsed `Cli` and returns the process exit code
/// (docs/design.md §2.2). Returns a plain `u8` rather than `ExitCode` (which
/// has no `PartialEq`) so tests can assert on the result directly; `main` is
/// the only caller that wraps it for the real process exit.
fn dispatch(cli: Cli) -> u8 {
    if let Some(shell) = cli.completions {
        let mut out = io::stdout().lock();
        let _ = out.write_all(&completions_script(shell));
        return completions_exit_code();
    }

    if cli.cmd.is_none() && cli.files.is_empty() {
        if cli.force {
            return 0;
        }
        eprintln!("rip: missing operand");
        return 2;
    }

    let home = match home_dir() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("rip: {e}");
            return 2;
        }
    };
    let cfg = match load_config(cli.config.as_deref(), &config_home(&home)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("rip: {e}");
            return 2;
        }
    };
    let cx = match Cx::new(cfg, home) {
        Ok(cx) => cx,
        Err(e) => {
            eprintln!("rip: {e}");
            return 2;
        }
    };

    let result = match &cli.cmd {
        None => put::run(&cx, &cli),
        Some(Cmd::Undo { yes }) => restore::undo(&cx, *yes),
        Some(Cmd::List { all, null }) => restore::list(&cx, *all, *null),
        Some(Cmd::Restore {
            all,
            rename,
            yes,
            paths,
        }) => restore::restore(&cx, paths, *all, *rename, *yes),
        Some(Cmd::Empty {
            older_than,
            max_size,
            yes,
        }) => empty::run(&cx, *older_than, *max_size, *yes),
        Some(Cmd::Purge { all, yes, paths }) => restore::purge(&cx, paths, *all, *yes),
    };

    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("rip: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// CLI types
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "rip",
    version,
    disable_help_subcommand = true,
    about = "Move files to the freedesktop.org trash",
    after_help = "A subcommand is recognized only as the first word (after an optional --config PATH).\n\
                  To trash a file with a subcommand's name, write: rip -- NAME"
)]
pub struct Cli {
    /// Files to trash; every word after `--` is a file
    #[arg(value_name = "FILE", value_hint = ValueHint::AnyPath)]
    pub files: Vec<PathBuf>,
    /// Ignored: trashing is always recursive
    #[arg(short = 'r', visible_short_alias = 'R', long = "recursive")]
    #[allow(dead_code)]
    recursive: bool,
    /// Ignored
    #[arg(short = 'd', long = "dir")]
    #[allow(dead_code)]
    dir: bool,
    /// Ignore missing files and never prompt
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Prompt before trashing each item
    #[arg(short = 'i')]
    pub interactive: bool,
    /// Prompt once before trashing more than three items or any directory
    #[arg(short = 'I')]
    pub interactive_once: bool,
    /// Print each item as it is trashed
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Config file [default: $XDG_CONFIG_HOME/ripper/config.toml]
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long, hide = true, exclusive = true, value_enum, value_name = "SHELL")]
    pub completions: Option<Shell>,
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Restore every item with the newest deletion date
    Undo {
        #[arg(short, long)]
        yes: bool,
    },
    /// List trashed items whose original path is under the current directory
    List {
        #[arg(short, long)]
        all: bool,
        /// NUL-terminated records for scripts and completions
        #[arg(short = '0', long)]
        null: bool,
    },
    /// Restore items (no PATH: pick with fzf)
    Restore {
        #[arg(short, long, conflicts_with = "paths")]
        all: bool,
        /// If the original path exists, restore beside it as NAME~N
        #[arg(long)]
        rename: bool,
        #[arg(short, long)]
        yes: bool,
        #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
        paths: Vec<PathBuf>,
    },
    /// Permanently delete trashed items (no filter: everything in every trash)
    Empty {
        #[arg(long, value_name = "DUR", value_parser = parse_age)]
        older_than: Option<jiff::Span>,
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        max_size: Option<u64>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Permanently delete chosen items (no PATH: pick with fzf)
    Purge {
        #[arg(short, long, conflicts_with = "paths")]
        all: bool,
        #[arg(short, long)]
        yes: bool,
        #[arg(value_name = "PATH", value_hint = ValueHint::AnyPath)]
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

// ---------------------------------------------------------------------------
// The argv pre-scan (docs/design.md §2.1)
// ---------------------------------------------------------------------------

pub fn parse(args: Vec<OsString>) -> Result<Cli, clap::Error> {
    let mut cmd = Cli::command();
    match first_word(&cmd, &args) {
        First::SubcommandAfterRmFlags(name) => {
            return Err(cmd.error(
                ErrorKind::ArgumentConflict,
                format!(
                    "rm-style flags cannot come before the subcommand '{name}'\n  \
                     to trash a file named '{name}', write: rip -- {name}"
                ),
            ));
        }
        First::File => cmd = cmd.args_conflicts_with_subcommands(true),
        First::Subcommand | First::None => {}
    }
    Cli::from_arg_matches(&cmd.try_get_matches_from(args)?)
}

enum First {
    None,
    File,
    Subcommand,
    SubcommandAfterRmFlags(String),
}

/// Classifies the first word that is not an option. The values of --config and
/// --completions are skipped.
fn first_word(cmd: &clap::Command, args: &[OsString]) -> First {
    let mut rm_flags = false;
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_bytes() {
            b"--" => return First::File,
            b"--config" | b"--completions" => {
                it.next();
            }
            a if a.starts_with(b"--config=") || a.starts_with(b"--completions=") => {}
            b"-h" | b"--help" | b"-V" | b"--version" => {}
            [b'-', _, ..] => rm_flags = true,
            _ => {
                return match arg.to_str().filter(|w| cmd.find_subcommand(w).is_some()) {
                    Some(w) if rm_flags => First::SubcommandAfterRmFlags(w.to_owned()),
                    Some(_) => First::Subcommand,
                    None => First::File, // includes non-UTF-8 words and "-"
                };
            }
        }
    }
    First::None
}

// ---------------------------------------------------------------------------
// Prompts and value parsers
// ---------------------------------------------------------------------------

/// Without a terminal nobody can answer. The caller then does nothing irreversible.
pub fn confirm(question: &str, skip_flag: &str) -> Result<bool, String> {
    if !io::stdin().is_terminal() {
        return Err(format!(
            "{question} (no terminal to ask on; {skip_flag} skips this question)"
        ));
    }
    eprint!("rip: {question} [y/N] ");
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// "500M", "50G", "1048576", "4KiB": binary units. KB/MB/GB are rejected so
/// nobody reads them as SI.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let d = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let n: u64 = t[..d]
        .parse()
        .map_err(|_| format!("invalid size {s:?}: use a whole number with K, M, G or T"))?;
    let shift = match &t[d..] {
        "" | "B" => 0,
        "K" | "k" | "KiB" => 10,
        "M" | "m" | "MiB" => 20,
        "G" | "g" | "GiB" => 30,
        "T" | "t" | "TiB" => 40,
        u if ["kb", "mb", "gb", "tb"].contains(&u.to_ascii_lowercase().as_str()) => {
            return Err(format!(
                "invalid size {s:?}: units are binary; write {}{}",
                &t[..d],
                u[..1].to_uppercase()
            ));
        }
        _ => return Err(format!("invalid size {s:?}: unknown unit")),
    };
    n.checked_mul(1u64 << shift)
        .ok_or_else(|| format!("size {s:?} is too large"))
}

/// jiff span (friendly or ISO 8601). A negative span is rejected: `--older-than
/// -30d` would delete everything.
fn parse_age(s: &str) -> Result<jiff::Span, String> {
    let span: jiff::Span = s
        .parse()
        .map_err(|e| format!("invalid duration {s:?}: {e}"))?;
    if span.is_negative() {
        return Err(format!("invalid duration {s:?}: must not be negative"));
    }
    Ok(span)
}

/// Writes control bytes as `\n`, `\t` and `\xNN`, invalid UTF-8 as `\xNN`, and a
/// backslash as `\\`. Used for human output to a terminal and for fzf display.
#[allow(dead_code)] // not called until list/restore/fzf display land (C3/C4b)
pub fn escape(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    out.push_str(&format!("\\x{:02x}", c as u32))
                }
                c => out.push(c),
            }
        }
        for &byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out
}

/// Binary sizes, e.g. `1.2 GiB`.
#[allow(dead_code)] // not called until put/restore size prompts land (C4a/C4b)
pub fn human(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fallback {
    Copy,
    Refuse,
}

#[allow(dead_code)] // cfg.source is read once put's copy-fallback messages land (C4a)
#[derive(Clone, Debug)]
pub struct Config {
    pub fallback: Fallback,
    pub copy_threshold: u64,
    pub source: PathBuf,
}

impl Config {
    fn default_at(source: PathBuf) -> Self {
        Config {
            fallback: Fallback::Copy,
            copy_threshold: 500 << 20,
            source,
        }
    }
}

/// `$VAR` if it is set, non-empty and absolute.
fn env_abs(var: &str) -> Option<PathBuf> {
    let v = std::env::var_os(var)?;
    if v.is_empty() {
        return None;
    }
    let p = PathBuf::from(v);
    p.is_absolute().then_some(p)
}

fn home_dir() -> Result<PathBuf, String> {
    let h = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    let p = PathBuf::from(h);
    if !p.is_absolute() {
        return Err("HOME is not an absolute path".into());
    }
    Ok(p)
}

/// `$XDG_CONFIG_HOME` if it is set, non-empty and absolute; otherwise `home/.config`.
fn config_home(home: &Path) -> PathBuf {
    env_abs("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
}

/// `config_dir` is the resolved `$XDG_CONFIG_HOME` (or `$HOME/.config`); it is
/// only consulted when `explicit` is `None`.
fn load_config(explicit: Option<&Path>, config_dir: &Path) -> Result<Config, String> {
    let path = match explicit {
        Some(p) => p.to_owned(),
        None => config_dir.join("ripper/config.toml"),
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound && explicit.is_none() => {
            return Ok(Config::default_at(path));
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut cfg = Config::default_at(path.clone());
    for (key, value) in table {
        match (key.as_str(), value.as_str()) {
            ("fallback", Some("copy")) => cfg.fallback = Fallback::Copy,
            ("fallback", Some("refuse")) => cfg.fallback = Fallback::Refuse,
            ("copy_threshold", Some(s)) => {
                cfg.copy_threshold = parse_size(s)
                    .map_err(|e| format!("{}: copy_threshold: {e}", path.display()))?;
            }
            ("fallback" | "copy_threshold", _) => {
                return Err(format!(
                    "{}: invalid value for {key}: {value:?}",
                    path.display()
                ));
            }
            _ => return Err(format!("{}: unknown key {key}", path.display())),
        }
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

fn completions_script(shell: Shell) -> Vec<u8> {
    match shell {
        Shell::Fish => include_bytes!("../completions/rip.fish").to_vec(),
        Shell::Bash => generate_completion(clap_complete::aot::Shell::Bash),
        Shell::Zsh => generate_completion(clap_complete::aot::Shell::Zsh),
    }
}

fn generate_completion(shell: clap_complete::aot::Shell) -> Vec<u8> {
    let mut buf = Vec::new();
    clap_complete::aot::generate(shell, &mut Cli::command(), "rip", &mut buf);
    buf
}

/// `--completions` prints the script and succeeds (docs/design.md §2.2):
/// only combining it with another argument is a usage error, and clap's own
/// `exclusive` check rejects that before `dispatch` ever sees it.
fn completions_exit_code() -> u8 {
    0
}

// ---------------------------------------------------------------------------
// Shared context
// ---------------------------------------------------------------------------

/// Built once after argv parsing and config loading. Read by put/restore/empty
/// once those modules are implemented (C4a-C4c); nothing reads it yet.
#[allow(dead_code)]
pub struct Cx {
    pub cfg: Config,
    pub mounts: mounts::Mounts,
    pub uid: u32,
    pub cwd: PathBuf,
    pub home: PathBuf,
}

impl Cx {
    fn new(cfg: Config, home: PathBuf) -> Result<Self, String> {
        let uid = rustix::process::getuid().as_raw();
        let cwd = std::env::current_dir()
            .map_err(|e| format!("cannot determine the current directory: {e}"))?;
        let mounts = mounts::Mounts::read().map_err(|e| e.to_string())?;
        Ok(Cx {
            cfg,
            mounts,
            uid,
            cwd,
            home,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn argv(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn try_parse(words: &[&str]) -> Result<Cli, clap::Error> {
        parse(argv(words))
    }

    fn paths(words: &[&str]) -> Vec<PathBuf> {
        words.iter().map(PathBuf::from).collect()
    }

    // ---- argv pre-scan table (docs/design.md §2.1) ----

    #[test]
    fn plain_files() {
        let cli = try_parse(&["rip", "foo"]).unwrap();
        assert_eq!(cli.files, paths(&["foo"]));
        assert!(cli.cmd.is_none());
    }

    #[test]
    fn file_then_subcommand_name_is_a_file() {
        let cli = try_parse(&["rip", "foo", "empty"]).unwrap();
        assert_eq!(cli.files, paths(&["foo", "empty"]));
        assert!(cli.cmd.is_none());
    }

    #[test]
    fn file_then_flag_then_subcommand_name_is_a_file_with_force() {
        let cli = try_parse(&["rip", "foo", "-f", "empty"]).unwrap();
        assert_eq!(cli.files, paths(&["foo", "empty"]));
        assert!(cli.force);
        assert!(cli.cmd.is_none());
    }

    #[test]
    fn file_then_config_then_subcommand_name_is_a_file() {
        let cli = try_parse(&["rip", "foo", "--config", "c", "empty"]).unwrap();
        assert_eq!(cli.files, paths(&["foo", "empty"]));
        assert_eq!(cli.config, Some(PathBuf::from("c")));
        assert!(cli.cmd.is_none());
    }

    #[test]
    fn yes_is_not_a_root_flag() {
        assert!(try_parse(&["rip", "foo", "--config", "c", "empty", "-y"]).is_err());
    }

    #[test]
    fn bare_subcommand_names() {
        for words in [
            &["rip", "empty"][..],
            &["rip", "--config", "c", "empty"][..],
            &["rip", "--config=c", "list"][..],
            &["rip", "empty", "--config", "c"][..],
        ] {
            let cli = try_parse(words).unwrap_or_else(|e| panic!("{words:?}: {e}"));
            assert!(cli.cmd.is_some(), "{words:?} should parse as a subcommand");
        }
    }

    #[test]
    fn double_dash_makes_everything_a_file() {
        let cli = try_parse(&["rip", "--", "empty"]).unwrap();
        assert_eq!(cli.files, paths(&["empty"]));

        let cli = try_parse(&["rip", "-f", "--", "-f"]).unwrap();
        assert_eq!(cli.files, paths(&["-f"]));
        assert!(cli.force);

        let cli = try_parse(&["rip", "--", "--", "x"]).unwrap();
        assert_eq!(cli.files, paths(&["--", "x"]));
    }

    #[test]
    fn rm_flags_before_subcommand_is_an_error_with_hint() {
        for words in [
            &["rip", "-rf", "empty"][..],
            &["rip", "-v", "list"][..],
            &["rip", "--config", "c", "-v", "empty"][..],
        ] {
            let err = match try_parse(words) {
                Err(e) => e,
                Ok(cli) => panic!("{words:?} should be an error, got {cli:?}"),
            };
            let msg = err.to_string();
            assert!(msg.contains("rip -- "), "{words:?}: {msg}");
        }
    }

    #[test]
    fn dash_f_alone_before_subcommand_is_also_an_error() {
        let err = try_parse(&["rip", "-f", "empty"]).unwrap_err();
        assert!(err.to_string().contains("rip -- empty"), "{err}");
    }

    #[test]
    fn help_and_bare_dash_are_files() {
        let cli = try_parse(&["rip", "help"]).unwrap();
        assert_eq!(cli.files, paths(&["help"]));

        let cli = try_parse(&["rip", "-"]).unwrap();
        assert_eq!(cli.files, paths(&["-"]));
    }

    #[test]
    fn completions_flag_parses_alone() {
        let cli = try_parse(&["rip", "--completions", "fish"]).unwrap();
        assert_eq!(cli.completions, Some(Shell::Fish));
    }

    #[test]
    fn completions_flag_conflicts_with_other_args() {
        assert!(try_parse(&["rip", "--completions", "fish", "foo"]).is_err());
    }

    #[test]
    fn completions_flag_exits_success() {
        // `--completions` prints the script and succeeds; only combining it
        // with another argument is a (clap-level) usage error (docs/design.md
        // §2.2), already covered by `completions_flag_conflicts_with_other_args`.
        assert_eq!(completions_exit_code(), 0);
    }

    // ---- extra cases ----

    #[test]
    fn file_then_verbose_then_subcommand_name() {
        let cli = try_parse(&["rip", "foo", "-v", "empty"]).unwrap();
        assert_eq!(cli.files, paths(&["foo", "empty"]));
        assert!(cli.verbose);
    }

    #[test]
    fn list_flags() {
        let cli = try_parse(&["rip", "list", "-0", "-a"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::List {
                all: true,
                null: true
            })
        ));
    }

    #[test]
    fn restore_dash_x_after_double_dash() {
        let cli = try_parse(&["rip", "restore", "--", "-x"]).unwrap();
        match cli.cmd {
            Some(Cmd::Restore { paths: got, .. }) => assert_eq!(got, paths(&["-x"])),
            other => panic!("expected Restore, got {other:?}"),
        }
    }

    #[test]
    fn restore_all_conflicts_with_paths() {
        assert!(try_parse(&["rip", "restore", "--all", "a"]).is_err());
    }

    #[test]
    fn empty_with_both_filters() {
        let cli = try_parse(&[
            "rip",
            "empty",
            "--older-than",
            "30d",
            "--max-size",
            "50G",
            "-y",
        ])
        .unwrap();
        match cli.cmd {
            Some(Cmd::Empty {
                older_than,
                max_size,
                yes,
            }) => {
                assert!(older_than.is_some());
                assert_eq!(max_size, Some(50 * (1u64 << 30)));
                assert!(yes);
            }
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[test]
    fn empty_rejects_bad_values() {
        assert!(try_parse(&["rip", "empty", "--older-than", "-3d"]).is_err());
        assert!(try_parse(&["rip", "empty", "--max-size", "5GB"]).is_err());
        assert!(try_parse(&["rip", "empty", "--older-than", "banana"]).is_err());
    }

    #[test]
    fn non_utf8_operand_is_a_file() {
        let bad = OsStr::from_bytes(&[0x66, 0x6f, 0x80, 0x6f]); // "fo<0x80>o"
        let cli = parse(vec![OsString::from("rip"), bad.to_owned()]).unwrap();
        assert_eq!(cli.files, vec![PathBuf::from(bad)]);
    }

    #[test]
    fn command_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    // ---- config ----

    #[test]
    fn config_missing_default_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_config(None, dir.path()).unwrap();
        assert_eq!(cfg.fallback, Fallback::Copy);
        assert_eq!(cfg.copy_threshold, 500 << 20);
    }

    #[test]
    fn config_missing_explicit_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        assert!(load_config(Some(&missing), dir.path()).is_err());
    }

    fn write_config(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn config_rejects_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "nonsense = 1\n");
        assert!(load_config(Some(&path), dir.path()).is_err());
    }

    #[test]
    fn config_rejects_bad_fallback_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "fallback = \"maybe\"\n");
        assert!(load_config(Some(&path), dir.path()).is_err());
    }

    #[test]
    fn config_rejects_non_string_copy_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "copy_threshold = 5\n");
        assert!(load_config(Some(&path), dir.path()).is_err());
    }

    #[test]
    fn config_rejects_bad_copy_threshold_unit() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "copy_threshold = \"5X\"\n");
        assert!(load_config(Some(&path), dir.path()).is_err());
    }

    #[test]
    fn config_valid_file_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "fallback = \"refuse\"\ncopy_threshold = \"250M\"\n",
        );
        let cfg = load_config(Some(&path), dir.path()).unwrap();
        assert_eq!(cfg.fallback, Fallback::Refuse);
        assert_eq!(cfg.copy_threshold, 250 << 20);
    }

    // ---- parse_size / human / escape ----

    #[test]
    fn parse_size_units_and_case() {
        assert_eq!(parse_size("500M").unwrap(), 500 * (1u64 << 20));
        assert_eq!(parse_size("50G").unwrap(), 50 * (1u64 << 30));
        assert_eq!(parse_size("1048576").unwrap(), 1_048_576);
        assert_eq!(parse_size("4KiB").unwrap(), 4 * 1024);
        assert_eq!(parse_size("4k").unwrap(), 4 * 1024);
        assert_eq!(parse_size("0B").unwrap(), 0);
        assert_eq!(parse_size("1T").unwrap(), 1u64 << 40);
    }

    #[test]
    fn parse_size_rejects_si_units_with_hint() {
        let err = parse_size("5MB").unwrap_err();
        assert!(err.contains("5M"), "{err}");
        let err = parse_size("500kb").unwrap_err();
        assert!(err.contains("500K"), "{err}");
    }

    #[test]
    fn parse_size_rejects_non_integer() {
        assert!(parse_size("1.5G").is_err());
    }

    #[test]
    fn parse_size_overflow() {
        assert!(parse_size("99999999999999999999T").is_err());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(500 << 20), "500.0 MiB");
    }

    #[test]
    fn escape_bytes() {
        assert_eq!(escape(b"plain"), "plain");
        assert_eq!(escape(b"a\\b"), "a\\\\b");
        assert_eq!(escape(b"a\nb\tc"), "a\\nb\\tc");
        assert_eq!(escape(&[0x41, 0x01, 0x42]), "A\\x01B");
        assert_eq!(escape(&[0xff, 0x41]), "\\xffA");
    }

    #[test]
    fn confirm_without_a_terminal_fails_clearly() {
        // `cargo test` on artemis/CI has no controlling terminal, so this
        // exercises the real "no terminal" path. A developer running `cargo
        // test` interactively has a real stdin, which would make `confirm`
        // block on `read_line`; skip in that case rather than hang. The
        // sandbox tests (a later checkpoint) cover this path with a `rip`
        // that genuinely never has a terminal.
        if io::stdin().is_terminal() {
            return;
        }
        let err = confirm("proceed?", "-y").unwrap_err();
        assert!(err.contains("-y"), "{err}");
    }

    // ---- completions ----

    #[test]
    fn completions_bash_and_zsh_nonempty() {
        assert!(!completions_script(Shell::Bash).is_empty());
        assert!(!completions_script(Shell::Zsh).is_empty());
    }

    #[test]
    fn completions_fish_matches_file() {
        let expected = include_bytes!("../completions/rip.fish").to_vec();
        assert_eq!(completions_script(Shell::Fish), expected);
    }
}
