//! The payout-order persistence services (`spec/04` §13.1/§13.3): the
//!落库层 that turns the pure kernels ([`request_withdrawal`], [`fee`],
//! [`state`]) into the atomic writes the legacy spread over five entry
//! points. The merchant settlement line (§3.3 `saveClearing` + §4.2
//! `editStatus`) is wired end-to-end here; the entrusted / payout-API
//! sources reuse the same order rows and transitions.
//!
//! The three legacy concurrency defects this layer closes, in favour
//! (登记在语义差异清单):
//! - §11.2 read-modify-write balance debit → one guarded atomic UPDATE
//!   (`WHERE balance >= debit` — insufficient funds lose the guard, no tx);
//! - §11.3 the two separately-summed `tklist`/`wttklist` daily roll-ups →
//!   ONE aggregate over the unified `payout_orders` table, read inside the
//!   submitting transaction;
//! - §12.6 the find-then-add `out_trade_no` dedup → the partial unique
//!   index `(user_id, out_trade_no)` (a raced duplicate resurfaces the
//!   first row instead of forking a second withdrawal).
//!
//! Faithful quirks kept on purpose: the daily / per-card roll-ups do NOT
//! filter by status — a rejected order still burns its day's quota (§3.3
//! step 9 counts what the legacy counted).

use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set,
    Statement, TransactionTrait, Value as SeaValue,
};

use crate::data::payout_orders;
use crate::ledger::db::{credit, insert_flow, is_unique_violation};
use crate::ledger::snapshot::FlowIntent;
use crate::state::{db_err, GatewayError, GatewayResult};

use super::state::{refund_amounts, rejectable, PayoutStatus, Refund, Source};
use super::{request_withdrawal, DailyState, PayoutDraft};

/// The payee bank columns snapshotted onto the order (§3.3 `$data`; the
/// `pay_bankcard` entity is not modelled yet, so the caller passes them).
#[derive(Debug, Clone, Default)]
pub struct BankSnapshot {
    pub bankname: Option<String>,
    pub subbranch: Option<String>,
    pub accountname: Option<String>,
    pub cardnumber: Option<String>,
    pub province: Option<String>,
    pub city: Option<String>,
}

/// One withdrawal submission (money units; `out_trade_no` is the optional
/// idempotency key — own-panel withdrawals carry none).
#[derive(Debug, Clone)]
pub struct SubmitWithdrawal<'a> {
    pub user_id: i64,
    /// The requested principal `tkmoney`, money units.
    pub amount: i64,
    pub out_trade_no: Option<&'a str>,
    pub bank: BankSnapshot,
}

/// A payout order accepted and fully booked. The flows chain continuously
/// off `balance_after` (fixing §3.3's `gmoney` double-deduct on the fee row).
#[derive(Debug, Clone)]
pub struct Submitted {
    pub order: payout_orders::Model,
    /// The merchant's post-debit balance, money units.
    pub balance_after: i64,
}

/// The outcome of a `mark_paid` CAS.
#[derive(Debug, Clone)]
pub enum PaidOutcome {
    /// The row moved 0/1 → 2 and `settled_at` was (re)stamped.
    Transitioned(payout_orders::Model),
    /// Already paid — the stored `settled_at` is untouched.
    AlreadyPaid(payout_orders::Model),
    /// Status is not a pre-pay state (pending/processing) — no write.
    Rejected(payout_orders::Model),
}

/// The outcome of a [`PayoutService::reject`].
#[derive(Debug, Clone)]
pub enum RejectOutcome {
    /// Status CAS won; the refund lands in the same tx.
    Refunded {
        order: payout_orders::Model,
        refund: Refund,
        /// The merchant's post-refund balance, money units.
        balance_after: i64,
    },
    /// A rejected refund is idempotent: replaying it returns the row.
    AlreadyRejected(payout_orders::Model),
    /// The reject guard (§4.2 tklist only from 0, §4.3 from 0/1/4) refused.
    NotRejectable(payout_orders::Model),
}

/// The payout-order service over the shared connection.
#[derive(Debug, Clone)]
pub struct PayoutService {
    db: sea_orm::DatabaseConnection,
}

impl PayoutService {
    pub fn new(db: sea_orm::DatabaseConnection) -> Self {
        Self { db }
    }

    /// The shared connection, for the sibling [`super::exec`] queue impl
    /// (keeps the write field private to this module).
    pub(crate) fn conn(&self) -> &sea_orm::DatabaseConnection {
        &self.db
    }

