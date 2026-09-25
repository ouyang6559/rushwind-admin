//! The pure payout-fee core — the withdrawal 手续费 / 到账 / 余额扣减 formula
//! the legacy repeats across `saveClearing`/`saveEntrusted`/`dfsave`/`dfPass`
//! (`spec/04` §3.3, §13.1). All amounts are money units (1/10000 元, see
//! [`crate::money`]); no clock, DB or IO is touched, so the whole formula is
//! offline-unit-tested.
//!
//! Two orthogonal switches drive it:
//! - [`FeeKind`] selects per-transaction fixed (`tktype = 1` → `sxffixed`)
//!   versus proportional (`tktype = 0` → `tkmoney × sxfrate%`); and
//! - [`FeeRule::charge_from_balance`] (`tk_charge_type`) decides whether the
//!   fee is taken out of the arrival amount (`false`: 到账扣) or added on top
//!   of the withdrawal and taken from the balance (`true`: 余额扣).

use crate::money::scale_units;

/// The fee basis (`tikuanconfig.tktype`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeKind {
    /// A fixed amount per transaction, money units (`sxffixed`).
    Fixed,
    /// A percentage of the withdrawal, RATE_SCALE-scaled (`sxfrate`).
    Percent,
}

/// An assembled fee rule for one withdrawal (from a [`super::PayoutConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRule {
    /// Fixed vs proportional basis.
    pub kind: FeeKind,
    /// The fixed fee, money units (used when [`FeeKind::Fixed`]).
    pub fixed: i64,
    /// The proportional rate, RATE_SCALE-scaled (used when [`FeeKind::Percent`]).
    pub rate_scaled: i64,
    /// `tk_charge_type`: `true` deducts the fee from the balance (到账不变),
    /// `false` deducts it from the arrival amount.
    pub charge_from_balance: bool,
}

impl FeeRule {
    /// The raw fee for a `tkmoney` withdrawal, money units.
    pub fn fee(&self, tkmoney: i64) -> i64 {
        match self.kind {
            FeeKind::Fixed => self.fixed,
            FeeKind::Percent => scale_units(tkmoney, self.rate_scaled),
        }
    }
}

/// The fully derived money for one payout order (§13.1 `payout_order`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayoutAmounts {
    /// The requested withdrawal amount (本金), money units.
    pub tkmoney: i64,
    /// The computed fee, money units.
    pub fee: i64,
    /// The amount actually credited to the payee (`money`), money units.
    pub arrival: i64,
    /// The total debit against the merchant's available balance, money
    /// units: `tkmoney` plus the fee when the fee is balance-charged.
    pub balance_debit: i64,
}

/// Derives the payout amounts from a rule and the withdrawal principal
/// (`spec/04` §3.3 落库段 :523-532): `tk_charge_type=1` keeps the arrival at
/// `tkmoney` but debits the fee on top of the balance; `=0` shrinks the
/// arrival by the fee while the balance only loses `tkmoney`.
pub fn compute(rule: &FeeRule, tkmoney: i64) -> PayoutAmounts {
    let fee = rule.fee(tkmoney);
    let arrival = if rule.charge_from_balance {
        tkmoney
    } else {
        tkmoney - fee
    };
    let balance_debit = if rule.charge_from_balance {
        tkmoney + fee
    } else {
        tkmoney
    };
    PayoutAmounts {
        tkmoney,
        fee,
        arrival,
        balance_debit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: i64 = 10_000; // 1 元 in money units

    fn percent(rate_scaled: i64, from_balance: bool) -> FeeRule {
        FeeRule {
            kind: FeeKind::Percent,
            fixed: 0,
            rate_scaled,
            charge_from_balance: from_balance,
        }
    }

    fn fixed(fee: i64, from_balance: bool) -> FeeRule {
        FeeRule {
            kind: FeeKind::Fixed,
            fixed: fee,
            rate_scaled: 0,
            charge_from_balance: from_balance,
        }
    }

    #[test]
    fn percentage_fee_deducts_from_arrival() {
        // 100元 * 2% = 2元 fee, arrival 98元, balance only loses the 100元.
        let a = compute(&percent(20_000, false), 100 * K);
        assert_eq!(a.fee, 2 * K);
        assert_eq!(a.arrival, 98 * K);
        assert_eq!(a.balance_debit, 100 * K);
    }

    #[test]
    fn percentage_fee_charged_from_balance_keeps_arrival() {
        // tk_charge_type=1: arrival stays 100元, balance loses 100 + 2 = 102元.
        let a = compute(&percent(20_000, true), 100 * K);
        assert_eq!(a.fee, 2 * K);
        assert_eq!(a.arrival, 100 * K);
        assert_eq!(a.balance_debit, 102 * K);
    }

    #[test]
    fn fixed_fee_is_amount_independent() {
        let r = fixed(5 * K, false);
        assert_eq!(compute(&r, 100 * K).fee, 5 * K);
        assert_eq!(compute(&r, 20 * K).fee, 5 * K);
        assert_eq!(compute(&r, 100 * K).arrival, 95 * K);
    }

    #[test]
    fn zero_rate_yields_no_fee() {
        let a = compute(&percent(0, false), 100 * K);
        assert_eq!(a.fee, 0);
        assert_eq!(a.arrival, 100 * K);
        assert_eq!(a.balance_debit, 100 * K);
    }
}
