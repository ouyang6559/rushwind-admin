//! The downstream payout-API review flow (`spec/04` §6.3 / §6.4 / §7.2):
//! the merchant-panel `check_status` state machine that turns a signed payout
//! **application** into a booked, queue-ready order (`df_pass`) or refuses it
//! (`df_reject`). It rides the SAME unified `payout_orders` row as the
//! settlement line (§13.1 merges `df_api_order` + `wttklist` into one row,
//! discriminated by `source = 3`), so the review flips `check_status` on the
//! very row the execution queue (`super::exec`) later sweeps — and the
//! `status = 0` it lands on is exactly the queue's entry state.
//!
//! The `check_status` machine (§6.3):
//! ```text
//!   apply (§7.2 add, no debit) → 0 待审核
//!        │ df_pass (guards + debit)      │ df_reject
//!        ▼                                ▼
//!   1 已通过 (status=0, ready)      2 已驳回 (terminal)
//! ```
//! `df_pass` re-runs the FULL guard chain at approval time (§6.4 repeats
//! holiday / config / window / cycle / quota) and only then debits — so the
//! merchant's balance is NOT touched when the application is filed, matching
//! the legacy where `df_api_order` is a request and the debit belongs to the
//! `wttklist` created on approval.
//!
//! Faithful quirks kept (登记在语义差异清单):
//! - §6.4 dfReject refunds **only the principal (`tkmoney`), never the fee**
//!   (`df_charge_type` fee-split refunds live only in the Admin reject paths),
//!   so a balance-charged fee is forfeited on an API rejection;
//! - the fee-**debit** rides `lx = 14`「委托提现扣除手续费」 (not the
//!   settlement's `16`), while the refund rides `lx = 12`「商户代付驳回」;
//! - an approved order whose `status` already left `0` (the platform is
//!   executing it) is NOT rejectable (§6.4「后台已处理代付，不能驳回」).

use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set,
    Statement, TransactionTrait, Value as SeaValue,
};

use crate::data::payout_orders;
use crate::ledger::db::{credit, insert_flow, is_unique_violation};
use crate::state::{db_err, GatewayError, GatewayResult};

use super::order::{
    balance_opt, card_rollup, daily_rollup, debit_guarded, fee_debit_flow, next_order_no,
    principal_debit_flow, principal_flow, BankSnapshot, PayoutService,
};
use super::state::{CheckStatus, PayoutStatus, Refund, Source};
use super::{request_withdrawal, PayoutDraft};

/// A downstream payout-API application (§7.2 `Dfpay::add` persistence core —
/// the sign / domain / IP verification is the HTTP handler's job, a later
/// slice). `amount` is the requested principal; the fee is resolved later at
/// [`PayoutService::df_pass`] so the current config governs the debit.
#[derive(Debug, Clone)]
pub struct ApplyPayoutApi<'a> {
    pub user_id: i64,
    /// Requested principal (legacy `df_api_order.money`), money units.
    pub amount: i64,
    /// The downstream merchant's own order no — REQUIRED and the idempotency
    /// key (the partial unique index `(user_id, out_trade_no)`, §13.4).
    pub out_trade_no: &'a str,
    pub bank: BankSnapshot,
    /// The channel extension JSON (`extends`), stored on the row's
    /// `additional` snapshot column.
    pub extends: Option<String>,
}

/// The outcome of a [`PayoutService::df_pass`] / [`PayoutService::df_reject`].
#[derive(Debug, Clone)]
pub enum ReviewOutcome {
    /// `df_pass` won: the balance is debited, `check_status = 1`, `status = 0`
    /// — the order is now on the execution queue.
    Approved(payout_orders::Model),
    /// `df_reject` won. `refund.principal` is the returned balance (the whole
    /// `tkmoney` when the order had been debited, `0` when it was still
    /// pending-approval and never touched the balance); the fee is never
    /// returned (§6.4).
    Rejected {
        order: payout_orders::Model,
        refund: Refund,
        /// The merchant's post-reject balance, money units.
        balance_after: i64,
    },
    /// A replay of an approval / an already-approved order.
    AlreadyApproved(payout_orders::Model),
    /// A replay of a rejection / an already-rejected order.
    AlreadyRejected(payout_orders::Model),
    /// `df_reject` refused: an approved payout already left `status = 0`
    /// (the platform is executing it), §6.4.
    NotRejectable(payout_orders::Model),
}