    /// Load one order by its platform order no.
    pub async fn find(&self, order_no: &str) -> GatewayResult<Option<payout_orders::Model>> {
        Ok(payout_orders::Entity::find()
            .filter(payout_orders::Column::OrderNo.eq(order_no))
            .one(&self.db)
            .await?)
    }

    /// The §3.3 `saveClearing` submission as ONE transaction: roll-up reads,
    /// the full guard chain, the guarded atomic debit, the order row and the
    /// lx=6 (+ balance-charged lx=16) flows. Any breach rolls everything
    /// back — the legacy's non-atomic read-then-save (§11.2) can never
    /// strand a debit without its order.
    pub async fn submit_withdrawal(
        &self,
        req: &SubmitWithdrawal<'_>,
        now_ts: i64,
    ) -> GatewayResult<Submitted> {
        let txn = self.db.begin().await?;

        let hint = balance_opt(&txn, req.user_id).await?.ok_or_else(|| {
            GatewayError::Internal(format!("payout: member {} missing", req.user_id))
        })?;

        // Idempotency read rides the same tx; the partial unique index below
        // is the net for the racing twin (§12.6) — a duplicate insert
        // resurfaces the first row instead of forking a second withdrawal.
        if let Some(out) = &req.out_trade_no {
            if let Some(existing) = payout_orders::Entity::find()
                .filter(payout_orders::Column::UserId.eq(req.user_id))
                .filter(payout_orders::Column::OutTradeNo.eq(*out))
                .one(&txn)
                .await?
            {
                txn.rollback().await?;
                return Ok(Submitted {
                    order: existing,
                    balance_after: hint,
                });
            }
        }

        // The legacy aggregates every payout order of the day, whatever its
        // status — rejected rows keep burning the quota (§3.3 step 9).
        let daily = daily_rollup(&txn, req.user_id, now_ts).await?;
        let card_today_sum = match &req.bank.cardnumber {
            Some(card) => card_rollup(&txn, req.user_id, card, now_ts).await?,
            None => 0,
        };

        // Pure guard chain + fee math (config resolution included — the
        // whole submission is one tx, so the reads ride it).
        let draft = request_withdrawal(
            &txn,
            req.user_id,
            req.amount,
            hint,
            card_today_sum,
            &daily,
            now_ts,
        )
        .await?;

        let debit = draft.amounts.balance_debit;
        // Iron rule #1: the debit is the guarded atomic UPDATE — a vanished
        // or raced-away balance loses the `balance >= $1` guard (0 rows).
        let after = match debit_guarded(&txn, req.user_id, debit).await? {
            Some(after) => after,
            None => {
                txn.rollback().await?;
                return Err(GatewayError::BadRequest("余额不足！".to_string()));
            }
        };

        let order = match insert_order(&txn, &draft, req).await {
            Ok(order) => order,
            Err(e) if is_unique_violation(&e) => {
                // The racing twin under the same out_trade_no already booked;
                // the tx is aborted, so roll back and resurface that row.
                txn.rollback().await?;
                if let Some(out) = &req.out_trade_no {
                    if let Some(existing) = payout_orders::Entity::find()
                        .filter(payout_orders::Column::UserId.eq(req.user_id))
                        .filter(payout_orders::Column::OutTradeNo.eq(*out))
                        .one(&self.db)
                        .await?
                    {
                        return Ok(Submitted {
                            order: existing,
                            balance_after: hint,
                        });
                    }
                }
                return Err(GatewayError::BadRequest("提单已存在".to_string()));
            }
            Err(e) => return Err(db_err(e)),
        };

        // Iron rule #2: the flows ride the same tx, chained continuously off
        // the RETURNING balance (§3.3 charge-row gmoney bug fixed in favour).
        let before = after + debit;
        write_submit_flows(&txn, &draft, &order.order_no, before).await?;

        txn.commit().await?;
        Ok(Submitted {
            order,
            balance_after: after,
        })
    }

