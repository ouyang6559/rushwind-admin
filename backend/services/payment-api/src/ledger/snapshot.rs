//! The pure settlement accounting (`spec/02-funds-order.md` §3.3 orderadd
//! amounts, §4.3 T0/T1 credit, §4.4 complaints deposit, §4.5 flow rows).
//!
//! This is the arithmetic the [`crate::ledger`] service replays inside one
//! transaction (the four iron rules) — kept as offline-testable pure
//! functions so the money split is pinned before any balance is touched.
//! Money stays in integer units (1/10000 元, [`crate::money`]); rates are
//! `RATE_SCALE`-scaled integers.
//!
//! Precision note (carried into the Phase-7 语义差异清单): the legacy `cost`
//! uses `bcmul(cost_rate, pay_amount, 2)` — a *truncating* 2-decimal multiply —
//! whereas the merchant `poundage`/`actual` are raw PHP floats and the deposit
//! is `round(.., 2|4)` (the two accounting paths even disagree on the digit,
//! `spec/02` §4 / §10). The rewrite normalises every one of them to
//! [`crate::money::fee_units`] (half-up to whole 元-cents), which is the
//! single deliberate divergence from the PHP bytes.

use crate::money::fee_units;
use crate::rate::ResolvedRate;

/// The `pay_moneychange.lx` flow types (`spec/02` §4.5 full table, cross-read
/// from the legacy `exceldownload` switch). Only the order-domain codes ride
/// here; the payout reject / fee-refund codes live in [`crate::payout`].
pub mod lx {
    /// 入账 (order credited).
    pub const IN: i32 = 1;
    /// 手动增加 (admin manual +).
    pub const MANUAL_ADD: i32 = 3;
    /// 手动减少 (admin manual -).
    pub const MANUAL_SUB: i32 = 4;
    /// 冻结 (order frozen).
    pub const FREEZE: i32 = 7;
    /// 解冻 (T+1 / manual thaw, blocked → available).
    pub const UNFREEZE: i32 = 8;
    /// 提成 (agent profit split).
    pub const PROFIT: i32 = 9;
    /// 投诉保证金解冻 (complaints deposit unfreeze).
    pub const DEPOSIT_UNFREEZE: i32 = 13;
}

/// The frozen money snapshot written on the order at `orderadd`
/// (`spec/02` §3.3): the merchant fee, the 到账 amount, and the platform cost.
/// Once persisted these are never recomputed — settlement reads them back
/// verbatim (§3.2 "费率选择与订单 `t` 强绑定").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderAmounts {
    /// `pay_amount` — the original order amount, money units.
    pub amount: i64,
    /// `pay_poundage` — the merchant 手续费 (rate, then capped).
    pub poundage: i64,
    /// `pay_actualamount` — `amount - poundage`, before any deposit.
    pub actual_amount: i64,
    /// `cost` — the platform's upstream 成本 (its own, lower, rate).
    pub cost: i64,
}

impl OrderAmounts {
    /// Composes the four stored amounts from the resolved merchant `rate` and
    /// the channel's cost rate (`spec/02` §3.3 steps 2-4). `cost_rate` is a
    /// `RATE_SCALE`-scaled fraction (the platform's cost, typically below the
    /// merchant rate; the margin is the platform's gross).
    pub fn compose(amount_units: i64, rate: &ResolvedRate, cost_rate: i64) -> Self {
        let poundage = rate.poundage(amount_units);
        Self {
            amount: amount_units,
            poundage,
            actual_amount: amount_units - poundage,
            cost: fee_units(amount_units, cost_rate),
        }
    }

    /// The platform's gross margin on this order (`poundage - cost`) — the
    /// figure the profit tree and reporting reconcile against.
    pub fn margin(&self) -> i64 {
        self.poundage - self.cost
    }
}

