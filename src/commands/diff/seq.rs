//! Ordered sequence diff for `diff -v` details.
//!
//! Element equality (`crate::content`) compares way refs, relation members
//! and tags in stored order with multiplicity, so the verbose detail under a
//! modified element has to account for order and duplicates too, or a
//! reorder prints a bare `*` line or an uninformative count. This computes
//! an edit script between two sequences: the old positions removed and the
//! new positions inserted. Positions address the original old list and the
//! final new list, not a list mutated as the script is applied: removing the
//! marked old items and placing the inserted items at the marked new
//! positions, with the kept items filling the rest in order, yields the new
//! list.
//!
//! The script explains the difference; it is not a move log. A moved item
//! shows as one removal and one insertion of the same value, at its old and
//! new positions, and when several shortest scripts exist (duplicates,
//! swaps) the one chosen is fixed by the recurrence below, not by which item
//! "really" moved.
//!
//! Algorithm: trim the common prefix and suffix, then Myers' greedy O((N+M)D)
//! search with a per-layer snapshot for the traceback. At each diagonal `k`
//! of layer `d` the path comes from `k + 1` (an insertion) when
//! `k == -d || (k != d && V[k-1] < V[k+1])`, otherwise from `k - 1` (a
//! removal); the traceback applies the same rule. The search counts its work
//! (diagonal visits plus snake comparisons) against
//! `VERBOSE_SEQUENCE_DIFF_MAX_WORK`, and a script longer than
//! `VERBOSE_SEQUENCE_DIFF_MAX_EDITS` is not itemised either; in both cases
//! the result is `SeqEdit::Replaced`, the trimmed middles as ranges. That
//! replacement is faithful but not minimal: it does not claim every item in
//! the ranges differs.

use std::ops::Range;

use crate::tunables::{VERBOSE_SEQUENCE_DIFF_MAX_EDITS, VERBOSE_SEQUENCE_DIFF_MAX_WORK};

/// The difference between two sequences.
pub(super) enum SeqEdit {
    /// A shortest edit script: old indices removed and new indices inserted,
    /// each ascending. Empty for equal sequences.
    Exact {
        removed: Vec<usize>,
        inserted: Vec<usize>,
    },
    /// Too costly to search or too long to print: the old range is replaced
    /// by the new range (zero-based, half-open, both starting at the common
    /// prefix). Never returned for equal sequences.
    Replaced {
        old: Range<usize>,
        new: Range<usize>,
    },
}

pub(super) fn seq_diff<T: Eq>(old: &[T], new: &[T]) -> SeqEdit {
    seq_diff_bounded(
        old,
        new,
        VERBOSE_SEQUENCE_DIFF_MAX_WORK,
        VERBOSE_SEQUENCE_DIFF_MAX_EDITS,
    )
}

fn seq_diff_bounded<T: Eq>(old: &[T], new: &[T], max_work: usize, max_edits: usize) -> SeqEdit {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let old_rest = &old[prefix..];
    let new_rest = &new[prefix..];
    let suffix = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old_rest[..old_rest.len() - suffix];
    let b = &new_rest[..new_rest.len() - suffix];

    let replaced = || SeqEdit::Replaced {
        old: prefix..prefix + a.len(),
        new: prefix..prefix + b.len(),
    };
    // An empty side needs no search, so the work budget never sees it; the
    // edit cap still applies before anything is allocated.
    if (a.is_empty() || b.is_empty()) && a.len() + b.len() > max_edits {
        return replaced();
    }
    let Some((mut removed, mut inserted)) = myers(a, b, max_work, max_edits) else {
        return replaced();
    };
    for i in &mut removed {
        *i += prefix;
    }
    for i in &mut inserted {
        *i += prefix;
    }
    SeqEdit::Exact { removed, inserted }
}