    /// Records the actual bank transfer: `0/1 → 2` CAS + `settled_at`
    /// (§4.2 case 2 stamps `cldatetime`; replaying a paid order never
    /// re-stamps it).
    pub async fn mark_paid(&self, order_no: &str) -> GatewayResult<PaidOutcome> {
        let res = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE payout_orders SET status = 2, settled_at = $2 \
                 WHERE order_no = $1 AND status IN (0, 1)",
                [
                    SeaValue::from(order_no.to_string()),
                    SeaValue::from(crate::data::now_ts()),
                ],
            ))
            .await?;
        if res.rows_affected() == 1 {
            return Ok(PaidOutcome::Transitioned(self.required(order_no).await?));
        }
        let order = self.required(order_no).await?;
        if PayoutStatus::from_code(order.status) == Some(PayoutStatus::Success) {
            return Ok(PaidOutcome::AlreadyPaid(order));
        }
        Ok(PaidOutcome::Rejected(order))
    }

    /// §4.2 / §4.3 驳回: the source-aware rejectability guard, the status → 3
    /// CAS, and the balance refund (principal lx=11/12, plus lx=17/15 when
    /// the fee was balance-charged) in one tx. A raced double-reject loses
    /// the CAS and reads back the already-rejected row idempotently.
    pub async fn reject(
        &self,
        order_no: &str,
        reason: Option<&str>,
    ) -> GatewayResult<RejectOutcome> {
        let order = self.required(order_no).await?;
        let source = Source::from_code(order.source).ok_or_else(|| {
            GatewayError::Internal(format!("payout: bad source {}", order.source))
        })?;
        let status = PayoutStatus::from_code(order.status).ok_or_else(|| {
            GatewayError::Internal(format!("payout: bad status {}", order.status))
        })?;
        if !rejectable(source, status) {
            return Ok(RejectOutcome::NotRejectable(order));
        }

        let txn = self.db.begin().await?;
        let cas = txn
            .execute_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "UPDATE payout_orders SET status = 3, settled_at = $2, reject_reason = $3 \
                 WHERE order_no = $1 AND status = $4",
                [
                    SeaValue::from(order_no.to_string()),
                    SeaValue::from(crate::data::now_ts()),
                    SeaValue::from(reason.map(str::to_string)),
                    SeaValue::from(order.status),
                ],
            ))
            .await?;
        if cas.rows_affected() == 0 {
            txn.rollback().await?;
            let fresh = self.required(order_no).await?;
            return Ok(if fresh.status == PayoutStatus::Failed.code() {
                RejectOutcome::AlreadyRejected(fresh)
            } else {
                RejectOutcome::NotRejectable(fresh)
            });
        }

        let refund = refund_amounts(order.charge_type == 1, order.tkmoney, order.sxfmoney);
        // Iron rule #4: the member row locks first, then the flows. Principal
        // (§4.2 lx=11) and the balance-charged fee (lx=17) are TWO credits,
        // chained — never a single total, so each flow row snapshots its own
        // before/after exactly like the legacy's two saves.
        let after_principal = credit(&txn, "balance", order.user_id, refund.principal).await?;
        insert_flow(
            &txn,
            &principal_flow(&order, after_principal, source),
            Some(format!("refund:{}", order.order_no)),
        )
        .await?;
        let balance_after = if refund.fee > 0 {
            let after = credit(&txn, "balance", order.user_id, refund.fee).await?;
            insert_flow(
                &txn,
                &fee_flow(&order, after, source),
                Some(format!("refund:{}:fee", order.order_no)),
            )
            .await?;
            after
        } else {
            after_principal
        };

        txn.commit().await?;
        let order = self.required(order_no).await?;
        Ok(RejectOutcome::Refunded {
            order,
            refund,
            balance_after,
        })
    }

    async fn required(&self, order_no: &str) -> GatewayResult<payout_orders::Model> {
        self.find(order_no)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("提现单不存在".to_string()))
    }
}

// --- pure helpers (offline-unit-tested) -----------------------------------

/// The legacy `WithdrawalController::getOrderId` format (§13.1 `order_no`):
/// year letter (`2026 → Q`) + `md` + last 5 unix digits + 5 micro digits +
/// 2 zero-padded random digits. Uniqueness is the DB index's job — the
/// microsecond slice makes collisions vanishing, the letter overflows before
/// 2037 but that reads as an internal error, never a wrong key.
pub fn gen_order_no(now: &chrono::DateTime<chrono::Local>, micros: u32, rand: u32) -> String {
    use chrono::Datelike;
    let year_code = b"ABCDEFGHIJKLMNOPQRSTUVXYZ";
    let index = usize::try_from(now.year() - 2011).unwrap_or(usize::MAX);
    let letter = char::from(*year_code.get(index).expect("order_no year beyond 2037"));
    let ts = now.timestamp();
    format!(
        "{letter}{:02}{:02}{:0>5}{:0>5}{:0>2}",
        now.month(),
        now.day(),
        ts % 100_000,
        micros % 100_000,
        rand % 100,
    )
}

