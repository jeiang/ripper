//! `.trashinfo` encode/parse, `Path=` percent-encoding, `DeletionDate` parsing,
//! trash `Kind`, `Path=` -> absolute path resolution, and `NAME_MAX`-safe
//! collision names. See docs/design.md §4 (on-disk names).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use jiff::civil;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode, percent_encode};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Home,
    /// `$topdir/.Trash/$uid`
    Admin,
    /// `$topdir/.Trash-$uid`
    User,
}

/// Bytes left unescaped in a `Path=` value: unreserved (`A-Za-z0-9-._~`) plus
/// `/`, matching gio's `percent_encoding` behavior.
const PATH_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

pub const INFO_SUFFIX: &str = ".trashinfo";

/// Builds a `.trashinfo` file's contents. `date` is formatted with no
/// fractional seconds, matching the spec's `DeletionDate`.
pub fn encode(path_field: &[u8], date: civil::DateTime) -> Vec<u8> {
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(path_field, PATH_SET),
        date.strftime("%Y-%m-%dT%H:%M:%S")
    )
    .into_bytes()
}

/// Parses a `.trashinfo` file's contents. The first `Path=` and
/// `DeletionDate=` line in the `[Trash Info]` group win; unknown keys and
/// lines without `=` are ignored, and parsing stops at the next `[Group]`
/// header. Each line has its trailing CR (and any other ASCII whitespace)
/// stripped before matching.
pub fn parse(text: &[u8]) -> Result<(Vec<u8>, civil::DateTime), &'static str> {
    let mut lines = text.split(|&b| b == b'\n').map(<[u8]>::trim_ascii_end);
    if lines.next() != Some(b"[Trash Info]") {
        return Err("first line is not [Trash Info]");
    }
    let (mut path, mut date) = (None, None);
    for l in lines.take_while(|l| !l.starts_with(b"[")) {
        if let Some(v) = l.strip_prefix(b"Path=") {
            path.get_or_insert(v);
        } else if let Some(v) = l.strip_prefix(b"DeletionDate=") {
            date.get_or_insert(v);
        }
    }
    let date = parse_date(date.ok_or("no DeletionDate")?).ok_or("bad DeletionDate")?;
    // Plain percent-decoding: unlike form decoding, a literal '+' stays '+'.
    Ok((percent_decode(path.ok_or("no Path")?).collect(), date))
}

/// Accepts the spec's dashed `DeletionDate` (civil `DateTime`'s own format,
/// `YYYY-MM-DDThh:mm:ss`) and the undashed form some writers use
/// (`YYYYMMDDThh:mm:ss`).
fn parse_date(v: &[u8]) -> Option<civil::DateTime> {
    let s = std::str::from_utf8(v).ok()?.trim();
    s.parse()
        .ok()
        .or_else(|| civil::DateTime::strptime("%Y%m%dT%H:%M:%S", s).ok())
}

/// Resolves a decoded `Path=` value to an absolute original path. Rejects an
/// empty path, a NUL byte, and any `..` component. For `Admin` and `User`
/// trashes the result must land strictly beneath `base` (`$topdir`); `Home`
/// accepts any absolute path, since it is written as the user reached it and
/// may point outside `$XDG_DATA_HOME`.
pub fn original(kind: Kind, base: &Path, decoded: &[u8]) -> Result<PathBuf, &'static str> {
    if decoded.is_empty() || decoded.contains(&0) {
        return Err("empty Path or NUL byte");
    }
    let p = Path::new(OsStr::from_bytes(decoded));
    if p.components().any(|c| c == Component::ParentDir) {
        return Err("'..' in Path");
    }
    let abs: PathBuf = (if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    })
    .components()
    .collect();
    if kind != Kind::Home && (!abs.starts_with(base) || abs == base) {
        return Err("Path is outside its topdir");
    }
    Ok(abs)
}