/// The pure `df_reject` branch decision (§6.4's three cases), split out so
/// the refund / refusal rule is offline-unit-testable without a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectPlan {
    /// `check_status = 2`: already rejected — idempotent no-op.
    Already,
    /// `check_status = 1` but `status != 0`: the平台已处理, cannot reject.
    NotRejectable,
    /// `check_status = 0`: never debited — flip to rejected, no refund.
    FlipOnly,
    /// `check_status = 1` and `status = 0`: refund the principal.
    RefundPrincipal,
}

fn reject_plan(check: CheckStatus, status: i16) -> RejectPlan {
    match check {
        CheckStatus::Rejected => RejectPlan::Already,
        CheckStatus::Approved => {
            if status == PayoutStatus::Pending.code() {
                RejectPlan::RefundPrincipal
            } else {
                RejectPlan::NotRejectable
            }
        }
        CheckStatus::Pending => RejectPlan::FlipOnly,
    }
}

/// The per-row tally of a [`PayoutService::df_pass_batch`] /
/// [`PayoutService::df_reject_batch`] sweep (§6.5). A batch is NOT one big
/// transaction — the legacy loops the ids and gives EACH row its own
/// commit/rollback, so a mixed run is the normal outcome (「成功 X 失败 Y」).
/// A row lands in `succeeded` iff its single-row call committed (an
/// idempotent re-approve / re-reject counts — the row is decided); it lands in
/// `failures` (carrying the exact legacy message) iff the call rolled back or
/// was refused (balance short, guard breach, 平台已处理, …).
#[derive(Debug, Clone, Default)]
pub struct ReviewBatchReport {
    /// `(order_no, outcome)` for every row that committed.
    pub succeeded: Vec<(String, ReviewOutcome)>,
    /// `(order_no, legacy message)` for every row that failed.
    pub failures: Vec<(String, String)>,
}

impl ReviewBatchReport {
    pub fn succeeded_count(&self) -> usize {
        self.succeeded.len()
    }

    pub fn failed_count(&self) -> usize {
        self.failures.len()
    }

    /// The legacy summary string 「成功 X 失败 Y」 (`dfPassBatch:2824-2830`).
    pub fn summary(&self) -> String {
        format!(
            "成功 {} 失败 {}",
            self.succeeded_count(),
            self.failed_count()
        )
    }
}

