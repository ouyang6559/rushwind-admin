//! The pure settle orchestration (`spec/02-funds-order.md` §4). It composes
//! the three Phase-3 kernels — the status CAS ([`crate::ledger::state`]), the
//! amount math ([`crate::ledger::snapshot`]) and the profit walk
//! ([`crate::ledger::profit`]) — into the *ordered set of writes* one settle
//! transaction must emit, with no IO. The [`crate::ledger::LedgerService`]
//! replays exactly these decisions as conditional UPDATEs + atomic balance
//! SQL + flow INSERTs (the four iron rules); it never re-derives a number.
//!
//! The sequence mirrors the legacy `EditMoney` / `completeOrder` in-transaction
//! body (§4.1–§4.5): settle CAS → deposit withhold + net credit routing →
//! the merchant `lx = 1` flow → the T+1 `blocked_log` → the ancestor `lx = 9`
//! brokerage flows. The merchant notification (§4.6) and the post-commit risk
//! accumulation (§4.7) stay outside this plan (they ride the DB / Redis layer).

use crate::ledger::profit::{self, ChainNode};
use crate::ledger::snapshot::{self, lx, DepositRule, Destination, FlowIntent};
use crate::ledger::state::{settle_transition, PayStatus, SettleCas};

/// The merchant's two balance buckets immediately before the settle. The
/// destination bucket is the flow's `ymoney` snapshot (§4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MerchantBuckets {
    pub available: i64,
    pub blocked: i64,
}

/// One agent-chain node with the balance snapshot its brokerage flow records.
/// Index `0` is the settling merchant; each later node is the previous one's
/// parent (the `parentid` walk `profit::split` consumes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLevel {
    pub user_id: i64,
    /// The member's effective fee rate for the order's cycle, RATE_SCALE-scaled.
    pub feilv: i64,
    /// The parent's `balance` before its credit (the flow's `ymoney`).
    pub balance_before: i64,
}

impl AgentLevel {
    fn node(&self) -> ChainNode {
        ChainNode {
            user_id: self.user_id,
            feilv: self.feilv,
        }
    }
}

/// A complaints-deposit freeze ledger row, produced when a deposit is
/// withheld (§4.4). It is *not* a `balance` flow — the money leaves the
/// arrival and is tracked separately until its scheduled release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositEntry {
    pub amount: i64,
    /// When the deposit is released back to `balance` (a DB-side decision).
    pub unfreeze_at: i64,
}

/// A T+1 scheduled-thaw (`blocked_log`) row produced only for a [`Destination::Blocked`]
/// settle (§4.3). Its `amount` is the net credit that froze; `thaw_at` is the
/// `tomorrow + rand` release timestamp the caller computes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedLogEntry {
    pub user_id: i64,
    pub order_id: Option<String>,
    pub amount: i64,
    pub thaw_at: i64,
}

/// The ordered writes a single settle transaction must apply — every value
/// already decided, nothing left to compute at the DB boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleWrites {
    /// The `pay_status` to advance to (`Paid`, the `0 → 1` CAS target).
    pub status_to: PayStatus,
    /// The merchant's `lx = 1` arrival flow.
    pub merchant_flow: FlowIntent,
    /// The T+1 freeze ledger, present only when the credit is blocked.
    pub blocked_log: Option<BlockedLogEntry>,
    /// The complaints-deposit freeze, present only when a deposit was withheld.
    pub deposit: Option<DepositEntry>,
    /// The ancestor `lx = 9` brokerage flows (empty for a flat merchant).
    pub brokerage_flows: Vec<FlowIntent>,
}

/// The settle decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleOutcome {
    /// The order was already credited (a duplicate callback) — the caller runs
    /// no accounting but may still (re)notify the merchant (§2.1 note).
    AlreadySettled,
    /// The order is newly credited; apply [`SettleWrites`] in one transaction.
    Settled(SettleWrites),
}

/// A settle that cannot be planned (an illegal settlement cycle `t` with no
/// accounting branch, §4.3 `default`) — an order-integrity bug, never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleError {
    IllegalCycle(i32),
}

