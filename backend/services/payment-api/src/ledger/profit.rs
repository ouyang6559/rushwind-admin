//! The pure agent profit split (`spec/02-funds-order.md` §5, `bianliticheng` /
//! `huoqufeilv`). A settled order pays a brokerage to every agent above the
//! merchant, equal to the original order amount times the **rate difference**
//! between the child and its parent, walking up `parentid` for at most three
//! levels — the recursion that carries the `money_changes.lx = 9` rows.
//!
//! The legacy recursion (`P::bianliticheng`, `num = 3`, `tcjb = 1`) collapses
//! here into a walk over an already-resolved `[merchant, parent, grandparent,
//! …]` chain, one rate per node, chosen for the order's settlement cycle `t`
//! (T0 reads `t0_rate`, T+1 reads `rate`, `spec/02` §5.2). The two stop rules
//! are preserved exactly:
//! - the platform / no-agent boundary (`parentid <= 1`) ends the walk;
//! - a child whose rate is **not above** its parent's (`rate_diff <= 0`) pays
//!   no brokerage and **stops the whole chain** there (the legacy `return`),
//!   so an under-priced downline cuts off every ancestor above it.
//!
//! Money is integer units; rates are `RATE_SCALE`-scaled, so the legacy
//! `(x*1000 - s*1000)/1000` float-avoidance trick becomes a plain integer
//! subtraction and the brokerage reuses [`crate::money::fee_units`] (round
//! half-up to 元-cents) exactly as the merchant fee does.

use crate::money::fee_units;

/// The legacy recursion budget (`bianliticheng` default `num = 3`) — the
/// merchant plus up to three agent levels.
pub const DEFAULT_MAX_LEVELS: usize = 3;

/// One member of the resolved profit chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainNode {
    /// The member's internal user id (`<= 1` marks the platform boundary).
    pub user_id: i64,
    /// The member's effective fee rate for the order's cycle, `RATE_SCALE`
    /// scaled (already resolved through the userrate → channel fallback).
    pub feilv: i64,
}

/// A single brokerage hop: `amount` credited to `to_user_id` because
/// `from_user_id`'s rate exceeded it by `rate_diff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Brokerage {
    /// The parent agent receiving the credit.
    pub to_user_id: i64,
    /// The downline whose higher rate generated the spread.
    pub from_user_id: i64,
    /// The level counter (`tcjb`, 1-based) recorded on the flow row.
    pub level: usize,
    /// The rate spread, `RATE_SCALE` scaled.
    pub rate_diff: i64,
    /// The brokerage amount, money units: `round(order_amount * rate_diff)`.
    pub amount: i64,
}

/// Walks the chain (index `0` is the settling merchant, `1` its direct parent,
/// …) and yields the brokerage to each qualifying ancestor, at most
/// `max_levels` hops. See the module docs for the stop rules.
pub fn split(amount_units: i64, chain: &[ChainNode], max_levels: usize) -> Vec<Brokerage> {
    let mut out = Vec::new();
    for i in 0..max_levels {
        let (Some(child), Some(parent)) = (chain.get(i), chain.get(i + 1)) else {
            break;
        };
        // `parentid <= 1` is the platform, never an agent to pay.
        if parent.user_id <= 1 {
            break;
        }
        let rate_diff = child.feilv - parent.feilv;
        // The child must carry a strictly higher rate; otherwise the chain
        // stops here (the legacy `if (ratediff <= 0) return;`).
        if rate_diff <= 0 {
            break;
        }
        out.push(Brokerage {
            to_user_id: parent.user_id,
            from_user_id: child.user_id,
            level: i + 1,
            rate_diff,
            amount: fee_units(amount_units, rate_diff),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(user_id: i64, feilv: i64) -> ChainNode {
        ChainNode { user_id, feilv }
    }

    #[test]
    fn single_agent_spread_pays_one_brokerage() {
        // 100元 order; merchant 0.6% (6_000), parent 0.5% (5_000) → 0.1% = 0.10元.
        let chain = [node(101, 6_000), node(50, 5_000)];
        let out = split(1_000_000, &chain, DEFAULT_MAX_LEVELS);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_user_id, 50);
        assert_eq!(out[0].from_user_id, 101);
        assert_eq!(out[0].level, 1);
        assert_eq!(out[0].rate_diff, 1_000);
        assert_eq!(out[0].amount, 1_000); // 0.10元 in money units
    }

    #[test]
    fn walks_up_to_three_levels() {
        // merchant 0.9%, L1 0.7%, L2 0.5%, L3 0.3% → three hops.
        let chain = [
            node(101, 9_000),
            node(50, 7_000),
            node(40, 5_000),
            node(30, 3_000),
        ];
        let out = split(1_000_000, &chain, DEFAULT_MAX_LEVELS);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].amount, 2_000); // 0.2%
        assert_eq!(out[1].amount, 2_000);
        assert_eq!(out[2].amount, 2_000);
        assert_eq!(out[2].level, 3);
        assert_eq!(out[2].to_user_id, 30);
    }

    #[test]
    fn non_positive_spread_stops_the_chain() {
        // merchant 0.5%, parent 0.7% (child cheaper than parent) → nothing.
        let chain = [node(101, 5_000), node(50, 7_000)];
        assert!(split(1_000_000, &chain, DEFAULT_MAX_LEVELS).is_empty());
        // an equal spread (0) also stops.
        let equal = [node(101, 6_000), node(50, 6_000)];
        assert!(split(1_000_000, &equal, DEFAULT_MAX_LEVELS).is_empty());
    }

    #[test]
    fn a_dead_branch_below_a_rich_one_stops_everything_above() {
        // merchant 0.9% → L1 0.7% (hop), L1 0.7% → L2 0.8% (spread ≤ 0 → stop),
        // so the L3 ancestor never gets paid even though its rate is lowest.
        let chain = [
            node(101, 9_000),
            node(50, 7_000),
            node(40, 8_000),
            node(30, 1_000),
        ];
        let out = split(1_000_000, &chain, DEFAULT_MAX_LEVELS);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_user_id, 50);
    }

    #[test]
    fn platform_boundary_ends_the_walk() {
        // parent id 1 is the platform → no brokerage even with a wide spread.
        let chain = [node(101, 9_000), node(1, 1_000)];
        assert!(split(1_000_000, &chain, DEFAULT_MAX_LEVELS).is_empty());
    }

    #[test]
    fn empty_or_single_chain_never_panics() {
        assert!(split(1_000_000, &[], DEFAULT_MAX_LEVELS).is_empty());
        assert!(split(1_000_000, &[node(101, 6_000)], DEFAULT_MAX_LEVELS).is_empty());
    }

    #[test]
    fn max_level_budget_caps_the_walk() {
        let chain = [
            node(101, 9_000),
            node(50, 7_000),
            node(40, 5_000),
            node(30, 3_000),
        ];
        // Budget of 1 pays only the direct parent.
        let out = split(1_000_000, &chain, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_user_id, 50);
    }
}