/// Splits a batch id string on `_` (the legacy `explode('_', $ids)`,
/// `dfPassBatch:2600`), dropping empty segments so a trailing separator (the
/// JS `ids.join('_') + '_'` shape) never yields a phantom row.
pub fn parse_batch_ids(raw: &str) -> Vec<String> {
    raw.split('_')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl PayoutService {
    /// Files a downstream payout application: a `source = 3`, `check_status =
    /// 0`, `status = 0` row with the balance **untouched**, idempotent on
    /// `(user_id, out_trade_no)` — a raced duplicate resurfaces the first row
    /// instead of forking a second application (§12.6 / §13.4).
    pub async fn apply_payout_api(
        &self,
        req: &ApplyPayoutApi<'_>,
    ) -> GatewayResult<payout_orders::Model> {
        if let Some(existing) = self.by_out_trade_no(req.user_id, req.out_trade_no).await? {
            return Ok(existing);
        }
        match insert_application(self.conn(), req).await {
            Ok(order) => Ok(order),
            Err(e) if is_unique_violation(&e) => {
                // The racing twin under the same out_trade_no already filed;
                // resurface it (the tx here is a plain insert, nothing to
                // roll back).
                Ok(self
                    .by_out_trade_no(req.user_id, req.out_trade_no)
                    .await?
                    .ok_or_else(|| GatewayError::BadRequest("代付申请已存在".to_string()))?)
            }
            Err(e) => Err(db_err(e)),
        }
    }

    /// §6.4 审核通过: re-runs the full guard chain + fee math on the stored
    /// `tkmoney` (§6.4 repeats every check at approval), then — in ONE tx —
    /// CAS-flips `check_status 0→1`, applies the guarded atomic debit and the
    /// `lx = 6` / balance-charged `lx = 14` flows. A raced double-approve
    /// loses the `check_status` CAS and rolls its debit back, resurfacing the
    /// already-approved row idempotently.
    pub async fn df_pass(&self, order_no: &str, now_ts: i64) -> GatewayResult<ReviewOutcome> {
        let order = self
            .find(order_no)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
        match CheckStatus::from_code(order.check_status.unwrap_or(-1)) {
            Some(CheckStatus::Approved) => return Ok(ReviewOutcome::AlreadyApproved(order)),
            Some(CheckStatus::Rejected) => return Ok(ReviewOutcome::AlreadyRejected(order)),
            Some(CheckStatus::Pending) => {}
            _ => {
                return Err(GatewayError::Internal(format!(
                    "df_pass: 非代付审核单 check_status={:?}",
                    order.check_status
                )))
            }
        }

        let txn = self.conn().begin().await?;
        let hint = balance_opt(&txn, order.user_id).await?.ok_or_else(|| {
            GatewayError::Internal(format!("df_pass: member {} missing", order.user_id))
        })?;
        let daily = daily_rollup(&txn, order.user_id, now_ts).await?;
        let card_sum = match &order.cardnumber {
            Some(card) => card_rollup(&txn, order.user_id, card, now_ts).await?,
            None => 0,
        };
        // The whole guard chain + fee math, recomputed at approval time (§6.4).
        let draft = request_withdrawal(
            &txn,
            order.user_id,
            order.tkmoney,
            hint,
            card_sum,
            &daily,
            now_ts,
        )
        .await?;
        let charge_type = i32::from(draft.amounts.balance_debit != draft.amounts.tkmoney);

        // CAS `check_status 0→1` FIRST (refreshing the derived money) so a
        // raced approve folds nothing; the debit rides the same tx.
        let cas = txn
            .execute_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "UPDATE payout_orders SET check_status = 1, review_time = $2, status = 0, \
                 t = $3, sxfmoney = $4, money = $5, charge_type = $6 \
                 WHERE order_no = $1 AND check_status = 0",
                vec![
                    SeaValue::from(order_no.to_string()),
                    SeaValue::from(now_ts),
                    SeaValue::from(draft.t),
                    SeaValue::from(draft.amounts.fee),
                    SeaValue::from(draft.amounts.arrival),
                    SeaValue::from(charge_type),
                ],
            ))
            .await
            .map_err(db_err)?;
        if cas.rows_affected() == 0 {
            txn.rollback().await?;
            let fresh = self
                .find(order_no)
                .await?
                .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
            return Ok(
                match CheckStatus::from_code(fresh.check_status.unwrap_or(-1)) {
                    Some(CheckStatus::Approved) => ReviewOutcome::AlreadyApproved(fresh),
                    _ => ReviewOutcome::AlreadyRejected(fresh),
                },
            );
        }

        // Iron rule #1: the guarded atomic debit (a raced-away balance loses
        // the `balance >= $1` guard → rollback, the approve never books).
        let debit = draft.amounts.balance_debit;
        let after = match debit_guarded(&txn, order.user_id, debit).await? {
            Some(after) => after,
            None => {
                txn.rollback().await?;
                return Err(GatewayError::BadRequest("余额不足！".to_string()));
            }
        };
        let before = after + debit;
        write_review_debit_flows(&txn, &draft, order_no, before).await?;

        txn.commit().await?;
        let order = self
            .find(order_no)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
        Ok(ReviewOutcome::Approved(order))
    }

    /// §6.4 审核驳回: the three-branch reject. Only an approved, still-
    /// `status=0` order refunds its principal (`lx = 12`, never the fee); a
    /// pending (never-debited) application just flips to rejected; a payout
    /// the platform already moved off `status = 0` is refused. Every reject
    /// terminalises the row (`check_status = 2`, `status = 3`) and is
    /// idempotent — a raced replay resurfaces the rejected row.
    pub async fn df_reject(
        &self,
        order_no: &str,
        reason: &str,
        now_ts: i64,
    ) -> GatewayResult<ReviewOutcome> {
        let order = self
            .find(order_no)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
        let check = CheckStatus::from_code(order.check_status.unwrap_or(-1)).ok_or_else(|| {
            GatewayError::Internal(format!(
                "df_reject: 非代付审核单 check_status={:?}",
                order.check_status
            ))
        })?;
        match reject_plan(check, order.status) {
            RejectPlan::Already => Ok(ReviewOutcome::AlreadyRejected(order)),
            RejectPlan::NotRejectable => Ok(ReviewOutcome::NotRejectable(order)),
            RejectPlan::FlipOnly => {
                // Never debited: a plain check_status 0→2 (+ status 3) flip.
                let cas = self
                    .conn()
                    .execute_raw(Statement::from_sql_and_values(
                        self.conn().get_database_backend(),
                        "UPDATE payout_orders SET check_status = 2, status = 3, \
                         reject_reason = $2, review_time = $3 \
                         WHERE order_no = $1 AND check_status = 0",
                        vec![
                            SeaValue::from(order_no.to_string()),
                            SeaValue::from(reason.to_string()),
                            SeaValue::from(now_ts),
                        ],
                    ))
                    .await
                    .map_err(db_err)?;
                if cas.rows_affected() == 0 {
                    let fresh = self
                        .find(order_no)
                        .await?
                        .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
                    return Ok(ReviewOutcome::AlreadyRejected(fresh));
                }
                let fresh = self
                    .find(order_no)
                    .await?
                    .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
                let balance_after = balance_opt(self.conn(), order.user_id)
                    .await?
                    .unwrap_or_default();
                Ok(ReviewOutcome::Rejected {
                    order: fresh,
                    refund: Refund {
                        principal: 0,
                        fee: 0,
                    },
                    balance_after,
                })
            }
            RejectPlan::RefundPrincipal => self.reject_with_refund(&order, reason, now_ts).await,
        }
    }

    /// §6.5 批量审核通过: the legacy `dfPassBatch` loop — each id is driven
    /// through [`PayoutService::df_pass`] with its OWN transaction (this is
    /// why the batch is a plain sequential loop, not one big tx: a row that
    /// breaches a guard or loses its balance race rolls back alone while the
    /// rest still commit). Returns the 「成功 X 失败 Y」 tally.
    pub async fn df_pass_batch(
        &self,
        order_nos: &[String],
        now_ts: i64,
    ) -> GatewayResult<ReviewBatchReport> {
        let mut rep = ReviewBatchReport::default();
        for order_no in order_nos {
            match self.df_pass(order_no, now_ts).await {
                Ok(outcome) => rep.succeeded.push((order_no.clone(), outcome)),
                Err(e) => rep
                    .failures
                    .push((order_no.clone(), e.message().to_string())),
            }
        }
        Ok(rep)
    }

    /// §6.5 批量审核驳回: the legacy `dfRejectBatch` loop — one independent
    /// [`PayoutService::df_reject`] per id, `reason` defaulting to the empty
    /// string the batch form sends (§6.5「reject_reason=''」). Same per-row
    /// commit/rollback semantics and tally as [`PayoutService::df_pass_batch`].
    pub async fn df_reject_batch(
        &self,
        order_nos: &[String],
        reason: &str,
        now_ts: i64,
    ) -> GatewayResult<ReviewBatchReport> {
        let mut rep = ReviewBatchReport::default();
        for order_no in order_nos {
            match self.df_reject(order_no, reason, now_ts).await {
                Ok(outcome) => rep.succeeded.push((order_no.clone(), outcome)),
                Err(e) => rep
                    .failures
                    .push((order_no.clone(), e.message().to_string())),
            }
        }
        Ok(rep)
    }

    /// The approved-and-debited reject: CAS `check_status = 1 AND status = 0
    /// AND df_lock = 0` (an in-flight order under an executor lock is not
    /// rejectable) → refund ONLY the principal (`lx = 12`), never the fee
    /// (§6.4). A lost CAS resurfaces idempotently.
    async fn reject_with_refund(
        &self,
        order: &payout_orders::Model,
        reason: &str,
        now_ts: i64,
    ) -> GatewayResult<ReviewOutcome> {
        let txn = self.conn().begin().await?;
        let cas = txn
            .execute_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "UPDATE payout_orders SET check_status = 2, status = 3, reject_reason = $2, \
                 memo = $2, review_time = $3, settled_at = $3 \
                 WHERE order_no = $1 AND check_status = 1 AND status = 0 AND df_lock = 0",
                vec![
                    SeaValue::from(order.order_no.clone()),
                    SeaValue::from(reason.to_string()),
                    SeaValue::from(now_ts),
                ],
            ))
            .await
            .map_err(db_err)?;
        if cas.rows_affected() == 0 {
            txn.rollback().await?;
            let fresh = self
                .find(&order.order_no)
                .await?
                .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
            return Ok(
                if fresh.check_status == Some(CheckStatus::Rejected.code()) {
                    ReviewOutcome::AlreadyRejected(fresh)
                } else {
                    ReviewOutcome::NotRejectable(fresh)
                },
            );
        }
        // Iron rule #4: the member locks first, then the single refund flow.
        // ONLY the principal returns — the fee is forfeited (§6.4, faithful).
        let after = credit(&txn, "balance", order.user_id, order.tkmoney).await?;
        insert_flow(
            &txn,
            &principal_flow(order, after, Source::PayoutApi),
            Some(format!("dfreject:{}", order.order_no)),
        )
        .await?;
        txn.commit().await?;
        let fresh = self
            .find(&order.order_no)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("代付申请不存在".to_string()))?;
        Ok(ReviewOutcome::Rejected {
            order: fresh,
            refund: Refund {
                principal: order.tkmoney,
                fee: 0,
            },
            balance_after: after,
        })
    }

    async fn by_out_trade_no(
        &self,
        user_id: i64,
        out_trade_no: &str,
    ) -> GatewayResult<Option<payout_orders::Model>> {
        Ok(payout_orders::Entity::find()
            .filter(payout_orders::Column::UserId.eq(user_id))
            .filter(payout_orders::Column::OutTradeNo.eq(out_trade_no))
            .one(self.conn())
            .await?)
    }
}