/// Everything the plan needs, with the time / randomness inputs (`thaw_at`,
/// `deposit_unfreeze_at`) supplied pre-computed by the caller so the kernel
/// stays pure and deterministic under test.
#[derive(Debug, Clone)]
pub struct SettleInput<'a> {
    /// The order's current status (the CAS source).
    pub current: PayStatus,
    /// `pay_amount` — the original order amount (the brokerage base, §5.1).
    pub order_amount: i64,
    /// `pay_actualamount` — the arrival before any deposit (§4.3).
    pub actual_before: i64,
    /// The frozen settlement cycle `t`.
    pub t: i32,
    /// The merchant's complaints-deposit rule.
    pub deposit_rule: &'a DepositRule,
    pub merchant_user_id: i64,
    pub merchant_buckets: MerchantBuckets,
    /// The agent chain, index `0` = the settling merchant.
    pub chain: &'a [AgentLevel],
    /// The T+1 release timestamp for a blocked credit (`blocked_log.thawtime`).
    pub blocked_thaw_at: i64,
    /// The release timestamp for a withheld deposit (`+ rule.freeze_time`).
    pub deposit_unfreeze_at: i64,
    /// The platform order id (`trans_id`).
    pub trans_id: Option<String>,
    /// The merchant out order id (`order_id`, display only on the flow).
    pub out_order_id: Option<String>,
}

