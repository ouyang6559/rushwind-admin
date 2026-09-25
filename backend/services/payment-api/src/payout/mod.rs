//! Payout / withdrawal domain (`spec/04-payout-withdrawal.md`).
//!
//! The money-out flows (merchant settlement 提现, entrusted 委托代付 and the
//! downstream payout API) share one rule set — the `tikuanconfig` limits, the
//! holiday / trading-window / cycle gates, the single/daily/per-card amount
//! bounds and the fee formula. This module factors that shared core out of the
//! five legacy entry points:
//!
//! - [`fee`] — the pure 手续费 / 到账 / 余额扣减 formula;
//! - [`config`] — the pure guard chain (`check_withdrawal`) over a modernized
//!   [`config::PayoutConfig`], plus the personal/system merge and the thin
//!   [`config::PayoutConfigRepo`] / [`config::HolidayRepo`]; and
//! - [`state`] — the pure order state machine (`handle` normalisation,
//!   rejectability, refund split, review sub-status).
//!
//! [`request_withdrawal`] wires config + fee + validation into a validated
//! [`PayoutDraft`]; [`order::PayoutService`] is the落库 half that turns a
//! draft into the atomic debit + order + flow writes (`spec/04` §13.3).

pub mod auto_df;
pub mod channel;
pub mod config;
pub mod exec;
pub mod fee;
pub mod order;
pub mod payout_channel;
pub mod review;
pub mod state;

pub use auto_df::{
    over_cap, AutoDfConfig, AutoDfRepo, AutoDfSettings, AUTO_SUBMIT_LIMIT, AUTO_SUBMIT_TRY_CAP,
};
pub use config::{
    check_withdrawal, choose_effective, DailyState, HolidayRepo, PayoutConfig, PayoutConfigRepo,
    RequestTime, WithdrawRequest,
};
pub use fee::{compute, FeeKind, FeeRule, PayoutAmounts};
pub use state::{
    apply_outcome, refund_amounts, rejectable, ChannelOutcome, CheckStatus, ExecEffect,
    PayoutStatus, Refund, Source,
};

use sea_orm::ConnectionTrait;

use crate::state::{db_err, GatewayError, GatewayResult};

pub use exec::{
    cost_of, BatchReport, ExecAttribution, ExecResp, PayoutChannelCfg, PayoutExec, PayoutRegistry,
    SubmitGate, SubmitOutcome,
};
pub use order::{
    BankSnapshot, PaidOutcome, PayoutService, RejectOutcome, SubmitWithdrawal, Submitted,
};
pub use payout_channel::{to_cfg, NewPayoutChannel, PayoutChannelRepo, UpdatePayoutChannel};
pub use review::{parse_batch_ids, ApplyPayoutApi, ReviewBatchReport, ReviewOutcome};

/// A validated, fee-resolved withdrawal draft ready for the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayoutDraft {
    /// The merchant's internal user id.
    pub user_id: i64,
    /// The settlement `t` value recorded on the order (`0`/`1`/`7`/`30`).
    pub t: i32,
    /// The derived money (principal / fee / arrival / balance debit).
    pub amounts: PayoutAmounts,
}

/// Loads the merchant's effective config, runs the full guard chain and, if
/// it admits, computes the payout amounts. Returns the legacy rejection
/// message on any guard breach, a caller error when withdrawal is globally
/// closed, and a validated [`PayoutDraft`] otherwise. Generic over the
/// connection so [`order::PayoutService::submit_withdrawal`] can run the
/// whole chain inside its one submission tx.
pub async fn request_withdrawal<C: ConnectionTrait>(
    db: &C,
    user_id: i64,
    amount: i64,
    balance: i64,
    card_today_sum: i64,
    daily: &DailyState,
    now_ts: i64,
) -> GatewayResult<PayoutDraft> {
    let cfg = PayoutConfigRepo::new(db)
        .resolve(user_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| GatewayError::BadRequest("提款已关闭".to_string()))?;

    let req = WithdrawRequest {
        amount,
        balance,
        card_today_sum,
    };
    let now = RequestTime::from_ts(now_ts);
    let holidays = HolidayRepo::new(db).load().await.map_err(db_err)?;
    check_withdrawal(&cfg, &req, &now, &holidays, daily).map_err(GatewayError::BadRequest)?;

    Ok(PayoutDraft {
        user_id,
        t: cfg.settlement_t(),
        amounts: compute(&cfg.fee, amount),
    })
}