/// `name`, or `name~k` for `k > 0`, cut on bytes so the result plus `reserve`
/// bytes (for example `INFO_SUFFIX`) fits `NAME_MAX` (255 bytes). A name that
/// is valid UTF-8 is cut back to a char boundary instead of splitting a
/// codepoint; a non-UTF-8 name is cut on the raw byte count.
pub fn candidate(name: &OsStr, k: u64, reserve: usize) -> OsString {
    let suffix = if k == 0 {
        String::new()
    } else {
        format!("~{k}")
    };
    let mut end = name.len().min(255 - reserve - suffix.len());
    if let Some(s) = name.to_str() {
        while !s.is_char_boundary(end) {
            end -= 1;
        }
    }
    let mut v = name.as_bytes()[..end].to_vec();
    v.extend_from_slice(suffix.as_bytes());
    OsString::from_vec(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> civil::DateTime {
        civil::date(y, mo, d).at(h, mi, s, 0)
    }

    // --- encode / parse round trip -----------------------------------

    #[test]
    fn round_trips_every_byte_value() {
        let path_field: Vec<u8> = (0u8..=255).collect();
        let dt = date(2026, 9, 21, 10, 30, 5);
        let text = encode(&path_field, dt);
        let (decoded, parsed) = parse(&text).unwrap();
        assert_eq!(decoded, path_field);
        assert_eq!(parsed, dt);
    }

    #[test]
    fn encode_escapes_space_as_percent_20_and_plus_as_percent_2b() {
        let text = encode(b" +", date(2026, 1, 1, 0, 0, 0));
        assert!(
            text.starts_with(b"[Trash Info]\nPath=%20%2B\n"),
            "{:?}",
            String::from_utf8_lossy(&text)
        );
    }

    #[test]
    fn encode_leaves_unreserved_dash_dot_underscore_tilde_slash_literal() {
        let text = encode(b"-._~/AZaz09", date(2026, 1, 1, 0, 0, 0));
        assert!(
            text.starts_with(b"[Trash Info]\nPath=-._~/AZaz09\n"),
            "{:?}",
            String::from_utf8_lossy(&text)
        );
    }

    #[test]
    fn decode_leaves_a_literal_plus_as_plus() {
        let (decoded, _) =
            parse(b"[Trash Info]\nPath=a+b\nDeletionDate=2026-01-01T00:00:00\n").unwrap();
        assert_eq!(decoded, b"a+b");
    }

    // --- parse edge cases ----------------------------------------------

    #[test]
    fn parse_valid() {
        let (decoded, dt) =
            parse(b"[Trash Info]\nPath=/home/user/a%20b\nDeletionDate=2026-09-21T10:30:00\n")
                .unwrap();
        assert_eq!(decoded, b"/home/user/a b");
        assert_eq!(dt, date(2026, 9, 21, 10, 30, 0));
    }

    #[test]
    fn parse_accepts_undashed_deletion_date() {
        let (_, dt) = parse(b"[Trash Info]\nPath=x\nDeletionDate=20260921T10:30:00\n").unwrap();
        assert_eq!(dt, date(2026, 9, 21, 10, 30, 0));
    }

    #[test]
    fn parse_strips_crlf_line_endings() {
        let (decoded, dt) =
            parse(b"[Trash Info]\r\nPath=x\r\nDeletionDate=2026-09-21T10:30:00\r\n").unwrap();
        assert_eq!(decoded, b"x");
        assert_eq!(dt, date(2026, 9, 21, 10, 30, 0));
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let (decoded, _) =
            parse(b"[Trash Info]\nFoo=bar\nPath=x\nDeletionDate=2026-09-21T10:30:00\nBaz=qux\n")
                .unwrap();
        assert_eq!(decoded, b"x");
    }

    #[test]
    fn parse_first_path_and_date_win_on_duplicates() {
        let (decoded, dt) = parse(
            b"[Trash Info]\nPath=first\nPath=second\n\
              DeletionDate=2026-09-21T10:30:00\nDeletionDate=2027-01-01T00:00:00\n",
        )
        .unwrap();
        assert_eq!(decoded, b"first");
        assert_eq!(dt, date(2026, 9, 21, 10, 30, 0));
    }

    #[test]
    fn parse_stops_at_the_next_group_header() {
        let err =
            parse(b"[Trash Info]\nPath=x\n[Another Group]\nDeletionDate=2026-09-21T10:30:00\n")
                .unwrap_err();
        assert_eq!(err, "no DeletionDate");
    }

    #[test]
    fn parse_ignores_a_line_without_equals() {
        let (decoded, dt) = parse(
            b"[Trash Info]\nthis line has no equals sign\nPath=x\nDeletionDate=2026-09-21T10:30:00\n",
        )
        .unwrap();
        assert_eq!(decoded, b"x");
        assert_eq!(dt, date(2026, 9, 21, 10, 30, 0));
    }

    #[test]
    fn parse_rejects_missing_header() {
        let err = parse(b"Path=x\nDeletionDate=2026-09-21T10:30:00\n").unwrap_err();
        assert_eq!(err, "first line is not [Trash Info]");
    }

    #[test]
    fn parse_rejects_missing_path() {
        let err = parse(b"[Trash Info]\nDeletionDate=2026-09-21T10:30:00\n").unwrap_err();
        assert_eq!(err, "no Path");
    }

    #[test]
    fn parse_rejects_missing_date() {
        let err = parse(b"[Trash Info]\nPath=x\n").unwrap_err();
        assert_eq!(err, "no DeletionDate");
    }

    #[test]
    fn parse_rejects_bad_date() {
        let err = parse(b"[Trash Info]\nPath=x\nDeletionDate=not-a-date\n").unwrap_err();
        assert_eq!(err, "bad DeletionDate");
    }

    // --- original() ------------------------------------------------------

    #[test]
    fn original_home_relative_resolves_under_base() {
        let base = Path::new("/home/user/.local/share/Trash");
        let got = original(Kind::Home, base, b"rel/a").unwrap();
        assert_eq!(got, base.join("rel/a"));
    }

    #[test]
    fn original_home_absolute_used_as_is_even_outside_base() {
        let base = Path::new("/home/user/.local/share/Trash");
        let got = original(Kind::Home, base, b"/mnt/Mumei/a").unwrap();
        assert_eq!(got, Path::new("/mnt/Mumei/a"));
    }

    #[test]
    fn original_topdir_relative_resolves_under_base() {
        let base = Path::new("/mnt/Mumei");
        let got = original(Kind::User, base, b"a/b").unwrap();
        assert_eq!(got, base.join("a/b"));
    }

    #[test]
    fn original_topdir_absolute_inside_base_accepted() {
        let base = Path::new("/mnt/Mumei");
        let got = original(Kind::Admin, base, b"/mnt/Mumei/a/b").unwrap();
        assert_eq!(got, base.join("a/b"));
    }

    #[test]
    fn original_topdir_absolute_outside_base_rejected() {
        let base = Path::new("/mnt/Mumei");
        let err = original(Kind::User, base, b"/home/user/a").unwrap_err();
        assert_eq!(err, "Path is outside its topdir");
    }

    #[test]
    fn original_topdir_equal_to_base_rejected() {
        let base = Path::new("/mnt/Mumei");
        let err = original(Kind::User, base, b"/mnt/Mumei").unwrap_err();
        assert_eq!(err, "Path is outside its topdir");
    }

    #[test]
    fn original_rejects_dotdot() {
        let base = Path::new("/mnt/Mumei");
        assert_eq!(
            original(Kind::User, base, b"../etc/passwd").unwrap_err(),
            "'..' in Path"
        );
        assert_eq!(
            original(Kind::Home, base, b"/a/../b").unwrap_err(),
            "'..' in Path"
        );
    }

    #[test]
    fn original_rejects_nul_byte() {
        let base = Path::new("/mnt/Mumei");
        let err = original(Kind::Home, base, b"/a\0b").unwrap_err();
        assert_eq!(err, "empty Path or NUL byte");
    }

    #[test]
    fn original_rejects_empty_path() {
        let base = Path::new("/mnt/Mumei");
        let err = original(Kind::Home, base, b"").unwrap_err();
        assert_eq!(err, "empty Path or NUL byte");
    }

    // --- candidate() -------------------------------------------------

    #[test]
    fn candidate_no_suffix_when_k_is_zero() {
        let got = candidate(OsStr::new("file.txt"), 0, 10);
        assert_eq!(got, OsStr::new("file.txt"));
    }

    #[test]
    fn candidate_appends_tilde_k_suffix() {
        let got = candidate(OsStr::new("file.txt"), 3, 10);
        assert_eq!(got, OsStr::new("file.txt~3"));
    }

    #[test]
    fn candidate_cuts_a_255_byte_name_to_fit_with_trashinfo_reserve() {
        let name: String = "a".repeat(255);
        let got = candidate(OsStr::new(&name), 0, INFO_SUFFIX.len());
        assert_eq!(got.len(), 255 - INFO_SUFFIX.len());
        assert_eq!(got, OsStr::new(&"a".repeat(255 - INFO_SUFFIX.len())));
    }

    #[test]
    fn candidate_backs_off_to_a_utf8_char_boundary() {
        // The Euro sign is a 3-byte codepoint; a raw byte cut at the budget
        // would otherwise land inside one.
        let ch = '\u{20ac}';
        let name: String = std::iter::repeat_n(ch, 100).collect(); // 300 bytes
        let reserve = INFO_SUFFIX.len();
        let got = candidate(OsStr::new(&name), 0, reserve);
        let want_bytes = 255 - reserve; // 245, not a multiple of 3
        assert!(got.len() < want_bytes);
        assert!(got.len() % ch.len_utf8() == 0);
        assert!(std::str::from_utf8(got.as_bytes()).is_ok());
    }

    #[test]
    fn candidate_cuts_a_non_utf8_name_on_the_raw_byte_count() {
        // 0xFF is not valid UTF-8 anywhere, so `to_str()` fails and the cut
        // stays at the raw byte budget with no boundary search.
        let name = OsStr::from_bytes(&[0xFFu8; 300]);
        let got = candidate(name, 0, INFO_SUFFIX.len());
        assert_eq!(got.len(), 255 - INFO_SUFFIX.len());
        assert_eq!(
            got.as_bytes(),
            vec![0xFFu8; 255 - INFO_SUFFIX.len()].as_slice()
        );
    }
}
