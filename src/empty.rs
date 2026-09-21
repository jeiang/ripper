//! `rip empty`: permanently deletes trash entries, optionally filtered by
//! age and total size. `select_for_empty` is the pure decision (docs/design.md
//! §8.1); `run` discovers and loads every trash dir, selects candidates,
//! prompts, and hands each trash dir's doomed entries to `trash::delete_batch`
//! (docs/design.md §8.2, §8.3).

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

/// One candidate for deletion: an item or an orphan (docs/design.md §8.1;
/// dangling infos are not candidates -- every `empty` removes them
/// unconditionally, see `run` below). `key` is `(trash index, files/ name)`,
/// which is unique per candidate and lets a caller look the real `Item`/
/// `Orphan` back up after this sorts purely on dates.
pub struct Cand {
    pub date: civil::DateTime,
    pub key: (usize, OsString),
}

/// Indices into `c`, after this sorts it newest-first, to delete. No filter:
/// every candidate. `cutoff`: `date < cutoff`. `max`: keep the newest
/// candidates while their running total stays at most `max`, then delete the
/// first one that pushes the total over and every older one (even a smaller
/// one). Both filters together delete their union. The newest candidate
/// alone over `max` is an error carrying (its size, `max`): nothing is
/// deleted (docs/design.md §8.1, the brief's `--max-size`).
pub fn select_for_empty(
    c: &mut [Cand],
    cutoff: Option<civil::DateTime>,
    max: Option<u64>,
    mut size: impl FnMut(&Cand) -> u64,
) -> Result<Vec<usize>, (u64, u64)> {
    // Newest first; ties break on `key` so the order is reproducible however
    // the caller happened to build the candidate list.
    c.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.key.cmp(&b.key)));

    let mut doomed = vec![cutoff.is_none() && max.is_none(); c.len()];
    if let Some(cut) = cutoff {
        for (d, e) in doomed.iter_mut().zip(c.iter()) {
            *d |= e.date < cut;
        }
    }
    if let Some(max) = max {
        let mut total = 0u64;
        let mut first = None;
        let overflow = c.iter().position(|e| {
            let s = size(e);
            first.get_or_insert(s);
            total = total.saturating_add(s);
            total > max
        });
        if let Some(i) = overflow {
            if i == 0 {
                return Err((first.unwrap(), max));
            }
            doomed[i..].iter_mut().for_each(|d| *d = true);
        }
    }
    Ok((0..c.len()).filter(|&i| doomed[i]).collect())
}

/// `files/NAME`'s apparent size, computed lazily by walking just that one
/// entry (docs/design.md §8.1: never the whole trash). A walk error (the
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
    // name), which `lookup` maps back to the real entry after selection
    // (docs/design.md §8.1).
    let mut cands: Vec<Cand> = Vec::with_capacity(contents.items.len() + contents.orphans.len());
    let mut lookup: HashMap<(usize, OsString), Entry<'_>> = HashMap::new();
    for it in &contents.items {
        let key = (it.trash, it.name.clone());
        cands.push(Cand {
            date: it.date,
            key: key.clone(),
        });
        lookup.insert(key, Entry::Item(it));
    }
    for o in &contents.orphans {
        let key = (o.trash, o.name.clone());
        cands.push(Cand {
            date: o.date,
            key: key.clone(),
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
                "the newest item alone is {} but --max-size is {}; nothing was deleted",
                human(have),
                human(max)
            ));
        }
    };

    // Group the selected items/orphans by trash dir, oldest first within
    // each (docs/design.md §8.3 "Empty flow"); count for the prompt.
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
        // A size is shown only when `--max-size` already made rip compute
        // one; under `--older-than` alone (or no filter) summing sizes here
        // would walk entries nobody asked to have measured.
        if max_size.is_some() {
            let total: u64 = doomed_idx
                .iter()
                .map(|&i| {
                    let key = &cands[i].key;
                    if let Some(&s) = size_cache.get(key) {
                        s
                    } else {
                        let s = entry_size(&trashes[key.0], &key.1);
                        size_cache.insert(key.clone(), s);
                        s
                    }
                })
                .sum();
            question.push_str(&format!(" ({})", human(total)));
        }
        question.push('?');
        match confirm(&question, "-y") {
            Ok(true) => {}
            Ok(false) => return Ok(true),
            // confirm()'s own message names the question; empty's refusal is
            // this fixed line instead (docs/design.md §8.2).
            Err(_) if !io::stdin().is_terminal() => {
                return Err("refusing to delete without confirmation (use -y)".into());
            }
            Err(e) => return Err(e),
        }
    }

    // Every trash dir found gets a delete_batch call: its selected items and
    // orphans (possibly none), plus its dangling infos (always -- brief item
    // 9), plus staging cleanup (docs/design.md §8.3 "Empty flow").
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
// Tests (docs/design.md §13.1 "empty.rs / restore.rs": select_for_empty)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(trash: usize, name: &str, date: &str) -> Cand {
        Cand {
            date: date.parse().unwrap(),
            key: (trash, OsString::from(name)),
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
}
