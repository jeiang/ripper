//! `rip empty`: permanently deletes trash entries, optionally filtered by
//! age and total size. `select_for_empty` is the pure decision (docs/design.md
//! §6); `run` discovers and loads every trash dir, selects candidates,
//! prompts, and hands each trash dir's doomed entries to `trash::delete_batch`
//! (docs/design.md §5.4).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;

use jiff::civil;

use crate::sys::{self, Check};
use crate::trash::{self, Doomed};
use crate::{Cx, confirm, human};

/// A candidate resolved back to its real `Item`/`Orphan`. `trash::Doomed`
/// itself is not `Copy`, so it cannot be read out of a `HashMap` behind a
/// shared borrow (`map[key]`); this small local stand-in is `Copy`, and
/// converts to `Doomed` with `.into()` once a trash dir's final batch is
/// built.
#[derive(Clone, Copy)]
enum Entry<'a> {
    Item(&'a trash::Item),
    Orphan(&'a trash::Orphan),
}

impl Entry<'_> {
    fn date(&self) -> civil::DateTime {
        match self {
            Entry::Item(it) => it.date,
            Entry::Orphan(o) => o.date,
        }
    }
}

impl<'a> From<Entry<'a>> for Doomed<'a> {
    fn from(e: Entry<'a>) -> Doomed<'a> {
        match e {
            Entry::Item(it) => Doomed::Item(it),
            Entry::Orphan(o) => Doomed::Orphan(o),
        }
    }
}

/// One candidate for deletion: an item or an orphan (docs/design.md §4;
/// dangling infos are not candidates -- every `empty` removes them
/// unconditionally, see `run` below). `key` is `(trash index, files/ name)`,
/// which is unique per candidate and lets a caller look the real `Item`/
/// `Orphan` back up after this sorts purely on dates.
pub struct Cand {
    pub date: civil::DateTime,
    pub key: (usize, OsString),
    /// Whether this candidate can merge with an adjacent, equal-date
    /// candidate into one `--max-size` batch: only an item can (its
    /// `DeletionDate` names one `rip` invocation, so every item sharing it
    /// is that same batch). An orphan is dated by its files/ entry's local
    /// ctime, an unrelated clock, so it is always its own single-item
    /// batch even if that ctime happens to equal a neighboring item's
    /// `DeletionDate` (docs/design.md §1).
    pub batchable: bool,
}

/// Indices into `c`, after this sorts it newest-first, to delete. No filter:
/// every candidate. `cutoff`: `date < cutoff`, per candidate. `max`: groups
/// candidates into batches -- a run of adjacent, equal-date `batchable`
/// candidates is one batch, everything else is its own single-item batch --
/// then keeps the newest batches while their running total stays at most
/// `max`, and deletes the first batch that pushes the total over and every
/// older batch whole (never splitting a batch by size). Both filters
/// together delete their union. The newest batch alone over `max` is an
/// error carrying (its total, `max`): nothing is deleted (docs/design.md
/// §1).
pub fn select_for_empty(
    c: &mut [Cand],
    cutoff: Option<civil::DateTime>,
    max: Option<u64>,
    mut size: impl FnMut(&Cand) -> u64,
) -> Result<Vec<usize>, (u64, u64)> {
    // Newest first; ties break on `key` so the order is reproducible however
    // the caller happened to build the candidate list. A `batchable` tie
    // still needs this for a deterministic *within-batch* order; it no
    // longer affects which batch a candidate lands in.
    c.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.key.cmp(&b.key)));

    let mut doomed = vec![cutoff.is_none() && max.is_none(); c.len()];
    if let Some(cut) = cutoff {
        for (d, e) in doomed.iter_mut().zip(c.iter()) {
            *d |= e.date < cut;
        }
    }
    if let Some(max) = max {
        let mut total = 0u64;
        let mut i = 0usize;
        while i < c.len() {
            // One batch: `c[start]` plus every following candidate that
            // shares its date and is batchable too. Sizes only as far as
            // this needs -- never past the batch that overflows `max`.
            let start = i;
            let mut batch = 0u64;
            loop {
                batch = batch.saturating_add(size(&c[i]));
                i += 1;
                let joins_batch = i < c.len()
                    && c[start].batchable
                    && c[i].batchable
                    && c[i].date == c[start].date;
                if !joins_batch {
                    break;
                }
            }
            total = total.saturating_add(batch);
            if total > max {
                if start == 0 {
                    return Err((batch, max));
                }
                doomed[start..].iter_mut().for_each(|d| *d = true);
                break;
            }
        }
    }
    Ok((0..c.len()).filter(|&i| doomed[i]).collect())
}

