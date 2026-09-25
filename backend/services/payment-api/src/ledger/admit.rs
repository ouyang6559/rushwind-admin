//! Pre-dispatch order admission (`spec/02-funds-order.md` §3.1 steps
//! 10–11) — the pure checks between "signature + risk gate passed" and the
//! order INSERT. It validates the resolved rate snapshot, then freezes the
//! amounts via [`OrderAmounts::compose`] so the order row stores exactly what
//! the settle kernel will later replay.
//!
//! The legacy let several bad configurations through to the money:
//! * an amount must be `> 0` (P:151-153 金額錯誤) — kept as [`AdmitError::AmountNotPositive`];
//! * a merchant with NEITHER a `userrate` row NOR a channel default silently
//!   produced a ZERO-fee order — we reject [`AdmitError::RateUnresolved`]
//!   instead (intentional deviation, Phase-7 语义差异清单);
//! * an unhandled settlement cycle `t` only surfaced at settle time (§4.3
//!   `default` wrote no accounting) — since `t` is frozen ON the order
//!   (P:204), admission validates it up front ([`AdmitError::IllegalCycle`]);
//! * a misconfigured rate ≥ 100% (fee ≥ amount) would credit a non-positive
//!   arrival — [`AdmitError::FeeConsumesAmount`] blocks it (cap wins first, so
//!   a high rate under a low 封顶 still admits).
//!
//! Per-transaction amount bands and merchant counters live in
//! [`crate::risk`] (the sub-account 限额 of P:57-99), and the merchant
//! existence / status / apikey checks run in the gateway handler before this
//! kernel — admission only sees well-formed, authenticated candidates.

use crate::rate::ResolvedRate;

use super::snapshot::{self, OrderAmounts};

/// Everything admission reads — all values are already-loaded inputs (the
/// handler resolves identity, the rate layer resolves [`ResolvedRate`]).
#[derive(Debug, Clone)]
pub struct AdmitRequest<'a> {
    /// The parsed, positive-checked `pay_amount` in money units.
    pub amount_units: i64,
    /// The effective rate snapshot for the order's cycle ([`crate::rate::resolve`]).
    pub rate: &'a ResolvedRate,
    /// The cycle-selected channel cost rate to freeze into `cost` (§3.3 #4).
    pub cost_rate: i64,
    /// The settlement cycle `t` frozen from `tikuanconfig.t1zt` (§3.2).
    pub t: i32,
}

/// Why an order was not admitted. Every variant is a caller-facing rejection
/// (never an internal fault) — mapped to [`crate::state::GatewayError::BadRequest`]
/// by [`AdmitError::message`]'s legacy wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitError {
    /// `pay_amount` absent / zero / negative (legacy 金額錯誤, P:151-153).
    AmountNotPositive,
    /// Neither the merchant `userrate` nor the channel default carries a rate
    /// — the order would silently be free.
    RateUnresolved,
    /// `t` has no settle accounting branch (§4.3); fail at order, not settle.
    IllegalCycle(i32),
    /// Fee consumes the whole order amount (rate / 封顶 misconfiguration).
    FeeConsumesAmount,
}

impl AdmitError {
    /// The caller-facing legacy gateway wording.
    pub fn message(&self) -> String {
        match self {
            AdmitError::AmountNotPositive => "金额错误".into(),
            AdmitError::RateUnresolved => "费率未配置".into(),
            AdmitError::IllegalCycle(t) => format!("结算周期异常: t={t}"),
            AdmitError::FeeConsumesAmount => "手续费率超出金额配置".into(),
        }
    }
}

impl From<AdmitError> for crate::state::GatewayError {
    fn from(e: AdmitError) -> Self {
        crate::state::GatewayError::BadRequest(e.message())
    }
}

/// Validates admission and returns the frozen [`OrderAmounts`] to store on
/// the order row. Check order mirrors §3.1: amount → rate → cycle → compose.
pub fn admit(req: &AdmitRequest<'_>) -> Result<OrderAmounts, AdmitError> {
    if req.amount_units <= 0 {
        return Err(AdmitError::AmountNotPositive);
    }
    if req.rate.feilv <= 0 {
        return Err(AdmitError::RateUnresolved);
    }
    if !snapshot::is_valid_cycle(req.t) {
        return Err(AdmitError::IllegalCycle(req.t));
    }
    let amounts = OrderAmounts::compose(req.amount_units, req.rate, req.cost_rate);
    if amounts.poundage >= amounts.amount {
        return Err(AdmitError::FeeConsumesAmount);
    }
    Ok(amounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::RATE_SCALE;

    fn rate(feilv: i64, fengding: i64) -> ResolvedRate {
        ResolvedRate { feilv, fengding }
    }

    fn req<'a>(amount_units: i64, rate: &'a ResolvedRate, t: i32) -> AdmitRequest<'a> {
        AdmitRequest {
            amount_units,
            rate,
            cost_rate: 4_000, // 0.4% channel cost
            t,
        }
    }

    #[test]
    fn non_positive_amount_is_rejected() {
        let r = rate(6_000, 0);
        assert_eq!(admit(&req(0, &r, 0)), Err(AdmitError::AmountNotPositive));
        assert_eq!(
            admit(&req(-1_000, &r, 0)),
            Err(AdmitError::AmountNotPositive)
        );
    }

    #[test]
    fn zero_rate_is_rejected_not_silently_free() {
        // Legacy would have created a 0-fee order; the rewrite refuses.
        let r = rate(0, 0);
        assert_eq!(
            admit(&req(1_000_000, &r, 0)),
            Err(AdmitError::RateUnresolved)
        );
    }

    #[test]
    fn only_settleable_cycles_admit() {
        let r = rate(6_000, 0);
        for t in [0, 1, 7, 30] {
            assert!(admit(&req(1_000_000, &r, t)).is_ok(), "t={t} should admit");
        }
        assert_eq!(
            admit(&req(1_000_000, &r, 2)),
            Err(AdmitError::IllegalCycle(2))
        );
        assert_eq!(
            admit(&req(1_000_000, &r, -1)),
            Err(AdmitError::IllegalCycle(-1))
        );
    }

    #[test]
    fn happy_path_freezes_the_full_amount_snapshot() {
        let r = rate(6_000, 0);
        let a = admit(&req(1_000_000, &r, 0)).unwrap();
        assert_eq!(a.amount, 1_000_000); // 100元
        assert_eq!(a.poundage, 6_000); // 0.6% fee
        assert_eq!(a.actual_amount, 994_000);
        assert_eq!(a.cost, 4_000); // 0.4% cost = 0.4元
        assert_eq!(a.margin(), 2_000);
    }

    #[test]
    fn fee_of_100_percent_consumes_the_order() {
        let r = rate(RATE_SCALE, 0); // 100%
        assert_eq!(
            admit(&req(1_000_000, &r, 0)),
            Err(AdmitError::FeeConsumesAmount)
        );
    }

    #[test]
    fn a_low_cap_rescues_a_high_rate() {
        // 100% rate but a 5元 封顶 → fee 5元 < 100元 → admits with the cap.
        let r = rate(RATE_SCALE, 50_000);
        let a = admit(&req(1_000_000, &r, 1)).unwrap();
        assert_eq!(a.poundage, 50_000);
        assert_eq!(a.actual_amount, 950_000);
    }

    #[test]
    fn errors_map_to_legacy_bad_request_wording() {
        let e: crate::state::GatewayError = AdmitError::RateUnresolved.into();
        assert!(matches!(e, crate::state::GatewayError::BadRequest(m) if m == "费率未配置"));
    }
}
