//! The pure payout state machine (`spec/04` §4, §7, §8.3, §13.2). It owns the
//! main order status shared by the legacy `pay_tklist` (merchant settlement)
//! and `pay_wttklist` (entrusted / API payout), the channel-execution result
//! normalisation (`handle`), the rejectability rule and the reject-time
//! refund split — all offline-unit-tested with no IO.
//!
//! The single most important legacy trap captured here (§8.3 / §12.7): a
//! channel **failure** (`ChannelOutcome::Failed`) is *not* recorded as
//! terminal `Failed`, it drops to [`PayoutStatus::Unconfirmed`] (`4` — "转账
//! 失败 / 待确认") so the query loop can re-confirm it. `4` is therefore not a
//! terminal state and only the查单 path can settle it.

/// The payout order main status (`tklist/wttklist.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutStatus {
    /// `0` 待处理.
    Pending,
    /// `1` 处理中 (submitted to the channel).
    Processing,
    /// `2` 已打款 (terminal success).
    Success,
    /// `3` 已驳回 / 失败 (terminal; balance refunded).
    Failed,
    /// `4` 待确认 (channel reported failure, awaiting query re-confirm).
    Unconfirmed,
}

impl PayoutStatus {
    /// The stored `status` code.
    pub fn code(self) -> i16 {
        match self {
            PayoutStatus::Pending => 0,
            PayoutStatus::Processing => 1,
            PayoutStatus::Success => 2,
            PayoutStatus::Failed => 3,
            PayoutStatus::Unconfirmed => 4,
        }
    }

    /// Parses a stored `status` code; unknown codes read as [`None`].
    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            0 => Some(PayoutStatus::Pending),
            1 => Some(PayoutStatus::Processing),
            2 => Some(PayoutStatus::Success),
            3 => Some(PayoutStatus::Failed),
            4 => Some(PayoutStatus::Unconfirmed),
            _ => None,
        }
    }

    /// Whether no further transition is allowed.
    pub fn is_terminal(self) -> bool {
        matches!(self, PayoutStatus::Success | PayoutStatus::Failed)
    }
}

/// The normalised channel result (§9.1): every adapter's `PaymentExec`/
/// `PaymentQuery` returns one of these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOutcome {
    /// `1` 提交成功 / 处理中.
    Processing,
    /// `2` 成功.
    Success,
    /// `3` 失败.
    Failed,
    /// `4` 待确认 / 未知.
    Unknown,
}

impl ChannelOutcome {
    /// Parses the raw channel status int (1..=4).
    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(ChannelOutcome::Processing),
            2 => Some(ChannelOutcome::Success),
            3 => Some(ChannelOutcome::Failed),
            4 => Some(ChannelOutcome::Unknown),
            _ => None,
        }
    }
}

/// The effect of folding a [`ChannelOutcome`] into the current status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecEffect {
    /// The status to persist.
    pub status: PayoutStatus,
    /// Whether to stamp `cldatetime` (settle time) — only on success.
    pub settled: bool,
    /// Whether the status actually changed (else leave the row untouched).
    pub changed: bool,
}

/// `Payment/PaymentController::handle` (§8.3): `1→处理中`, `2→已打款` (+
/// settle time), `3→待确认` (the failure trap), `4→` keep as-is (no change).
pub fn apply_outcome(current: PayoutStatus, outcome: ChannelOutcome) -> ExecEffect {
    let (status, settled) = match outcome {
        ChannelOutcome::Processing => (PayoutStatus::Processing, false),
        ChannelOutcome::Success => (PayoutStatus::Success, true),
        ChannelOutcome::Failed => (PayoutStatus::Unconfirmed, false),
        ChannelOutcome::Unknown => (current, false),
    };
    ExecEffect {
        status,
        settled,
        changed: status != current || settled,
    }
}

/// The payout origin (`payout_order.source`, §13.1): it selects the reject
/// rules and the ledger `lx` flow types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `1` 商户结算提现 (`pay_tklist`).
    Settlement,
    /// `2` 委托代付 / 批量 (`pay_wttklist`).
    Entrusted,
    /// `3` 代付 API (`pay_df_api_order` → `pay_wttklist`).
    PayoutApi,
}