// --- tx-scoped SQL seams ---------------------------------------------------

/// Inserts the pending application row: `source = 3`, `check_status = 0`,
/// `status = 0`, the requested principal on both `tkmoney` and `money`, the
/// fee left at `0` (resolved at `df_pass`), and NO balance movement.
async fn insert_application<C: ConnectionTrait>(
    conn: &C,
    req: &ApplyPayoutApi<'_>,
) -> Result<payout_orders::Model, sea_orm::DbErr> {
    let now = crate::data::now();
    let bank = &req.bank;
    payout_orders::ActiveModel {
        id: ActiveValue::NotSet,
        order_no: Set(next_order_no()),
        out_trade_no: Set(Some(req.out_trade_no.to_string())),
        user_id: Set(req.user_id),
        source: Set(Source::PayoutApi.code()),
        status: Set(PayoutStatus::Pending.code()),
        check_status: Set(Some(CheckStatus::Pending.code())),
        t: Set(0),
        tkmoney: Set(req.amount),
        sxfmoney: Set(0),
        money: Set(req.amount),
        charge_type: Set(0),
        bankname: Set(bank.bankname.clone()),
        subbranch: Set(bank.subbranch.clone()),
        accountname: Set(bank.accountname.clone()),
        cardnumber: Set(bank.cardnumber.clone()),
        province: Set(bank.province.clone()),
        city: Set(bank.city.clone()),
        additional: Set(req.extends.clone()),
        df_channel_id: ActiveValue::NotSet,
        df_code: ActiveValue::NotSet,
        df_name: ActiveValue::NotSet,
        channel_mch_id: ActiveValue::NotSet,
        cost: Set(0),
        cost_rate: Set(0),
        rate_type: Set(0),
        df_lock: Set(0),
        last_submit_time: Set(0),
        auto_submit_try: Set(0),
        auto_query_num: Set(0),
        is_auto: Set(0),
        reject_reason: ActiveValue::NotSet,
        memo: ActiveValue::NotSet,
        created_at: Set(now),
        review_time: ActiveValue::NotSet,
        settled_at: ActiveValue::NotSet,
    }
    .insert(conn)
    .await
}