/// The complaints-deposit rule (`spec/02` §4.4): a percentage of the 到账
/// withheld into a separate freeze ledger when the rule is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepositRule {
    /// Whether the deposit is enforced for this merchant / order.
    pub active: bool,
    /// The withheld percentage of `actual_amount` (whole percent, clamped to
    /// `[0, 100]` exactly as the legacy `min(rule.ratio, 100)` did).
    pub ratio_pct: i64,
}

impl DepositRule {
    /// No deposit withheld.
    pub const NONE: DepositRule = DepositRule {
        active: false,
        ratio_pct: 0,
    };

    /// The deposit amount for a given gross 到账 (before deduction):
    /// `round(actual * ratio%, to 元-cents)`. An inactive rule or a `0` ratio
    /// yields `0`; the clamp guarantees the result never exceeds the input.
    pub fn withheld(&self, actual_before: i64) -> i64 {
        if !self.active {
            return 0;
        }
        let ratio = self.ratio_pct.clamp(0, 100);
        // whole percent → RATE_SCALE fraction (5% == 50_000)
        fee_units(actual_before, ratio * 10_000)
    }
}

/// Which member balance bucket the net credit lands in (`spec/02` §2.4 / §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// `balance` — T+0 (`t in {0,7,30}`).
    Available,
    /// `blocked_balance` + a `blocked_log` row — T+1 (`t == 1`).
    Blocked,
}

/// The settle-time credit plan (`spec/02` §4.3 / §4.4): how much of the order's
/// `actual_amount` reaches which bucket, and how much is diverted to the
/// complaints deposit. `net_credit + deposit == actual_before`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlePlan {
    /// The amount credited to the destination bucket (after the deposit).
    pub net_credit: i64,
    /// The complaints-deposit amount diverted (a separate freeze ledger, not
    /// part of `balance`).
    pub deposit: i64,
    /// The destination bucket, chosen by the order's settlement cycle `t`.
    pub destination: Destination,
}

/// The bucket a settlement cycle `t` credits, per the legacy `switch`
/// branches (§4.3). [`None`] for a `t` with no accounting branch.
pub fn destination_of(t: i32) -> Option<Destination> {
    match t {
        0 | 7 | 30 => Some(Destination::Available),
        1 => Some(Destination::Blocked),
        _ => None,
    }
}

/// Whether `t` is a settleable settlement cycle — admission uses this so an
/// illegal cycle fails at ORDER time, not at settle time (§4.3 default).
pub fn is_valid_cycle(t: i32) -> bool {
    destination_of(t).is_some()
}

/// Plans one settlement. Returns [`None`] for a cycle `t` the legacy `switch`
/// had no branch for (an illegal `t` writes no accounting, §4.3 `default`).
pub fn plan_settlement(
    actual_before: i64,
    deposit_rule: &DepositRule,
    t: i32,
) -> Option<SettlePlan> {
    let destination = destination_of(t)?;
    let deposit = deposit_rule.withheld(actual_before);
    Some(SettlePlan {
        net_credit: actual_before - deposit,
        deposit,
        destination,
    })
}

/// A `money_changes` row intent — the pairing of a balance move with its flow
/// record (iron rule #2). The service turns one of these into an INSERT in the
/// same transaction as the atomic `balance = balance + ?` UPDATE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowIntent {
    /// The member whose balance moved.
    pub user_id: i64,
    /// Balance before (the locked row's snapshot).
    pub y_money: i64,
    /// The delta (signed for the direction the caller encodes).
    pub money: i64,
    /// Balance after (`y_money + money`) — the invariant this guarantees.
    pub g_money: i64,
    /// The flow type ([`lx`] codes).
    pub lx: i32,
    /// The platform order id (`trans_id`) the flow belongs to.
    pub trans_id: Option<String>,
    /// The merchant out order id (`order_id`), for the flow display.
    pub order_id: Option<String>,
}