impl Source {
    /// The stored `source` code.
    pub fn code(self) -> i16 {
        match self {
            Source::Settlement => 1,
            Source::Entrusted => 2,
            Source::PayoutApi => 3,
        }
    }

    /// Parses a stored `source` code; unknown codes read as [`None`].
    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(Source::Settlement),
            2 => Some(Source::Entrusted),
            3 => Some(Source::PayoutApi),
            _ => None,
        }
    }

    /// The principal-refund `money_changes.lx` (§4.2 / §4.3 / §6.4).
    pub fn principal_lx(self) -> i32 {
        match self {
            Source::Settlement => 11,
            Source::Entrusted | Source::PayoutApi => 12,
        }
    }

    /// The fee-refund `money_changes.lx` (only when the fee was
    /// balance-charged; §4.2 / §4.3).
    pub fn fee_lx(self) -> i32 {
        match self {
            Source::Settlement => 17,
            Source::Entrusted | Source::PayoutApi => 15,
        }
    }

    /// The balance-charged fee-**debit** `money_changes.lx` booked at submit
    /// / dfPass (§3.3 手动结算扣费 `16` vs §6.4/§7.4 代付审核扣费 `14`). The
    /// principal debit is `lx=6` for every source.
    pub fn fee_debit_lx(self) -> i32 {
        match self {
            Source::PayoutApi => 14,
            Source::Settlement | Source::Entrusted => 16,
        }
    }
}

/// Whether a status may be rejected for a given origin (§4.2 tklist only from
/// `Pending`; §4.3/§6.4 wttklist/df from `Pending`/`Processing`/`Unconfirmed`).
pub fn rejectable(source: Source, status: PayoutStatus) -> bool {
    match source {
        Source::Settlement => status == PayoutStatus::Pending,
        Source::Entrusted | Source::PayoutApi => {
            matches!(
                status,
                PayoutStatus::Pending | PayoutStatus::Processing | PayoutStatus::Unconfirmed
            )
        }
    }
}

/// The money returned to the merchant on reject (§4.2 refund formula): the
/// principal always comes back; the fee only when it was balance-charged
/// (`tk_charge_type = 1`) — the arrival-charged mode never debited the fee
/// from the balance separately, so there is nothing to refund.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refund {
    /// The principal returned (`tkmoney`), money units.
    pub principal: i64,
    /// The fee returned, money units (always `0` when arrival-charged).
    pub fee: i64,
}

impl Refund {
    /// The total balance credit.
    pub fn total(self) -> i64 {
        self.principal + self.fee
    }
}

/// Computes the reject refund from the order's stored amounts.
pub fn refund_amounts(charge_from_balance: bool, tkmoney: i64, fee: i64) -> Refund {
    Refund {
        principal: tkmoney,
        fee: if charge_from_balance { fee } else { 0 },
    }
}

/// The downstream-API review sub-status (`df_api_order.check_status`, §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// `0` 待审核.
    Pending,
    /// `1` 已通过 (a `wttklist` row has been generated).
    Approved,
    /// `2` 已驳回.
    Rejected,
}

impl CheckStatus {
    /// The stored `check_status` code.
    pub fn code(self) -> i16 {
        match self {
            CheckStatus::Pending => 0,
            CheckStatus::Approved => 1,
            CheckStatus::Rejected => 2,
        }
    }