/// The lx=6 withdrawal principal-debit flow: the merchant's balance drops by
/// `tkmoney` (legacy 「提现操作」). Our ledger stores signed deltas, so the
/// chain is `before → -tkmoney → before - tkmoney`.
pub fn principal_debit_flow(user_id: i64, order_no: &str, before: i64, tkmoney: i64) -> FlowIntent {
    FlowIntent {
        user_id,
        y_money: before,
        money: -tkmoney,
        g_money: before - tkmoney,
        lx: 6,
        trans_id: Some(order_no.to_string()),
        order_id: Some(order_no.to_string()),
    }
}

/// The balance-charged fee flow (`tk_charge_type = 1`), chained right behind
/// the principal debit off `after_principal` (fixing §3.3's `gmoney`
/// double-deduct). `lx` is source-specific ([`Source::fee_debit_lx`]): `16`
/// 「手动结算扣除手续费」 on the settlement line, `14`「委托提现扣除手续费」
/// on the df-API review (`df_pass`).
pub fn fee_debit_flow(
    user_id: i64,
    order_no: &str,
    after_principal: i64,
    fee: i64,
    lx: i32,
) -> FlowIntent {
    FlowIntent {
        user_id,
        y_money: after_principal,
        money: -fee,
        g_money: after_principal - fee,
        lx,
        trans_id: Some(order_no.to_string()),
        order_id: Some(order_no.to_string()),
    }
}

/// The reject principal-refund flow (lx 11 settlement / 12 payout, §4.2/§4.3).
pub fn principal_flow(order: &payout_orders::Model, after: i64, source: Source) -> FlowIntent {
    refund_flow(
        order.user_id,
        &order.order_no,
        order.tkmoney,
        after,
        source.principal_lx(),
    )
}

/// The reject fee-refund flow (lx 17 / 15), only ever written when the fee
/// was balance-charged (§4.2 「手动结算驳回退回手续费」).
pub fn fee_flow(order: &payout_orders::Model, after: i64, source: Source) -> FlowIntent {
    refund_flow(
        order.user_id,
        &order.order_no,
        order.sxfmoney,
        after,
        source.fee_lx(),
    )
}

fn refund_flow(user_id: i64, order_no: &str, amount: i64, after: i64, lx: i32) -> FlowIntent {
    FlowIntent {
        user_id,
        y_money: after - amount,
        money: amount,
        g_money: after,
        lx,
        trans_id: Some(order_no.to_string()),
        order_id: Some(order_no.to_string()),
    }
}

// --- tx-scoped SQL seams ---------------------------------------------------

pub(crate) async fn balance_opt<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
) -> GatewayResult<Option<i64>> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "SELECT balance FROM members WHERE id = $1",
        [SeaValue::from(user_id)],
    );
    let row = conn.query_one_raw(stmt).await.map_err(db_err)?;
    Ok(match row {
        Some(r) => Some(r.try_get::<i64>("", "balance").map_err(db_err)?),
        None => None,
    })
}

/// `UPDATE members SET balance = balance - $1 WHERE id = $2 AND balance >= $1
/// RETURNING balance` — the §11.2 fix; `Ok(None)` = guard lost (insufficient
/// funds or the member vanished).
pub(crate) async fn debit_guarded<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
    debit: i64,
) -> GatewayResult<Option<i64>> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "UPDATE members SET balance = balance - $1 WHERE id = $2 AND balance >= $1 \
         RETURNING balance",
        [SeaValue::from(debit), SeaValue::from(user_id)],
    );
    let row = conn.query_one_raw(stmt).await.map_err(db_err)?;
    Ok(match row {
        Some(r) => Some(r.try_get::<i64>("", "balance").map_err(db_err)?),
        None => None,
    })
}

/// The ONE-statement day roll-up (legacy summed `tklist` + `wttklist`
/// separately, §11.3); no status filter — faithful to the legacy.
pub(crate) async fn daily_rollup<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
    now_ts: i64,
) -> GatewayResult<DailyState> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "SELECT COUNT(*) AS cnt, COALESCE(SUM(tkmoney), 0)::bigint AS total FROM payout_orders \
         WHERE user_id = $1 AND created_at >= to_timestamp($2) AT TIME ZONE 'UTC'",
        [
            SeaValue::from(user_id),
            SeaValue::from(today_start_ts(now_ts)),
        ],
    );
    let row = conn.query_one_raw(stmt).await.map_err(db_err)?;
    Ok(match row {
        Some(r) => DailyState {
            today_count: r.try_get::<i64>("", "cnt").map_err(db_err)?,
            today_sum: r.try_get::<i64>("", "total").map_err(db_err)?,
        },
        None => DailyState::default(),
    })
}

