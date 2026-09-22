//! `/proc/self/mountinfo` parsing and path translation between mounts of the
//! same filesystem (docs/design.md §5.1-5.3). Pure: nothing here touches the
//! filesystem except `Mounts::read`, which just reads one file.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FsId(pub u32, pub u32);

#[derive(Debug)]
pub struct Mount {
    pub id: u64,
    pub parent: u64,
    pub fs: FsId,
    /// The mount's root inside its filesystem (btrfs: from the top-level subvolume).
    pub root: PathBuf,
    /// The mount point in this namespace.
    pub point: PathBuf,
    /// The `ro` option in mountinfo field 6 (per-mount, independent of
    /// whether the underlying filesystem is itself writable: a `ro` bind
    /// mount of an otherwise-writable subvolume reads this way too).
    pub ro: bool,
    #[allow(dead_code)] // first read by trash::discover's autofs skip (C3, design §0.1 #12)
    pub fstype: OsString,
    /// Where the mount point lies: the parent's filesystem and the path inside it.
    #[allow(dead_code)] // first read by mount_conflict's containment check, called from C3/C4a
    pub under: Option<(FsId, PathBuf)>,
}

#[derive(Debug, Default)]
pub struct Mounts(Vec<Mount>);

impl Mounts {
    pub fn read() -> io::Result<Self> {
        let text = std::fs::read("/proc/self/mountinfo")?;
        Ok(Self::parse(&text))
    }

    /// Keeps the lines that parse and skips malformed ones, then fills in
    /// `under` for each mount: `(parent.fs, inside(parent, &m.point))`.
    /// Nothing here relies on the order of mountinfo lines.
    pub fn parse(text: &[u8]) -> Self {
        let mut mounts: Vec<Mount> = text
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(parse_line)
            .collect();

        let by_id: HashMap<u64, usize> =
            mounts.iter().enumerate().map(|(i, m)| (m.id, i)).collect();
        let under: Vec<Option<(FsId, PathBuf)>> = mounts
            .iter()
            .map(|m| -> Option<(FsId, PathBuf)> {
                let &i = by_id.get(&m.parent)?;
                let parent = &mounts[i];
                Some((parent.fs, inside(parent, &m.point)?))
            })
            .collect();
        for (m, u) in mounts.iter_mut().zip(under) {
            m.under = u;
        }
        Mounts(mounts)
    }

    #[allow(dead_code)] // first called by sys::route (C2b)
    pub fn by_id(&self, id: u64) -> Option<&Mount> {
        self.0.iter().find(|m| m.id == id)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Mount> {
        self.0.iter()
    }
}

/// No trailing slash, no repeated separators, no interior `.` component.
fn clean(p: PathBuf) -> PathBuf {
    p.components().collect()
}

/// `path` (under mount `m`) as a path inside m's filesystem.
pub fn inside(m: &Mount, path: &Path) -> Option<PathBuf> {
    Some(clean(m.root.join(path.strip_prefix(&m.point).ok()?)))
}

/// `path` (under mount `own`) as seen through `via`, another mount of the
/// same filesystem, if `via` shows it.
#[allow(dead_code)] // first called by route_candidates, itself first called from sys::route (C2b)
pub fn through(own: &Mount, via: &Mount, path: &Path) -> Option<PathBuf> {
    Some(clean(
        via.point
            .join(inside(own, path)?.strip_prefix(&via.root).ok()?),
    ))
}

/// Mounts that may show both `a` (in mount `am`) and `b` (in mount `bm`),
/// with the translated paths. The candidates start with `am` itself.
#[allow(dead_code)] // first called by sys::route (C2b)
pub fn route_candidates(
    ms: &Mounts,
    am: u64,
    a: &Path,
    bm: u64,
    b: &Path,
) -> Vec<(u64, PathBuf, PathBuf)> {
    let (Some(fa), Some(fb)) = (ms.by_id(am), ms.by_id(bm)) else {
        return vec![];
    };
    if fa.fs != fb.fs {
        return vec![];
    }
    std::iter::once(fa)
        .chain(ms.iter().filter(|c| c.fs == fa.fs && c.id != fa.id))
        .filter_map(|c| Some((c.id, through(fa, c, a)?, through(fb, c, b)?)))
        .collect()
}

/// Why `path` (reached through its own mount `own`) must not be trashed, if
/// there is a reason. Compares filesystem paths, so it also works through
/// alias views such as `/persist` and `/mnt/root`.
#[allow(dead_code)] // first called by put's refusal checks and trash::delete_batch (C3/C4a)
pub fn mount_conflict(
    ms: &Mounts,
    own: &Mount,
    path: &Path,
    is_mount_root: bool,
) -> Option<String> {
    if is_mount_root || path == own.point {
        return Some("is a mount point".into());
    }
    let p = inside(own, path)?;
    if let Some(m) = ms
        .iter()
        .find(|m| m.fs == own.fs && m.id != own.id && m.root.starts_with(&p))
    {
        return Some(format!("is mounted at {}", m.point.display())); // a bind source
    }
    ms.iter()
        .find(|m| {
            m.under
                .as_ref()
                .is_some_and(|(fs, at)| *fs == own.fs && at.starts_with(&p))
        })
        .map(|m| format!("contains the mount point {}", m.point.display()))
}

fn num(b: &[u8]) -> Option<u64> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

fn bytes_path(b: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(b))
}

