//! Ordered sequence diff for `diff -v` details.
//!
//! Element equality (`crate::content`) compares relation members and tags in
//! stored order with multiplicity, so the verbose detail under a modified
//! element has to account for order and duplicates too, or a reorder prints a
//! bare `*` line. This computes an edit script between two sequences: the
//! old positions removed and the new positions inserted.
//!
//! The script explains the difference; it is not a move log. A moved item
//! shows as one removal and one insertion of the same value, without its
//! positions, and when several shortest scripts exist (duplicates, swaps) the
//! one chosen is fixed by the recurrence below, not by which item "really"
//! moved.
//!
//! Algorithm: trim the common prefix and suffix, then Myers' greedy O((N+M)D)
//! search with a per-layer snapshot for the traceback. At each diagonal `k`
//! of layer `d` the path comes from `k + 1` (an insertion) when
//! `k == -d || (k != d && V[k-1] < V[k+1])`, otherwise from `k - 1` (a
//! removal); the traceback applies the same rule. The search counts its work
//! (diagonal visits plus snake comparisons) against
//! `VERBOSE_SEQUENCE_DIFF_MAX_WORK`; past it, the whole trimmed middle is
//! reported removed and the new middle inserted. That fallback is faithful
//! but not minimal, and bounds both time and the snapshot storage.

use crate::tunables::VERBOSE_SEQUENCE_DIFF_MAX_WORK;

/// Old indices removed and new indices inserted, each ascending.
pub(super) struct SeqEdit {
    pub(super) removed: Vec<usize>,
    pub(super) inserted: Vec<usize>,
}

pub(super) fn seq_diff<T: Eq>(old: &[T], new: &[T]) -> SeqEdit {
    seq_diff_budget(old, new, VERBOSE_SEQUENCE_DIFF_MAX_WORK)
}

fn seq_diff_budget<T: Eq>(old: &[T], new: &[T], max_work: usize) -> SeqEdit {
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

    let mut edit = myers(a, b, max_work).unwrap_or_else(|| SeqEdit {
        removed: (0..a.len()).collect(),
        inserted: (0..b.len()).collect(),
    });
    for i in &mut edit.removed {
        *i += prefix;
    }
    for i in &mut edit.inserted {
        *i += prefix;
    }
    edit
}

/// Myers' shortest edit script, or `None` when the work budget runs out.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "lengths come from in-memory slices, far below isize::MAX; \
              diagonal indices are offset to be non-negative before indexing"
)]
fn myers<T: Eq>(a: &[T], b: &[T], max_work: usize) -> Option<SeqEdit> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 || m == 0 {
        return Some(SeqEdit {
            removed: (0..a.len()).collect(),
            inserted: (0..b.len()).collect(),
        });
    }
    let max = n + m;
    let offset = max + 1;
    let mut v = vec![0_isize; (2 * max + 3) as usize];
    // trace[d] holds V[-d..=d] after layer d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut work = 0_usize;
    let mut done = None;

    'layers: for d in 0..=max {
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
    Some(SeqEdit { removed, inserted })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &[char], new: &[char], edit: &SeqEdit) {
        // Kept items of old must equal kept items of new, in order.
        let kept_old: Vec<char> = (0..old.len())
            .filter(|i| !edit.removed.contains(i))
            .map(|i| old[i])
            .collect();
        let kept_new: Vec<char> = (0..new.len())
            .filter(|i| !edit.inserted.contains(i))
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
        let e = seq_diff(&a, &a);
        assert!(e.removed.is_empty() && e.inserted.is_empty());
    }

    #[test]
    fn swap_is_one_removal_and_one_insertion() {
        let (a, b) = (chars("ab"), chars("ba"));
        let e = seq_diff(&a, &b);
        assert_eq!((e.removed.len(), e.inserted.len()), (1, 1));
        assert_eq!(a[e.removed[0]], b[e.inserted[0]]);
        apply(&a, &b, &e);
    }

    #[test]
    fn dropped_duplicate_is_one_removal() {
        let (a, b) = (chars("aa"), chars("a"));
        let e = seq_diff(&a, &b);
        assert_eq!(e.removed.len(), 1);
        assert!(e.inserted.is_empty());
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
            let e = seq_diff(&a, &b);
            assert_eq!(e.removed.len() + e.inserted.len(), cost, "{old} -> {new}");
            apply(&a, &b, &e);
        }
    }

    #[test]
    fn exhausted_budget_falls_back_to_whole_middle() {
        let (a, b) = (chars("pabcdq"), chars("pdcbaq"));
        let e = seq_diff_budget(&a, &b, 3);
        assert_eq!(e.removed, vec![1, 2, 3, 4]);
        assert_eq!(e.inserted, vec![1, 2, 3, 4]);
        apply(&a, &b, &e);
    }

    #[test]
    fn edit_is_deterministic() {
        let (a, b) = (chars("abab"), chars("baba"));
        let e1 = seq_diff(&a, &b);
        let e2 = seq_diff(&a, &b);
        assert_eq!((e1.removed, e1.inserted), (e2.removed, e2.inserted));
    }
}