    /// Parses a stored `check_status` code.
    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            0 => Some(CheckStatus::Pending),
            1 => Some(CheckStatus::Approved),
            2 => Some(CheckStatus::Rejected),
            _ => None,
        }
    }

    /// Only a `Pending` order can be approved (§6.4 `dfPass` blocks re-review).
    pub fn can_approve(self) -> bool {
        self == CheckStatus::Pending
    }

    /// Only a `Pending` order can be directly rejected before a `wttklist`
    /// exists (§6.4 `dfReject`); an `Approved` one rejects via the payout
    /// rollback path instead.
    pub fn can_reject(self) -> bool {
        self == CheckStatus::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_roundtrip() {
        for (s, c) in [
            (PayoutStatus::Pending, 0),
            (PayoutStatus::Processing, 1),
            (PayoutStatus::Success, 2),
            (PayoutStatus::Failed, 3),
            (PayoutStatus::Unconfirmed, 4),
        ] {
            assert_eq!(s.code(), c);
            assert_eq!(PayoutStatus::from_code(c), Some(s));
        }
        assert_eq!(PayoutStatus::from_code(9), None);
    }

    #[test]
    fn terminal_states_are_fixed() {
        assert!(PayoutStatus::Success.is_terminal());
        assert!(PayoutStatus::Failed.is_terminal());
        assert!(!PayoutStatus::Unconfirmed.is_terminal());
        assert!(!PayoutStatus::Pending.is_terminal());
    }

    #[test]
    fn channel_failure_degrades_to_unconfirmed() {
        let e = apply_outcome(PayoutStatus::Processing, ChannelOutcome::Failed);
        assert_eq!(e.status, PayoutStatus::Unconfirmed);
        assert!(!e.settled);
        assert!(e.changed);
    }

    #[test]
    fn channel_success_stamps_settle_time() {
        let e = apply_outcome(PayoutStatus::Processing, ChannelOutcome::Success);
        assert_eq!(e.status, PayoutStatus::Success);
        assert!(e.settled);
    }

    #[test]
    fn channel_unknown_leaves_row_untouched() {
        let e = apply_outcome(PayoutStatus::Processing, ChannelOutcome::Unknown);
        assert_eq!(e.status, PayoutStatus::Processing);
        assert!(!e.changed);
        assert!(!e.settled);
    }

    #[test]
    fn settlement_rejects_only_from_pending() {
        assert!(rejectable(Source::Settlement, PayoutStatus::Pending));
        assert!(!rejectable(Source::Settlement, PayoutStatus::Processing));
        assert!(!rejectable(Source::Settlement, PayoutStatus::Success));
    }

    #[test]
    fn payout_rejects_from_pending_processing_unconfirmed() {
        for s in [
            PayoutStatus::Pending,
            PayoutStatus::Processing,
            PayoutStatus::Unconfirmed,
        ] {
            assert!(rejectable(Source::Entrusted, s));
            assert!(rejectable(Source::PayoutApi, s));
        }
        assert!(!rejectable(Source::Entrusted, PayoutStatus::Success));
        assert!(!rejectable(Source::Entrusted, PayoutStatus::Failed));
    }

    #[test]
    fn refund_returns_fee_only_when_balance_charged() {
        let bal = refund_amounts(true, 100_000, 2_000);
        assert_eq!(bal.total(), 102_000);
        let arrival = refund_amounts(false, 100_000, 2_000);
        assert_eq!(arrival.fee, 0);
        assert_eq!(arrival.total(), 100_000);
    }

    #[test]
    fn lx_codes_match_legacy_flows() {
        assert_eq!(Source::Settlement.principal_lx(), 11);
        assert_eq!(Source::Settlement.fee_lx(), 17);
        assert_eq!(Source::Entrusted.principal_lx(), 12);
        assert_eq!(Source::PayoutApi.fee_lx(), 15);
        // Fee-debit: the settlement path charges 16, the df API review 14.
        assert_eq!(Source::Settlement.fee_debit_lx(), 16);
        assert_eq!(Source::Entrusted.fee_debit_lx(), 16);
        assert_eq!(Source::PayoutApi.fee_debit_lx(), 14);
    }

    #[test]
    fn check_status_only_pending_transitions() {
        assert!(CheckStatus::Pending.can_approve());
        assert!(CheckStatus::Pending.can_reject());
        assert!(!CheckStatus::Approved.can_approve());
        assert!(!CheckStatus::Rejected.can_reject());
        assert_eq!(CheckStatus::Approved.code(), 1);
        assert_eq!(CheckStatus::from_code(2), Some(CheckStatus::Rejected));
    }
}