/// Plans one settle: run the CAS, then (only on a won transition) the full
/// accounting. Returns [`SettleOutcome::AlreadySettled`] for a non-`Unpaid`
/// order so a duplicate callback is an idempotent no-op on money.
pub fn settle(input: &SettleInput<'_>) -> Result<SettleOutcome, SettleError> {
    if settle_transition(input.current) != SettleCas::Won {
        return Ok(SettleOutcome::AlreadySettled);
    }

    let plan = snapshot::plan_settlement(input.actual_before, input.deposit_rule, input.t)
        .ok_or(SettleError::IllegalCycle(input.t))?;

    // The merchant arrival flow snapshots the destination bucket's prior value.
    let bucket_before = match plan.destination {
        Destination::Available => input.merchant_buckets.available,
        Destination::Blocked => input.merchant_buckets.blocked,
    };
    let merchant_flow = snapshot::flow(
        input.merchant_user_id,
        bucket_before,
        plan.net_credit,
        lx::IN,
        input.trans_id.clone(),
        input.out_order_id.clone(),
    );

    let blocked_log = (plan.destination == Destination::Blocked).then(|| BlockedLogEntry {
        user_id: input.merchant_user_id,
        order_id: input.trans_id.clone(),
        amount: plan.net_credit,
        thaw_at: input.blocked_thaw_at,
    });

    let deposit = (plan.deposit > 0).then_some(DepositEntry {
        amount: plan.deposit,
        unfreeze_at: input.deposit_unfreeze_at,
    });

    // Brokerage walks the original order amount, not the net (§5.1).
    let nodes: Vec<ChainNode> = input.chain.iter().map(AgentLevel::node).collect();
    let brokerages = profit::split(input.order_amount, &nodes, profit::DEFAULT_MAX_LEVELS);
    let brokerage_flows = brokerages
        .iter()
        .filter_map(|b| {
            let parent = input.chain.iter().find(|n| n.user_id == b.to_user_id)?;
            Some(snapshot::flow(
                parent.user_id,
                parent.balance_before,
                b.amount,
                lx::PROFIT,
                input.trans_id.clone(),
                input.out_order_id.clone(),
            ))
        })
        .collect();

    Ok(SettleOutcome::Settled(SettleWrites {
        status_to: PayStatus::Paid,
        merchant_flow,
        blocked_log,
        deposit,
        brokerage_flows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate::ResolvedRate;

    fn merchant(feilv: i64, balance_before: i64) -> AgentLevel {
        AgentLevel {
            user_id: 101,
            feilv,
            balance_before,
        }
    }

    fn base_input<'a>(
        t: i32,
        deposit_rule: &'a DepositRule,
        chain: &'a [AgentLevel],
    ) -> SettleInput<'a> {
        SettleInput {
            current: PayStatus::Unpaid,
            order_amount: 1_000_000, // 100元
            actual_before: 994_000,  // after a 0.6% fee
            t,
            deposit_rule,
            merchant_user_id: 101,
            merchant_buckets: MerchantBuckets {
                available: 5_000_000,
                blocked: 0,
            },
            chain,
            blocked_thaw_at: 999,
            deposit_unfreeze_at: 888,
            trans_id: Some("P2026".into()),
            out_order_id: Some("MCH-1".into()),
        }
    }

    #[test]
    fn duplicate_callback_credits_nothing() {
        let chain = [merchant(6_000, 0)];
        let mut input = base_input(0, &DepositRule::NONE, &chain);
        input.current = PayStatus::Paid;
        assert_eq!(settle(&input).unwrap(), SettleOutcome::AlreadySettled);
        input.current = PayStatus::Notified;
        assert_eq!(settle(&input).unwrap(), SettleOutcome::AlreadySettled);
    }

    #[test]
    fn t0_settles_into_available_with_no_freeze() {
        let chain = [merchant(6_000, 0)];
        let input = base_input(0, &DepositRule::NONE, &chain);
        let SettleOutcome::Settled(w) = settle(&input).unwrap() else {
            panic!("expected settled");
        };
        assert_eq!(w.status_to, PayStatus::Paid);
        assert_eq!(w.merchant_flow.lx, lx::IN);
        assert_eq!(w.merchant_flow.money, 994_000);
        // available bucket snapshot
        assert_eq!(w.merchant_flow.y_money, 5_000_000);
        assert_eq!(w.merchant_flow.g_money, 5_994_000);
        assert!(w.blocked_log.is_none());
        assert!(w.deposit.is_none());
        assert!(w.brokerage_flows.is_empty()); // flat merchant (no parent in chain)
    }

    #[test]
    fn t1_settles_into_blocked_bucket_and_freezes() {
        let chain = [merchant(6_000, 0)];
        let input = base_input(1, &DepositRule::NONE, &chain);
        let SettleOutcome::Settled(w) = settle(&input).unwrap() else {
            panic!("expected settled");
        };
        // the flow snapshots the blocked bucket (was 0)
        assert_eq!(w.merchant_flow.y_money, 0);
        assert_eq!(w.merchant_flow.g_money, 994_000);
        let log = w.blocked_log.expect("T+1 writes a freeze ledger");
        assert_eq!(log.amount, 994_000);
        assert_eq!(log.thaw_at, 999);
        assert_eq!(log.user_id, 101);
    }

    #[test]
    fn deposit_withheld_reduces_net_credit() {
        let chain = [merchant(6_000, 0)];
        let rule = DepositRule {
            active: true,
            ratio_pct: 10,
        };
        let input = base_input(0, &rule, &chain);
        let SettleOutcome::Settled(w) = settle(&input).unwrap() else {
            panic!("expected settled");
        };
        // 10% of 994_000 = 99_400 withheld → net 894_600
        assert_eq!(w.merchant_flow.money, 894_600);
        let dep = w.deposit.expect("deposit entry");
        assert_eq!(dep.amount, 99_400);
        assert_eq!(dep.unfreeze_at, 888);
    }

    #[test]
    fn brokerage_flows_credit_each_ancestor_at_lx9() {
        // 100元; merchant 0.6% → parent 0.5% → grandparent 0.4% (two hops).
        let chain = [
            merchant(6_000, 0),
            AgentLevel {
                user_id: 50,
                feilv: 5_000,
                balance_before: 1_000_000,
            },
            AgentLevel {
                user_id: 40,
                feilv: 4_000,
                balance_before: 2_000_000,
            },
        ];
        let input = base_input(0, &DepositRule::NONE, &chain);
        let SettleOutcome::Settled(w) = settle(&input).unwrap() else {
            panic!("expected settled");
        };
        assert_eq!(w.brokerage_flows.len(), 2);
        // each spread is 0.1% of the 100元 base = 1_000 units
        assert_eq!(w.brokerage_flows[0].user_id, 50);
        assert_eq!(w.brokerage_flows[0].money, 1_000);
        assert_eq!(w.brokerage_flows[0].y_money, 1_000_000);
        assert_eq!(w.brokerage_flows[0].g_money, 1_001_000);
        assert_eq!(w.brokerage_flows[0].lx, lx::PROFIT);
        assert_eq!(w.brokerage_flows[1].user_id, 40);
    }

    #[test]
    fn illegal_cycle_is_an_error_not_a_silent_skip() {
        let chain = [merchant(6_000, 0)];
        let input = base_input(2, &DepositRule::NONE, &chain);
        assert_eq!(settle(&input), Err(SettleError::IllegalCycle(2)));
    }

    // A tiny helper so the tests can build a rate-derived arrival without
    // dragging in the whole orderadd path — documents the 994_000 constant.
    #[test]
    fn arrival_matches_order_amounts_compose() {
        let rate = ResolvedRate {
            feilv: 6_000,
            fengding: 0,
        };
        let a = crate::ledger::snapshot::OrderAmounts::compose(1_000_000, &rate, 4_000);
        assert_eq!(a.actual_amount, 994_000);
    }
}