/// `id parent maj:min root point opts [optional...] - fstype src superopts`
fn parse_line(l: &[u8]) -> Option<Mount> {
    let f: Vec<&[u8]> = l.split(|&b| b == b' ').collect();
    let sep = f.iter().position(|x| *x == b"-").filter(|&s| s >= 6)?;
    let (maj, min) = std::str::from_utf8(f[2]).ok()?.split_once(':')?;
    Some(Mount {
        id: num(f[0])?,
        parent: num(f[1])?,
        fs: FsId(maj.parse().ok()?, min.parse().ok()?),
        root: bytes_path(unescape(f[3])),
        point: bytes_path(unescape(f[4])),
        ro: f[5].split(|&b| b == b',').any(|o| o == b"ro"),
        fstype: OsString::from_vec(unescape(f.get(sep + 1)?)),
        under: None,
    })
}

/// The kernel writes space, tab, newline and backslash as `\NNN` (octal).
fn unescape(s: &[u8]) -> Vec<u8> {
    let (mut out, mut i) = (Vec::with_capacity(s.len()), 0);
    while i < s.len() {
        match s.get(i + 1..i + 4) {
            Some(d) if s[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)) => {
                out.push(d.iter().fold(0u16, |a, c| a * 8 + u16::from(c - b'0')) as u8);
                i += 4;
            }
            _ => {
                out.push(s[i]);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `tests/fixtures/mountinfo-artemis.txt`: a real `/proc/self/mountinfo`
    /// capture from artemis (NixOS, btrfs + impermanence bind mounts), with
    /// the user name replaced by `user`. It includes `/mnt/root` (the btrfs
    /// top level) and the home bind mounts (`~/Downloads`,
    /// `~/.local/share/Trash`, ...).
    fn fixture() -> Mounts {
        Mounts::parse(include_bytes!("../tests/fixtures/mountinfo-artemis.txt"))
    }

    fn find<'a>(ms: &'a Mounts, point: &str) -> &'a Mount {
        ms.iter()
            .find(|m| m.point == Path::new(point))
            .unwrap_or_else(|| panic!("no mount at {point} in the fixture"))
    }

    // ---- unescape ----

    #[test]
    fn unescape_octal_escapes() {
        assert_eq!(unescape(b"a\\040b"), b"a b");
        assert_eq!(unescape(b"a\\011b"), b"a\tb");
        assert_eq!(unescape(b"a\\012b"), b"a\nb");
        assert_eq!(unescape(b"a\\134b"), b"a\\b");
        assert_eq!(unescape(b"\\040\\011\\012\\134"), b" \t\n\\");
    }

    #[test]
    fn unescape_passthrough_and_incomplete_escape() {
        assert_eq!(unescape(b"plain"), b"plain");
        // Not three octal digits: the backslash stays literal.
        assert_eq!(unescape(b"a\\bc"), b"a\\bc");
        // A backslash with fewer than three bytes left in the input.
        assert_eq!(unescape(b"trailing\\"), b"trailing\\");
        assert_eq!(unescape(b"trailing\\04"), b"trailing\\04");
    }

    // ---- parse_line: optional fields and malformed lines ----

    #[test]
    fn parse_line_accepts_zero_one_and_two_optional_fields() {
        let zero = parse_line(b"20 1 0:1 / /mnt rw - ext4 /dev/sda1 rw").unwrap();
        assert_eq!(zero.id, 20);
        assert_eq!(zero.fstype, OsString::from("ext4"));

        let one = parse_line(b"20 1 0:1 / /mnt rw shared:1 - ext4 /dev/sda1 rw").unwrap();
        assert_eq!(one.fstype, OsString::from("ext4"));

        let two = parse_line(b"20 1 0:1 / /mnt rw shared:1 master:2 - ext4 /dev/sda1 rw").unwrap();
        assert_eq!(two.fstype, OsString::from("ext4"));
    }

    #[test]
    fn parse_line_captures_the_ro_option() {
        let rw = parse_line(b"20 1 0:1 / /mnt rw,nosuid - ext4 /dev/sda1 rw").unwrap();
        assert!(!rw.ro);
        let ro = parse_line(b"20 1 0:1 / /mnt ro,nosuid,nodev - ext4 /dev/sda1 rw").unwrap();
        assert!(ro.ro);
    }

    #[test]
    fn parse_skips_malformed_lines_keeps_good_ones() {
        let text: &[u8] = b"\
            20 1 0:1 / /mnt rw - ext4 /dev/sda1 rw\n\
            not a valid line at all\n\
            21 1 0X1 / /bad rw - ext4 /dev/sda1 rw\n\
            22 1 0:2 / - ext4 /dev/sda1 rw\n\
            abc 1 0:5 / /x rw - ext4 /dev/sda1 rw\n\
            23 1 0:3 / /good2 rw shared:1 - btrfs /dev/x rw\n";
        let ms = Mounts::parse(text);
        assert_eq!(ms.iter().count(), 2, "{:?}", ms.iter().collect::<Vec<_>>());
        assert!(ms.by_id(20).is_some());
        assert!(ms.by_id(21).is_none(), "bad maj:min should be skipped");
        assert!(
            ms.by_id(22).is_none(),
            "too few fields before '-' should be skipped"
        );
        assert!(ms.by_id(23).is_some());
    }

    // ---- the artemis fixture ----

    #[test]
    fn fixture_parses_every_line() {
        let ms = fixture();
        assert_eq!(ms.iter().count(), 92);
    }

    #[test]
    fn fixture_includes_mnt_root_btrfs_top_level() {
        let ms = fixture();
        let mnt_root = find(&ms, "/mnt/root");
        assert_eq!(mnt_root.root, PathBuf::from("/"));
        assert_eq!(mnt_root.fstype, OsString::from("btrfs"));
    }

    #[test]
    fn fixture_includes_home_bind_mounts() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        assert_eq!(
            downloads.root,
            PathBuf::from("/persist/data/home/user/Downloads")
        );
        let trash = find(&ms, "/home/user/.local/share/Trash");
        assert_eq!(
            trash.root,
            PathBuf::from("/persist/data/home/user/.local/share/Trash")
        );
    }

    #[test]
    fn under_is_filled_from_the_parent_mount() {
        let ms = fixture();
        let persist = find(&ms, "/persist");
        let root = find(&ms, "/");
        let (fs, at) = persist
            .under
            .clone()
            .expect("/persist should have a parent under /");
        assert_eq!(fs, root.fs);
        assert_eq!(at, PathBuf::from("/rootfs/persist"));
    }

    #[test]
    fn root_mount_has_no_under() {
        // mountinfo id 1 (the root's parent) is outside this mount namespace,
        // so it never appears as a mount to fill `under` from.
        let ms = fixture();
        let root = find(&ms, "/");
        assert!(ms.by_id(root.parent).is_none());
        assert!(root.under.is_none());
    }

    // ---- inside / through: the Downloads -> Trash case through /persist ----

    #[test]
    fn inside_and_through_translate_downloads_to_trash_via_persist() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        let trash = find(&ms, "/home/user/.local/share/Trash");
        let persist = find(&ms, "/persist");

        let src = Path::new("/home/user/Downloads/x");
        assert_eq!(
            inside(downloads, src).unwrap(),
            PathBuf::from("/persist/data/home/user/Downloads/x")
        );
        assert_eq!(
            through(downloads, persist, src).unwrap(),
            PathBuf::from("/persist/data/home/user/Downloads/x")
        );

        let dst = trash.point.join("files");
        assert_eq!(
            through(trash, persist, &dst).unwrap(),
            PathBuf::from("/persist/data/home/user/.local/share/Trash/files")
        );
    }

    // ---- route_candidates ----

    #[test]
    fn route_candidates_persist_and_mnt_root_translate_no_rootfs_candidate() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        let trash = find(&ms, "/home/user/.local/share/Trash");
        let persist = find(&ms, "/persist");
        let mnt_root = find(&ms, "/mnt/root");
        let root = find(&ms, "/");

        let a = Path::new("/home/user/Downloads/x");
        let b = trash.point.join("files");
        let candidates = route_candidates(&ms, downloads.id, a, trash.id, &b);

        let ids: Vec<u64> = candidates.iter().map(|(id, _, _)| *id).collect();
        assert!(ids.contains(&persist.id), "{candidates:?}");
        assert!(ids.contains(&mnt_root.id), "{candidates:?}");
        assert!(
            !ids.contains(&root.id),
            "no candidate should route /rootfs -> /persist files: {candidates:?}"
        );

        let (_, pa, pb) = candidates
            .iter()
            .find(|(id, _, _)| *id == persist.id)
            .unwrap();
        assert_eq!(pa, &PathBuf::from("/persist/data/home/user/Downloads/x"));
        assert_eq!(
            pb,
            &PathBuf::from("/persist/data/home/user/.local/share/Trash/files")
        );
    }

    #[test]
    fn route_candidates_empty_across_different_filesystems() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        let mumei = find(&ms, "/mnt/Mumei");
        let candidates = route_candidates(
            &ms,
            downloads.id,
            Path::new("/home/user/Downloads/x"),
            mumei.id,
            Path::new("/mnt/Mumei/x"),
        );
        assert!(candidates.is_empty(), "{candidates:?}");
    }

    #[test]
    fn route_candidates_empty_for_unknown_mount_id() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        let candidates = route_candidates(
            &ms,
            downloads.id,
            Path::new("/home/user/Downloads/x"),
            999_999,
            Path::new("/nowhere"),
        );
        assert!(candidates.is_empty(), "{candidates:?}");
    }

    // ---- mount_conflict ----

    #[test]
    fn mount_conflict_downloads_is_a_mount_point() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        let point = downloads.point.clone();
        assert_eq!(
            mount_conflict(&ms, downloads, &point, false).unwrap(),
            "is a mount point"
        );
    }

    #[test]
    fn mount_conflict_is_mount_root_flag_forces_a_conflict() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        // Even a path that is not the mount point itself is refused when the
        // caller already knows (via statx) that it is a mount root.
        let msg =
            mount_conflict(&ms, downloads, Path::new("/home/user/Downloads/x"), true).unwrap();
        assert_eq!(msg, "is a mount point");
    }

    #[test]
    fn mount_conflict_home_contains_mount_points() {
        let ms = fixture();
        let root = find(&ms, "/");
        let msg = mount_conflict(&ms, root, Path::new("/home/user"), false).unwrap();
        assert!(msg.starts_with("contains the mount point "), "{msg}");
    }

    #[test]
    fn mount_conflict_persist_downloads_is_mounted_at_home_downloads() {
        let ms = fixture();
        let persist = find(&ms, "/persist");
        let msg = mount_conflict(
            &ms,
            persist,
            Path::new("/persist/data/home/user/Downloads"),
            false,
        )
        .unwrap();
        assert_eq!(msg, "is mounted at /home/user/Downloads");
    }

    #[test]
    fn mount_conflict_through_mnt_root_alias_still_finds_the_conflict() {
        // Compares filesystem paths, so the same real location reached
        // through the /mnt/root alias view is still caught.
        let ms = fixture();
        let mnt_root = find(&ms, "/mnt/root");
        let msg = mount_conflict(
            &ms,
            mnt_root,
            Path::new("/mnt/root/persist/data/home/user"),
            false,
        );
        assert!(
            msg.is_some(),
            "the /mnt/root alias view should still see the conflict"
        );
    }

    #[test]
    fn mount_conflict_mnt_root_rootfs_is_mounted_at_root() {
        let ms = fixture();
        let mnt_root = find(&ms, "/mnt/root");
        let msg = mount_conflict(&ms, mnt_root, Path::new("/mnt/root/rootfs"), false).unwrap();
        assert_eq!(msg, "is mounted at /");
    }

    #[test]
    fn mount_conflict_downloads_x_has_no_conflict() {
        let ms = fixture();
        let downloads = find(&ms, "/home/user/Downloads");
        let msg = mount_conflict(&ms, downloads, Path::new("/home/user/Downloads/x"), false);
        assert!(msg.is_none(), "{msg:?}");
    }
}