/// The per-card day roll-up (`cardnumber` snapshot column), same no-status-
/// filter semantics as [`daily_rollup`].
pub(crate) async fn card_rollup<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
    cardnumber: &str,
    now_ts: i64,
) -> GatewayResult<i64> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        "SELECT COALESCE(SUM(tkmoney), 0)::bigint AS total FROM payout_orders \
         WHERE user_id = $1 AND cardnumber = $2 \
         AND created_at >= to_timestamp($3) AT TIME ZONE 'UTC'",
        [
            SeaValue::from(user_id),
            SeaValue::from(cardnumber.to_string()),
            SeaValue::from(today_start_ts(now_ts)),
        ],
    );
    let row = conn.query_one_raw(stmt).await.map_err(db_err)?;
    Ok(match row {
        Some(r) => r.try_get::<i64>("", "total").map_err(db_err)?,
        None => 0,
    })
}

async fn insert_order<C: ConnectionTrait>(
    conn: &C,
    draft: &PayoutDraft,
    req: &SubmitWithdrawal<'_>,
) -> Result<payout_orders::Model, sea_orm::DbErr> {
    let now = crate::data::now();
    let order_no = next_order_no();
    let charge_type = i32::from(draft.amounts.balance_debit != draft.amounts.tkmoney);
    let bank = &req.bank;
    payout_orders::ActiveModel {
        id: ActiveValue::NotSet,
        order_no: Set(order_no),
        out_trade_no: Set(req.out_trade_no.map(str::to_string)),
        user_id: Set(req.user_id),
        source: Set(Source::Settlement.code()),
        status: Set(PayoutStatus::Pending.code()),
        check_status: ActiveValue::NotSet,
        t: Set(draft.t),
        tkmoney: Set(draft.amounts.tkmoney),
        sxfmoney: Set(draft.amounts.fee),
        money: Set(draft.amounts.arrival),
        charge_type: Set(charge_type),
        bankname: Set(bank.bankname.clone()),
        subbranch: Set(bank.subbranch.clone()),
        accountname: Set(bank.accountname.clone()),
        cardnumber: Set(bank.cardnumber.clone()),
        province: Set(bank.province.clone()),
        city: Set(bank.city.clone()),
        additional: ActiveValue::NotSet,
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

/// The submit-side money trail: lx=6 principal off `before`, then the chained
/// lx=16 fee row when the fee was balance-charged (`balance_debit > tkmoney`).
async fn write_submit_flows<C: ConnectionTrait>(
    conn: &C,
    draft: &PayoutDraft,
    order_no: &str,
    before: i64,
) -> GatewayResult<()> {
    let after_principal = before - draft.amounts.tkmoney;
    insert_flow(
        conn,
        &principal_debit_flow(draft.user_id, order_no, before, draft.amounts.tkmoney),
        Some(format!("withdraw:{order_no}")),
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
                Source::Settlement.fee_debit_lx(),
            ),
            Some(format!("withdraw:{order_no}:fee")),
        )
        .await?;
    }
    Ok(())
}

/// The day's start (local, mirroring legacy `date('Y-m-d')` comparisons).
fn today_start_ts(now_ts: i64) -> i64 {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_opt(now_ts, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap());
    let day = dt.date_naive().and_hms_opt(0, 0, 0).expect("midnight");
    day.and_local_timezone(Local)
        .single()
        .expect("local midnight")
        .timestamp()
}