/// The df-pass money trail: `lx = 6` principal off `before`, then the chained
/// `lx = 14` fee row when balance-charged (§6.4 / §7.4 — the df-API fee code,
/// distinct from the settlement's `16`).
async fn write_review_debit_flows<C: ConnectionTrait>(
    conn: &C,
    draft: &PayoutDraft,
    order_no: &str,
    before: i64,
) -> GatewayResult<()> {
    let after_principal = before - draft.amounts.tkmoney;
    insert_flow(
        conn,
        &principal_debit_flow(draft.user_id, order_no, before, draft.amounts.tkmoney),
        Some(format!("dfpass:{order_no}")),
    )
    .await?;
    if draft.amounts.balance_debit > draft.amounts.tkmoney {
        insert_flow(
            conn,
            &fee_debit_flow(
                draft.user_id,
                order_no,
                after_principal,
                draft.amounts.fee,
                Source::PayoutApi.fee_debit_lx(),
            ),
            Some(format!("dfpass:{order_no}:fee")),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_batch_ids_splits_on_underscore_dropping_empties() {
        // The legacy `explode('_', $ids)` over a JS `ids.join('_')`, tolerant
        // of a trailing separator and any doubled underscore.
        assert_eq!(
            parse_batch_ids("A1_B2_C3"),
            vec!["A1".to_string(), "B2".to_string(), "C3".to_string()]
        );
        assert_eq!(
            parse_batch_ids("A1_B2_"),
            vec!["A1".to_string(), "B2".to_string()]
        );
        assert_eq!(parse_batch_ids(""), Vec::<String>::new());
        assert_eq!(parse_batch_ids("___"), Vec::<String>::new());
        assert_eq!(parse_batch_ids("solo"), vec!["solo".to_string()]);
    }

    #[test]
    fn batch_report_tallies_and_summaries() {
        let mut rep = ReviewBatchReport::default();
        rep.succeeded.push((
            "A".into(),
            ReviewOutcome::Approved(payout_orders::Model::default()),
        ));
        rep.failures.push(("B".into(), "余额不足！".into()));
        assert_eq!(rep.succeeded_count(), 1);
        assert_eq!(rep.failed_count(), 1);
        assert_eq!(rep.summary(), "成功 1 失败 1");
    }

    #[test]
    fn reject_plan_covers_the_three_legacy_branches() {
        // §6.4 pending application: never debited → flip only, no refund.
        assert_eq!(
            reject_plan(CheckStatus::Pending, PayoutStatus::Pending.code()),
            RejectPlan::FlipOnly
        );
        // Approved and still awaiting execution → refund the principal.
        assert_eq!(
            reject_plan(CheckStatus::Approved, PayoutStatus::Pending.code()),
            RejectPlan::RefundPrincipal
        );
        // Approved but the platform moved it (processing / success / 待确认) →
        // cannot reject.
        for s in [
            PayoutStatus::Processing,
            PayoutStatus::Success,
            PayoutStatus::Unconfirmed,
        ] {
            assert_eq!(
                reject_plan(CheckStatus::Approved, s.code()),
                RejectPlan::NotRejectable
            );
        }
        // Already rejected → idempotent no-op, whatever the status.
        assert_eq!(
            reject_plan(CheckStatus::Rejected, PayoutStatus::Failed.code()),
            RejectPlan::Already
        );
    }
}