/// Myers' shortest edit script as (removed, inserted) indices, or `None`
/// when the work budget runs out or the script would exceed `max_edits`.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "lengths come from in-memory slices, far below isize::MAX; \
              diagonal indices are offset to be non-negative before indexing"
)]
fn myers<T: Eq>(
    a: &[T],
    b: &[T],
    max_work: usize,
    max_edits: usize,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 || m == 0 {
        return Some(((0..a.len()).collect(), (0..b.len()).collect()));
    }
    let max = n + m;
    let offset = max + 1;
    let mut v = vec![0_isize; (2 * max + 3) as usize];
    // trace[d] holds V[-d..=d] after layer d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut work = 0_usize;
    let mut done = None;

    'layers: for d in 0..=max {
        // Layer d finds scripts of exactly d edits; past the cap no
        // acceptable script can emerge, so stop searching.
        if d as usize > max_edits {
            return None;
        }
        work += (2 * d + 1) as usize;
        if work > max_work {
            return None;
        }
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d
                || (k != d && v[(offset + k - 1) as usize] < v[(offset + k + 1) as usize])
            {
                v[(offset + k + 1) as usize]
            } else {
                v[(offset + k - 1) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
                work += 1;
            }
            if work > max_work {
                return None;
            }
            v[(offset + k) as usize] = x;
            if x >= n && y >= m {
                trace.push(v[(offset - d) as usize..=(offset + d) as usize].to_vec());
                done = Some(d);
                break 'layers;
            }
            k += 2;
        }
        trace.push(v[(offset - d) as usize..=(offset + d) as usize].to_vec());
    }

    let final_d = done?;
    let mut removed = Vec::new();
    let mut inserted = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=final_d).rev() {
        let prev = &trace[(d - 1) as usize];
        let at = |k: isize| prev[(k + d - 1) as usize];
        let k = x - y;
        let down = k == -d || (k != d && at(k - 1) < at(k + 1));
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        if down {
            inserted.push(prev_y as usize);
        } else {
            removed.push(prev_x as usize);
        }
        x = prev_x;
        y = prev_y;
    }
    removed.reverse();
    inserted.reverse();
    Some((removed, inserted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(e: SeqEdit) -> (Vec<usize>, Vec<usize>) {
        match e {
            SeqEdit::Exact { removed, inserted } => (removed, inserted),
            SeqEdit::Replaced { old, new } => panic!("unexpected replacement {old:?} {new:?}"),
        }
    }

    fn apply(old: &[char], new: &[char], (removed, inserted): &(Vec<usize>, Vec<usize>)) {
        // Kept items of old must equal kept items of new, in order.
        let kept_old: Vec<char> = (0..old.len())
            .filter(|i| !removed.contains(i))
            .map(|i| old[i])
            .collect();
        let kept_new: Vec<char> = (0..new.len())
            .filter(|i| !inserted.contains(i))
            .map(|i| new[i])
            .collect();
        assert_eq!(kept_old, kept_new);
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn equal_sequences_have_no_edit() {
        let a = chars("abc");
        let (r, i) = exact(seq_diff(&a, &a));
        assert!(r.is_empty() && i.is_empty());
        // Equality never summarises, even with a zero edit cap.
        let (r, i) = exact(seq_diff_bounded(&a, &a, 0, 0));
        assert!(r.is_empty() && i.is_empty());
    }

    #[test]
    fn swap_is_one_removal_and_one_insertion() {
        let (a, b) = (chars("ab"), chars("ba"));
        let e = exact(seq_diff(&a, &b));
        assert_eq!((e.0.len(), e.1.len()), (1, 1));
        assert_eq!(a[e.0[0]], b[e.1[0]]);
        apply(&a, &b, &e);
    }

    #[test]
    fn moves_to_different_places_print_different_positions() {
        let a = chars("abc");
        let e1 = exact(seq_diff(&a, &chars("bac")));
        let e2 = exact(seq_diff(&a, &chars("bca")));
        assert_eq!(e1, (vec![0], vec![1]));
        assert_eq!(e2, (vec![0], vec![2]));
    }

    #[test]
    fn dropped_duplicate_is_one_removal() {
        let (a, b) = (chars("aa"), chars("a"));
        let (r, i) = exact(seq_diff(&a, &b));
        assert_eq!(r.len(), 1);
        assert!(i.is_empty());
    }

    #[test]
    fn edit_scripts_are_minimal_and_consistent() {
        let cases = [
            ("abcabba", "cbabac", 5),
            ("", "xyz", 3),
            ("xyz", "", 3),
            ("abcdef", "abXdef", 2),
            ("abc", "cab", 2),
            ("aaaa", "aa", 2),
            ("abcd", "dcba", 6),
        ];
        for (old, new, cost) in cases {
            let (a, b) = (chars(old), chars(new));
            let e = exact(seq_diff(&a, &b));
            assert_eq!(e.0.len() + e.1.len(), cost, "{old} -> {new}");
            apply(&a, &b, &e);
        }
    }

    #[test]
    fn exhausted_budget_replaces_whole_middle() {
        let (a, b) = (chars("pabcdq"), chars("pdcbaq"));
        match seq_diff_bounded(&a, &b, 3, usize::MAX) {
            SeqEdit::Replaced { old, new } => assert_eq!((old, new), (1..5, 1..5)),
            SeqEdit::Exact { .. } => panic!("expected a replacement"),
        }
    }

    #[test]
    fn edit_cap_replaces_long_scripts_including_bulk_insertion() {
        let a = chars("pq");
        let b = chars("pxyzq");
        match seq_diff_bounded(&a, &b, usize::MAX, 2) {
            SeqEdit::Replaced { old, new } => assert_eq!((old, new), (1..1, 1..4)),
            SeqEdit::Exact { .. } => panic!("bulk insertion must respect the edit cap"),
        }
        let (a, b) = (chars("abcd"), chars("dcba"));
        assert!(matches!(
            seq_diff_bounded(&a, &b, usize::MAX, 5),
            SeqEdit::Replaced { .. }
        ));
        // At the cap the script is still itemised.
        let e = exact(seq_diff_bounded(&a, &b, usize::MAX, 6));
        apply(&a, &b, &e);
    }

    #[test]
    fn edit_is_deterministic() {
        let (a, b) = (chars("abab"), chars("baba"));
        assert_eq!(exact(seq_diff(&a, &b)), exact(seq_diff(&a, &b)));
    }
}