/// Fresh order no from the wall clock + microseconds + one random draw
/// (the `getOrderId` triple); the unique index is the arbiter.
pub fn next_order_no() -> String {
    use rand::Rng;
    let now = chrono::Local::now();
    let micros = now.timestamp_subsec_micros() % 100_000;
    let rand: u32 = rand::rng().random_range(1..=99);
    gen_order_no(&now, micros, rand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    const K: i64 = 10_000; // 1 元

    fn local(y: i32, mo: u32, d: u32, h: u32) -> chrono::DateTime<chrono::Local> {
        Local.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
    }

    #[test]
    fn order_no_replays_the_legacy_shape() {
        // 2026 → index 15 → 'P' (year_code[intval(Y) - 2010 - 1]).
        let now = local(2026, 9, 22, 10);
        let no = gen_order_no(&now, 12345, 7);
        assert!(no.starts_with('P'), "{no}");
        // P + md(0922) + last-5 ts + 5 micro + 2 rand.
        assert_eq!(&no[1..5], "0922");
        let ts5 = format!("{:0>5}", now.timestamp() % 100_000);
        assert_eq!(&no[5..10], ts5);
        assert_eq!(&no[10..15], "12345");
        assert_eq!(&no[15..], "07");
        assert_eq!(no.len(), 17);
    }

    #[test]
    fn order_no_micro_and_rand_are_zero_padded() {
        let no = gen_order_no(&local(2025, 1, 5, 0), 3, 99);
        assert!(no.starts_with('O'), "{no}"); // 2025 - 2011 = 14 → 'O'
        assert_eq!(&no[10..15], "00003");
        assert_eq!(&no[15..], "99");
    }

    #[test]
    fn submit_flows_chain_continuously() {
        // Balance-charged 100元 + 2元 fee off a 300元 balance: the two rows
        // chain 300→200 (lx 6) then 200→198 (lx 16) — §3.3's gmoney
        // double-deduct never re-subtracts the fee.
        let p = principal_debit_flow(7, "Q0922001230000101", 300 * K, 100 * K);
        assert_eq!(
            (p.y_money, p.money, p.g_money, p.lx),
            (300 * K, -100 * K, 200 * K, 6)
        );
        let f = fee_debit_flow(
            7,
            "Q0922001230000101",
            p.g_money,
            2 * K,
            Source::Settlement.fee_debit_lx(),
        );
        assert_eq!(
            (f.y_money, f.money, f.g_money, f.lx),
            (200 * K, -2 * K, 198 * K, 16)
        );
        // The df-API review charges the same math under lx=14 instead.
        assert_eq!(
            fee_debit_flow(7, "X", p.g_money, 2 * K, Source::PayoutApi.fee_debit_lx()).lx,
            14
        );
        assert_eq!(f.g_money, 300 * K - (100 * K + 2 * K)); // == post-debit balance
    }

    #[test]
    fn refund_flows_split_by_charge_mode() {
        let order = payout_orders::Model {
            id: 1,
            order_no: "Q0922001230000101".into(),
            user_id: 7,
            tkmoney: 100 * K,
            sxfmoney: 2 * K,
            ..payout_order_fixture()
        };
        // Settlement 驳回: principal lx=11 chains to the credited balance,
        // fee lx=17 rides behind it (§4.2).
        let p = principal_flow(&order, 110 * K, Source::Settlement);
        assert_eq!(
            (p.lx, p.money, p.y_money, p.g_money),
            (11, 100 * K, 10 * K, 110 * K)
        );
        let f = fee_flow(&order, 112 * K, Source::Settlement);
        assert_eq!((f.lx, f.money, f.g_money), (17, 2 * K, 112 * K));
        // Payout sources carry the 12/15 codes instead (§4.3).
        assert_eq!(principal_flow(&order, 110 * K, Source::PayoutApi).lx, 12);
        assert_eq!(fee_flow(&order, 112 * K, Source::Entrusted).lx, 15);
    }

    fn payout_order_fixture() -> payout_orders::Model {
        payout_orders::Model {
            id: 0,
            order_no: String::new(),
            out_trade_no: None,
            user_id: 0,
            source: 1,
            status: 0,
            check_status: None,
            t: 1,
            tkmoney: 0,
            sxfmoney: 0,
            money: 0,
            charge_type: 0,
            bankname: None,
            subbranch: None,
            accountname: None,
            cardnumber: None,
            province: None,
            city: None,
            additional: None,
            df_channel_id: None,
            df_code: None,
            df_name: None,
            channel_mch_id: None,
            cost: 0,
            cost_rate: 0,
            rate_type: 0,
            df_lock: 0,
            last_submit_time: 0,
            auto_submit_try: 0,
            auto_query_num: 0,
            is_auto: 0,
            reject_reason: None,
            memo: None,
            created_at: chrono::NaiveDateTime::default(),
            review_time: None,
            settled_at: None,
        }
    }

    #[test]
    fn source_codes_roundtrip() {
        for s in [Source::Settlement, Source::Entrusted, Source::PayoutApi] {
            assert_eq!(Source::from_code(s.code()), Some(s));
        }
        assert_eq!(Source::from_code(9), None);
    }
}
