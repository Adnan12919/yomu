//! Which pages to keep decoded and which to prefetch. Pure logic (no gpui), so the memory
//! behaviour is tested.
//!
//! Page *indices* always run in reading order (page `n + 1` is the next page to read), whether the
//! book is read left-to-right or right-to-left. Reading direction only decides which key, click or
//! slide direction maps to "next", so the preload plan is the same for both.

/// Pages decoded ahead of the current one.
pub const PREFETCH_AHEAD: usize = 3;
/// Pages decoded behind the current one (so going back is instant too).
pub const PREFETCH_BEHIND: usize = 1;
/// Pages kept after they fall out of the prefetch range, to avoid churn when flipping back and forth.
pub const KEEP_BEHIND: usize = 2;
pub const KEEP_AHEAD: usize = PREFETCH_AHEAD;

/// Upper bound on decoded page memory (BGRA bytes) held at once.
pub const BUDGET_BYTES: u64 = 320 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub index: usize,
    pub bytes: u64,
}

/// Pages to decode, most urgent first: the current page, then forward (the likeliest next move),
/// then one page back.
pub fn prefetch_order(current: usize, total: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(1 + PREFETCH_AHEAD + PREFETCH_BEHIND);
    order.push(current);
    order.extend((1..=PREFETCH_AHEAD).map(|ahead| current + ahead));
    order.extend((1..=PREFETCH_BEHIND).filter_map(|behind| current.checked_sub(behind)));
    // Interleave so the first page in each direction comes before the second, and so on.
    order.sort_by_key(|index| {
        let distance = index.abs_diff(current);
        (distance, *index < current)
    });
    order.retain(|index| *index < total);
    order
}

/// Which cached pages to drop: everything outside the keep window, then (if still over `budget`)
/// the pages furthest from `current`. The current page is never dropped.
pub fn plan_eviction(entries: &[Entry], current: usize, budget: u64) -> Vec<usize> {
    let keep: std::collections::HashSet<usize> =
        (current.saturating_sub(KEEP_BEHIND)..=current + KEEP_AHEAD).collect();
    plan_eviction_with_keep(entries, current, budget, &keep)
}

/// Evict decoded pages while preserving an explicit visible/prefetch set. Used by continuous
/// reading, where the viewport can cover a different number of pages depending on zoom.
pub fn plan_eviction_with_keep(
    entries: &[Entry],
    current: usize,
    budget: u64,
    keep: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    let mut evict: Vec<usize> = entries
        .iter()
        .filter(|entry| !keep.contains(&entry.index))
        .map(|entry| entry.index)
        .collect();

    let mut remaining: Vec<&Entry> = entries
        .iter()
        .filter(|e| !evict.contains(&e.index))
        .collect();
    let mut total: u64 = remaining.iter().map(|e| e.bytes).sum();
    if total > budget {
        remaining.sort_by_key(|entry| {
            (
                keep.contains(&entry.index),
                std::cmp::Reverse(entry.index.abs_diff(current)),
            )
        });
        for entry in remaining {
            if total <= budget {
                break;
            }
            if entry.index == current {
                continue;
            }
            total -= entry.bytes;
            evict.push(entry.index);
        }
    }
    evict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: usize, megabytes: u64) -> Entry {
        Entry {
            index,
            bytes: megabytes * 1024 * 1024,
        }
    }

    #[test]
    fn prefetch_goes_forward_first_then_one_back() {
        assert_eq!(prefetch_order(10, 100), vec![10, 11, 9, 12, 13]);
    }

    #[test]
    fn prefetch_is_clamped_at_both_ends() {
        assert_eq!(prefetch_order(0, 100), vec![0, 1, 2, 3]);
        assert_eq!(prefetch_order(99, 100), vec![99, 98]);
        assert_eq!(prefetch_order(98, 100), vec![98, 99, 97]);
        assert_eq!(prefetch_order(0, 1), vec![0]);
        assert_eq!(prefetch_order(0, 3), vec![0, 1, 2]);
    }

    #[test]
    fn prefetch_is_a_small_bounded_set() {
        for current in 0..50 {
            assert!(prefetch_order(current, 50).len() <= 1 + PREFETCH_AHEAD + PREFETCH_BEHIND);
        }
    }

    #[test]
    fn pages_outside_the_window_are_evicted() {
        let entries: Vec<Entry> = (0..12).map(|i| entry(i, 1)).collect();
        let mut evicted = plan_eviction(&entries, 6, BUDGET_BYTES);
        evicted.sort();
        // keeps 4..=9
        assert_eq!(evicted, vec![0, 1, 2, 3, 10, 11]);
    }

    #[test]
    fn nothing_is_evicted_inside_the_window_and_budget() {
        let entries: Vec<Entry> = (4..=9).map(|i| entry(i, 10)).collect();
        assert!(plan_eviction(&entries, 6, BUDGET_BYTES).is_empty());
    }

    #[test]
    fn over_budget_drops_the_furthest_pages_but_never_the_current_one() {
        // 6 pages x 100 MB against a 320 MB budget
        let entries: Vec<Entry> = (4..=9).map(|i| entry(i, 100)).collect();
        let mut evicted = plan_eviction(&entries, 6, BUDGET_BYTES);
        evicted.sort();
        assert_eq!(
            evicted,
            vec![4, 8, 9],
            "drops from the far end first: 9 (3 away), then 4 and 8 (2 away)"
        );
        assert!(!evicted.contains(&6));
    }

    #[test]
    fn a_single_huge_current_page_is_kept_even_over_budget() {
        let entries = vec![entry(3, 500)];
        assert!(plan_eviction(&entries, 3, BUDGET_BYTES).is_empty());
    }

    #[test]
    fn eviction_is_the_same_for_either_reading_direction() {
        // direction never enters the plan: indices are reading order
        let entries: Vec<Entry> = (0..8).map(|i| entry(i, 5)).collect();
        assert_eq!(
            plan_eviction(&entries, 4, BUDGET_BYTES),
            plan_eviction(&entries, 4, BUDGET_BYTES)
        );
    }
}
