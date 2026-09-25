//! Channel / sub-account selection. The weighted round-robin the legacy
//! `getWeight` (`Application/Common/Common/function.php:655`) performs,
//! re-stated in `spec/06-risk-control-route.md`.
//!
//! Kept pure (a `roll` is injected) so the distribution is unit-testable
//! without randomness; the gateway wraps [`pick_weighted`] with a roll
//! drawn from the ambient source.

/// Weighted pick over `(id, weight)` items. `roll` must be in
/// `[0, total_weight)`; returns the id whose cumulative band contains it.
/// Zero-weight items are skipped. `None` when the set is empty or the roll
/// is out of range.
pub fn pick_weighted(items: &[(i64, i64)], roll: i64) -> Option<i64> {
    let total: i64 = items.iter().map(|(_, w)| (*w).max(0)).sum();
    if total <= 0 || roll < 0 || roll >= total {
        return None;
    }
    let mut acc: i64 = 0;
    for (id, w) in items {
        acc += (*w).max(0);
        if roll < acc {
            return Some(*id);
        }
    }
    items.iter().rev().find(|(_, w)| *w > 0).map(|(id, _)| *id) // float-rounding fallback (unreachable given the sum)
}

/// Parses a legacy weight string `pid:weight|pid:weight` into items.
pub fn parse_weight_spec(spec: &str) -> Vec<(i64, i64)> {
    spec.split('|')
        .filter_map(|pair| {
            let (pid, w) = pair.split_once(':')?;
            Some((pid.trim().parse().ok()?, w.trim().parse().ok()?))
        })
        .collect()
}

/// The total routing weight of a `(id, weight)` candidate set (negatives
/// clamped to 0, matching the legacy band math).
pub fn total_weight(items: &[(i64, i32)]) -> i64 {
    items.iter().map(|(_, w)| (*w as i64).max(0)).sum()
}

/// Picks a sub-account id by weighted round-robin over `(id, weight)`
/// candidates (the caller projects `channel_account` rows to these tuples
/// after the risk filter narrows them to `status = 1`). `roll` must be in
/// `[0, total_weight)`; use [`draw_roll`] for the ambient source.
pub fn select_account_id(items: &[(i64, i32)], roll: i64) -> Option<i64> {
    let widened: Vec<(i64, i64)> = items.iter().map(|(id, w)| (*id, *w as i64)).collect();
    pick_weighted(&widened, roll)
}

/// Draws a roll in `[0, total)` from the ambient CSPRNG for a candidate set
/// of the given total weight (`0` when the set is empty / all-zero weight,
/// which the caller treats as “no selectable account”).
pub fn draw_roll(total: i64) -> i64 {
    if total <= 0 {
        return 0;
    }
    use rand::Rng;
    rand::rng().random_range(0..total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_are_cumulative() {
        let items = vec![(10, 3), (20, 5), (30, 2)]; // total 10
        assert_eq!(pick_weighted(&items, 0), Some(10));
        assert_eq!(pick_weighted(&items, 2), Some(10));
        assert_eq!(pick_weighted(&items, 3), Some(20));
        assert_eq!(pick_weighted(&items, 7), Some(20));
        assert_eq!(pick_weighted(&items, 8), Some(30));
        assert_eq!(pick_weighted(&items, 9), Some(30));
    }

    #[test]
    fn edge_rolls_and_weights() {
        assert_eq!(pick_weighted(&[], 0), None);
        let items = vec![(1, 0), (2, 0)];
        assert_eq!(pick_weighted(&items, 0), None); // total 0
        let z = vec![(1, 0), (2, 4)];
        assert_eq!(pick_weighted(&z, 0), Some(2)); // zero-weight skipped
        assert_eq!(pick_weighted(&[(5, 3)], 3), None); // roll == total
    }

    #[test]
    fn parses_legacy_spec() {
        assert_eq!(
            parse_weight_spec("12:3|13:1|14:5"),
            vec![(12, 3), (13, 1), (14, 5)]
        );
        assert_eq!(parse_weight_spec("12:2"), vec![(12, 2)]);
        assert!(parse_weight_spec("bad").is_empty());
    }

    #[test]
    fn account_selection_hits_weight_bands_exactly() {
        let items = [(10, 3), (20, 5), (30, 2)];
        assert_eq!(total_weight(&items), 10);
        let mut counts = std::collections::BTreeMap::new();
        for roll in 0..total_weight(&items) {
            *counts.entry(select_account_id(&items, roll)).or_insert(0) += 1;
        }
        // A full roll cycle reproduces the weights exactly.
        assert_eq!(counts.get(&Some(10)), Some(&3));
        assert_eq!(counts.get(&Some(20)), Some(&5));
        assert_eq!(counts.get(&Some(30)), Some(&2));
    }

    #[test]
    fn empty_or_zero_weight_set_is_not_selectable() {
        assert_eq!(select_account_id(&[], 0), None);
        assert_eq!(total_weight(&[(1, 0), (2, 0)]), 0);
        assert_eq!(select_account_id(&[(1, 0), (2, 0)], 0), None);
        // draw_roll on a zero-total set returns 0 (the caller maps that to
        // "no account"); on a positive total it stays within range.
        assert_eq!(draw_roll(0), 0);
        for _ in 0..1000 {
            assert!((0..7).contains(&draw_roll(7)));
        }
    }
}