/// Builds a flow intent and enforces `g_money == y_money + money` — the
/// ledger's double-entry invariant, checked at construction.
pub fn flow(
    user_id: i64,
    balance_before: i64,
    delta: i64,
    lx: i32,
    trans_id: Option<String>,
    order_id: Option<String>,
) -> FlowIntent {
    FlowIntent {
        user_id,
        y_money: balance_before,
        money: delta,
        g_money: balance_before + delta,
        lx,
        trans_id,
        order_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(feilv: i64, fengding: i64) -> ResolvedRate {
        ResolvedRate { feilv, fengding }
    }

    #[test]
    fn order_amounts_compose_fee_actual_cost() {
        // 100元 * 0.6% = 0.60元 fee, cost at 0.4% = 0.40元, actual = 99.40元.
        let a = OrderAmounts::compose(1_000_000, &rate(6_000, 0), 4_000);
        assert_eq!(a.amount, 1_000_000);
        assert_eq!(a.poundage, 6_000);
        assert_eq!(a.actual_amount, 994_000);
        assert_eq!(a.cost, 4_000);
        assert_eq!(a.margin(), 2_000); // 0.20元 gross margin
    }

    #[test]
    fn order_amounts_respects_fee_cap() {
        // 10000元 * 0.6% = 60元, cap 5元 (50_000 units) → fee 5元, actual 9995元.
        let a = OrderAmounts::compose(100_000_000, &rate(6_000, 50_000), 4_000);
        assert_eq!(a.poundage, 50_000);
        assert_eq!(a.actual_amount, 100_000_000 - 50_000);
    }

    #[test]
    fn deposit_clamps_ratio_and_withholds_percentage() {
        // 5% of a 99.40元 (994_000 units) arrival = 4.97元 → 49_700 units.
        let rule = DepositRule {
            active: true,
            ratio_pct: 5,
        };
        assert_eq!(rule.withheld(994_000), 49_700);
        // a ratio over 100 clamps to 100% (deposit == arrival)
        let over = DepositRule {
            active: true,
            ratio_pct: 150,
        };
        assert_eq!(over.withheld(500_000), 500_000);
        // inactive → nothing
        assert_eq!(DepositRule::NONE.withheld(500_000), 0);
    }

    #[test]
    fn settlement_routes_by_cycle_and_splits_deposit() {
        // T+0 (t=0) → available, no deposit → net == actual.
        let p = plan_settlement(994_000, &DepositRule::NONE, 0).unwrap();
        assert_eq!(p.destination, Destination::Available);
        assert_eq!(p.net_credit, 994_000);
        assert_eq!(p.deposit, 0);
        // T+1 (t=1) → blocked bucket.
        let p1 = plan_settlement(100_000, &DepositRule::NONE, 1).unwrap();
        assert_eq!(p1.destination, Destination::Blocked);
        // weekly / monthly (7 / 30) fall through to available.
        assert_eq!(
            plan_settlement(1, &DepositRule::NONE, 7)
                .unwrap()
                .destination,
            Destination::Available
        );
        assert_eq!(
            plan_settlement(1, &DepositRule::NONE, 30)
                .unwrap()
                .destination,
            Destination::Available
        );
        // an illegal t has no accounting branch.
        assert!(plan_settlement(1_000, &DepositRule::NONE, 2).is_none());
        // net + deposit == actual
        let rule = DepositRule {
            active: true,
            ratio_pct: 10,
        };
        let p2 = plan_settlement(1_000_000, &rule, 0).unwrap();
        assert_eq!(p2.deposit, 100_000);
        assert_eq!(p2.net_credit + p2.deposit, 1_000_000);
    }

    #[test]
    fn flow_enforces_double_entry_invariant() {
        let f = flow(
            42,
            1_000_000,
            994_000,
            lx::IN,
            Some("P20260921".into()),
            Some("MCH-1".into()),
        );
        assert_eq!(f.g_money, f.y_money + f.money);
        assert_eq!(f.g_money, 1_994_000);
        assert_eq!(f.lx, 1);
    }
}