/// `files/NAME`'s apparent size, computed lazily by walking just that one
/// entry (docs/design.md §1: never the whole trash). A walk error (the
/// entry raced away, an unreadable subdirectory, ...) contributes 0 rather
/// than aborting `empty` over a size estimate; the actual deletion recheck
/// happens later, inside `delete_batch` itself.
fn entry_size(t: &trash::Trash, name: &OsStr) -> u64 {
    sys::walk(t.files.as_fd(), name, Check::Size)
        .map(|w| w.size)
        .unwrap_or(0)
}

/// A `size` closure for `select_for_empty` that memoizes by `Cand::key` in
/// `cache`, shared with the prompt's own size total afterward so an entry is
/// never walked twice in one `empty` invocation.
fn cached_size<'c>(
    trashes: &'c [trash::Trash],
    cache: &'c mut HashMap<(usize, OsString), u64>,
) -> impl FnMut(&Cand) -> u64 + 'c {
    move |c: &Cand| {
        if let Some(&s) = cache.get(&c.key) {
            return s;
        }
        let s = entry_size(&trashes[c.key.0], &c.key.1);
        cache.insert(c.key.clone(), s);
        s
    }
}

pub fn run(
    cx: &Cx,
    older_than: Option<jiff::Span>,
    max_size: Option<u64>,
    yes: bool,
) -> Result<bool, String> {
    let (trashes, contents) = trash::load_all(&cx.mounts, cx.uid)?;
    for w in &contents.warnings {
        eprintln!("rip: {w}");
    }

    let cutoff = match older_than {
        Some(span) => Some(
            jiff::Zoned::now()
                .checked_sub(span)
                .map(|z| z.datetime())
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };

    // Items and orphans are candidates; each is keyed by (trash index,
    // name), which `lookup` maps back to the real entry after selection.
    let mut cands: Vec<Cand> = Vec::with_capacity(contents.items.len() + contents.orphans.len());
    let mut lookup: HashMap<(usize, OsString), Entry<'_>> = HashMap::new();
    for it in &contents.items {
        let key = (it.trash, it.name.clone());
        cands.push(Cand {
            date: it.date,
            key: key.clone(),
            batchable: true,
        });
        lookup.insert(key, Entry::Item(it));
    }
    for o in &contents.orphans {
        let key = (o.trash, o.name.clone());
        cands.push(Cand {
            date: o.date,
            key: key.clone(),
            batchable: false,
        });
        lookup.insert(key, Entry::Orphan(o));
    }

    let mut size_cache: HashMap<(usize, OsString), u64> = HashMap::new();
    let doomed_idx = match select_for_empty(
        &mut cands,
        cutoff,
        max_size,
        cached_size(&trashes, &mut size_cache),
    ) {
        Ok(idx) => idx,
        Err((have, max)) => {
            return Err(format!(
                "the newest batch alone is {} but --max-size is {}; nothing was deleted",
                human(have),
                human(max)
            ));
        }
    };

    // Group the selected items/orphans by trash dir, oldest first within
    // each; count for the prompt.
    let mut by_trash: Vec<Vec<Entry<'_>>> = trashes.iter().map(|_| Vec::new()).collect();
    let mut touched: HashSet<usize> = HashSet::new();
    let mut n_items = 0u64;
    let mut n_orphans = 0u64;
    for &i in &doomed_idx {
        let key = &cands[i].key;
        let e = lookup[key];
        match e {
            Entry::Item(_) => n_items += 1,
            Entry::Orphan(_) => n_orphans += 1,
        }
        touched.insert(key.0);
        by_trash[key.0].push(e);
    }
    for group in &mut by_trash {
        group.sort_by_key(Entry::date);
    }

    let selected = n_items + n_orphans;
    if selected > 0 && !yes {
        let mut question = format!(
            "permanently delete {n_items} items and {n_orphans} orphans from {} trash directories",
            touched.len()
        );
        // A size is shown only when every doomed entry's size is already
        // known from selection's own lazy walk (docs/design.md §1: it
        // sizes only as far as `--max-size` needs, up to the batch that
        // overflows). Walking the rest here just to fill in the prompt
        // would measure the whole older part of the trash before asking
        // anything -- so if even one doomed entry was never sized, the
        // size is left out instead of computed.
        if max_size.is_some() {
            let mut total = 0u64;
            let mut known = true;
            for &i in &doomed_idx {
                match size_cache.get(&cands[i].key) {
                    Some(&s) => total = total.saturating_add(s),
                    None => {
                        known = false;
                        break;
                    }
                }
            }
            if known {
                question.push_str(&format!(" ({})", human(total)));
            }
        }
        question.push('?');
        match confirm(&question, "-y") {
            Ok(true) => {}
            Ok(false) => return Ok(true),
            // confirm()'s own message names the question; empty's refusal is
            // this fixed line instead (docs/design.md §0 invariant 7).
            Err(_) if !io::stdin().is_terminal() => {
                return Err("refusing to delete without confirmation (use -y)".into());
            }
            Err(e) => return Err(e),
        }
    }

    // Every trash dir found gets a delete_batch call: its selected items and
    // orphans (possibly none), plus its dangling infos (always, docs/design.md
    // §4), plus staging cleanup (docs/design.md §5.4).
    let mut dangling_by_trash: Vec<Vec<&trash::Dangling>> =
        trashes.iter().map(|_| Vec::new()).collect();
    for g in &contents.dangling {
        dangling_by_trash[g.trash].push(g);
    }

    let mut deleted = 0u64;
    let mut ok = true;
    for (idx, t) in trashes.iter().enumerate() {
        let mut doomed: Vec<Doomed> = std::mem::take(&mut by_trash[idx])
            .into_iter()
            .map(Into::into)
            .collect();
        doomed.extend(dangling_by_trash[idx].iter().copied().map(Doomed::Dangling));
        let report = trash::delete_batch(t, &cx.mounts, &doomed, true);
        deleted += report.deleted;
        for (path, why) in &report.kept {
            eprintln!(
                "rip: could not delete {}: {why}",
                crate::escape(path.as_os_str().as_bytes())
            );
            ok = false;
        }
    }
    eprintln!("rip: deleted {deleted} items");

    Ok(ok)
}

// ---------------------------------------------------------------------------
// Tests (docs/design.md §6: select_for_empty table)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(trash: usize, name: &str, date: &str) -> Cand {
        Cand {
            date: date.parse().unwrap(),
            key: (trash, OsString::from(name)),
            batchable: true,
        }
    }

    fn orphan_cand(trash: usize, name: &str, date: &str) -> Cand {
        Cand {
            batchable: false,
            ..cand(trash, name, date)
        }
    }

    fn names(c: &[Cand], idx: &[usize]) -> Vec<String> {
        idx.iter()
            .map(|&i| c[i].key.1.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn no_filter_deletes_all() {
        let mut c = vec![
            cand(0, "a", "2026-01-02T00:00:00"),
            cand(0, "b", "2026-01-01T00:00:00"),
        ];
        let got = select_for_empty(&mut c, None, None, |_| 0).unwrap();
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn older_than_boundary_equal_is_kept() {
        let mut c = vec![
            cand(0, "new", "2026-01-03T00:00:00"),
            cand(0, "boundary", "2026-01-02T00:00:00"),
            cand(0, "old", "2026-01-01T00:00:00"),
        ];
        let cutoff: civil::DateTime = "2026-01-02T00:00:00".parse().unwrap();
        let got = select_for_empty(&mut c, Some(cutoff), None, |_| 0).unwrap();
        assert_eq!(names(&c, &got), vec!["old"]);
    }

    #[test]
    fn max_size_exact_fit_keeps_everything() {
        let mut c = vec![
            cand(0, "a", "2026-01-03T00:00:00"),
            cand(0, "b", "2026-01-02T00:00:00"),
            cand(0, "c", "2026-01-01T00:00:00"),
        ];
        let got = select_for_empty(&mut c, None, Some(30), |_| 10).unwrap();
        assert!(got.is_empty(), "{}", names(&c, &got).join(","));
    }

    #[test]
    fn max_size_first_overflow_deletes_every_older_item_even_a_smaller_one() {
        let mut c = vec![
            cand(0, "newest", "2026-01-03T00:00:00"),    // 20
            cand(0, "mid", "2026-01-02T00:00:00"),       // 10: total 30 > 25, overflows here
            cand(0, "old_small", "2026-01-01T00:00:00"), // 1: smaller, still doomed by cascade
        ];
        let sizes: HashMap<(usize, OsString), u64> = [
            ((0, OsString::from("newest")), 20u64),
            ((0, OsString::from("mid")), 10),
            ((0, OsString::from("old_small")), 1),
        ]
        .into_iter()
        .collect();
        let got = select_for_empty(&mut c, None, Some(25), |cd| sizes[&cd.key]).unwrap();
        assert_eq!(names(&c, &got), vec!["mid", "old_small"]);
    }

    #[test]
    fn max_size_newest_too_big_errors() {
        let mut c = vec![cand(0, "huge", "2026-01-01T00:00:00")];
        let err = select_for_empty(&mut c, None, Some(50), |_| 100).unwrap_err();
        assert_eq!(err, (100, 50));
    }

    #[test]
    fn union_of_both_filters() {
        // "a" is kept by both filters. "b" is old enough to be doomed by age
        // alone, but its size alone would fit under `max` (max only starts
        // dooming from "c" onward): the combined run must still catch "b".
        let mut c = vec![
            cand(0, "a", "2026-01-07T00:00:00"), // size 1
            cand(0, "b", "2026-01-03T00:00:00"), // size 1, older than the age cutoff
            cand(0, "c", "2026-01-02T00:00:00"), // size 100: overflows max here
            cand(0, "d", "2026-01-01T00:00:00"), // size 1, doomed by max's cascade too
        ];
        let sizes: HashMap<(usize, OsString), u64> = [
            ((0, OsString::from("a")), 1u64),
            ((0, OsString::from("b")), 1),
            ((0, OsString::from("c")), 100),
            ((0, OsString::from("d")), 1),
        ]
        .into_iter()
        .collect();
        let cutoff: civil::DateTime = "2026-01-04T00:00:00".parse().unwrap();
        let got = select_for_empty(&mut c, Some(cutoff), Some(50), |cd| sizes[&cd.key]).unwrap();
        assert_eq!(names(&c, &got), vec!["b", "c", "d"]);
    }

    #[test]
    fn tie_order_is_deterministic() {
        let mut c = vec![
            cand(1, "a", "2026-01-01T00:00:00"),
            cand(0, "b", "2026-01-01T00:00:00"),
            cand(0, "a", "2026-01-01T00:00:00"),
        ];
        let got = select_for_empty(&mut c, None, None, |_| 0).unwrap();
        assert_eq!(got, vec![0, 1, 2]);
        let order: Vec<(usize, String)> = c
            .iter()
            .map(|cd| (cd.key.0, cd.key.1.to_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            order,
            vec![
                (0, "a".to_string()),
                (0, "b".to_string()),
                (1, "a".to_string()),
            ]
        );
    }

    // ---- --max-size batching ----

    /// Two items sharing one `DeletionDate` -- one `rip` invocation -- are
    /// the newest batch. Its total alone is over `max`, so this must error
    /// and delete nothing, regardless of which of the two sorts first by
    /// name: it must not delete the larger one just because a smaller
    /// sibling happens to sort ahead of it by name.
    #[test]
    fn max_size_newest_batch_over_max_errors_whichever_name_sorts_first() {
        for small in ["a_notes", "z_notes"] {
            let mut c = vec![
                cand(0, "movie.mkv", "2026-09-01T12:00:00"), // size 8
                cand(0, small, "2026-09-01T12:00:00"),       // size 1
                cand(0, "older", "2026-08-01T12:00:00"),     // size 1
            ];
            let size = |cd: &Cand| {
                if cd.key.1 == OsStr::new("movie.mkv") {
                    8
                } else {
                    1
                }
            };
            let err = select_for_empty(&mut c, None, Some(4), size).unwrap_err();
            assert_eq!(err, (9, 4), "small={small}");
        }
    }

    /// An older (non-newest) batch of two same-`DeletionDate` items must be
    /// deleted or kept as a whole, never split so that only the one that
    /// sorts first (or last) by name goes.
    #[test]
    fn max_size_does_not_split_an_older_batch_by_name() {
        for (first, second) in [("b_small", "c_big"), ("b_big", "c_small")] {
            let mut c = vec![
                cand(0, "newest", "2026-01-03T00:00:00"), // size 1, always kept
                cand(0, first, "2026-01-01T00:00:00"),
                cand(0, second, "2026-01-01T00:00:00"),
            ];
            let sizes: HashMap<(usize, OsString), u64> = [
                ((0, OsString::from("newest")), 1u64),
                ((0, OsString::from(first)), 1),
                ((0, OsString::from(second)), 3),
            ]
            .into_iter()
            .collect();
            // total if the whole older batch is kept too: 1+1+3 = 5 > 2.
            let got = select_for_empty(&mut c, None, Some(2), |cd| sizes[&cd.key]).unwrap();
            let doomed = names(&c, &got);
            assert_eq!(
                doomed.len(),
                2,
                "batch split for ({first}, {second}): {doomed:?}"
            );
            assert!(doomed.contains(&first.to_string()) && doomed.contains(&second.to_string()));
        }
    }

    /// An orphan never joins a batch, even when its (ctime-derived) date
    /// exactly equals a neighboring item's `DeletionDate`: it is always its
    /// own single-item batch (docs/design.md §1).
    #[test]
    fn max_size_orphan_with_same_date_as_an_item_does_not_join_its_batch() {
        let mut c = vec![
            cand(0, "item", "2026-01-01T00:00:00"),
            orphan_cand(0, "orphan", "2026-01-01T00:00:00"),
        ];
        let sizes: HashMap<(usize, OsString), u64> = [
            ((0, OsString::from("item")), 3u64),
            ((0, OsString::from("orphan")), 100),
        ]
        .into_iter()
        .collect();
        // If they merged into one batch, the total (103) would be over max
        // (3) with start == 0, so this would error and delete nothing. As
        // separate batches, "item" alone (3) fits and "orphan" alone (100)
        // overflows on its own.
        let got = select_for_empty(&mut c, None, Some(3), |cd| sizes[&cd.key]).unwrap();
        assert_eq!(names(&c, &got), vec!["orphan"]);
    }
}
