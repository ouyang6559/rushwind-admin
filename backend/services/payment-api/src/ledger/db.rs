//! The DB seams of [`LedgerService`] — the Phase-3 wiring that replays the
//! pure kernels ([`crate::ledger::admit`], [`crate::ledger::settle`],
//! [`crate::ledger::thaw`]) as conditional UPDATEs + atomic balance SQL +
//! INSERTs under the four iron rules (`spec/02-funds-order.md` §11.1):
//!
//! 1. every balance change is one atomic statement
//!    (`SET balance = balance + $x`, debits guarded by `WHERE blocked_balance >= $x`)
//!    and returns the post-write balance via `RETURNING` — the flow row's
//!    `y_money`/`g_money` are derived from that authoritative value, so the
//!    snapshots recorded are exact even when the kernel planned from a
//!    non-locked hint read (fixing legacy 缺陷 P:618 「上级未加锁读」in favour);
//! 2. every balance change writes its `money_changes` row in the SAME tx,
//!    keyed by a unique `request_id` (new in the rewrite — legacy had none,
//!    §3.4 幂等);
//! 3. every order transition is a CAS (`WHERE order_id = $1 AND status = $2`);
//!    a lost race rolls the whole tx back and surfaces as
//!    [`SettleOutcome::AlreadySettled`] / [`false`], never a double credit;
//! 4. the lock order is Member → Order → ancestors: the merchant's credit
//!    UPDATE is the first statement in a settle tx (it row-locks the member),
//!    the order CAS follows, ancestor credits come last.
//!
//! The complaints-deposit rule (`spec/02` §4.4) is fully wired: settle
//! resolves the merchant's rule row (own active row ?: the `is_system`
//! fallback), withholds inside the pure kernel, and lands the
//! `complaints_deposits` freeze row in the SAME settle tx. Per-adapter notify
//! signature verification rides the channel-adapter wiring, not this file.

use sea_orm::{
    sea_query::Value, ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};

use crate::data::{
    blocked_logs, channels, complaints_deposit_rules, complaints_deposits, members, money_changes,
    orders, redo_orders,
};
use crate::rate::{self, ChannelRate, Cycle, ResolvedRate};
use crate::state::{GatewayError, GatewayResult};

use super::admit::{self, AdmitRequest};
use super::profit::DEFAULT_MAX_LEVELS;
use super::settle::{self, AgentLevel, MerchantBuckets, SettleInput, SettleOutcome, SettleWrites};
use super::snapshot::{self, DepositRule, FlowIntent};
use super::state::PayStatus;
use super::thaw::{self, ThawBuckets, ThawKind, ThawResult};
use super::LedgerService;

/// Everything one `orderadd` INSERT needs: the admission inputs plus the
/// frozen identity the gateway resolved (merchant, product, routed account,
/// signing snapshots). [`LedgerService::create_order`] runs the pure
/// [`admit::admit`] kernel itself and stores the resulting snapshot.
#[derive(Debug, Clone)]
pub struct NewOrder {
    /// The merchant (member id, NOT the wire mch id).
    pub user_id: i64,
    /// Wire `pay_orderid` — the unique order key (§3.4).
    pub order_id: String,
    /// `pay_amount` in money units.
    pub amount_units: i64,
    /// The effective rate snapshot for the order's cycle (§3.2).
    pub rate: ResolvedRate,
    /// The cycle-selected channel cost rate (§3.3 #4).
    pub cost_rate: i64,
    /// The settlement cycle frozen from `tikuanconfig.t1zt` (§3.2).
    pub t: i32,
    /// Wire `pay_bankcode` (the product id, §5.2 pid 口径).
    pub bank_code: String,
    /// The routed channel's code (legacy `pay_tongdao`, frozen on the row).
    pub channel_code: String,
    pub notify_url: String,
    pub callback_url: String,
    pub channel_id: i64,
    pub account_id: i64,
    /// The channel/sub-account signing key snapshot (orderadd `key`).
    pub sign_key: Option<String>,
    /// The sub-account appid snapshot (orderadd `account`).
    pub app_id: Option<String>,
    pub attach: Option<String>,
    pub product_name: Option<String>,
}

impl LedgerService {
    /// Order creation: pure admission decides acceptance and the frozen
    /// amounts; this persists the `status = 0` row. The unique `order_id`
    /// index is the idempotency net — a repeat surfaces as the caller-facing
    /// 「订单已存在」(legacy exposed a 500 系统错误 instead, §3.4).
    pub async fn create_order(&self, new: &NewOrder) -> GatewayResult<orders::Model> {
        let req = AdmitRequest {
            amount_units: new.amount_units,
            rate: &new.rate,
            cost_rate: new.cost_rate,
            t: new.t,
        };
        let amounts = admit::admit(&req).map_err(GatewayError::from)?;
        let row = orders::ActiveModel {
            mch_id: Set(crate::merchant::mch_id_of(new.user_id).to_string()),
            order_id: Set(new.order_id.clone()),
            amount: Set(amounts.amount),
            poundage: Set(amounts.poundage),
            actual_amount: Set(amounts.actual_amount),
            cost: Set(amounts.cost),
            apply_date: Set(crate::data::now_ts()),
            bank_code: Set(new.bank_code.clone()),
            channel_code: Set(Some(new.channel_code.clone())),
            // The rewrite's unique order id IS the merchant's own number, so
            // the legacy `out_trade_id` column carries it verbatim (§3.4).
            out_trade_id: Set(Some(new.order_id.clone())),
            notify_url: Set(new.notify_url.clone()),
            callback_url: Set(new.callback_url.clone()),
            status: Set(PayStatus::Unpaid.code()),
            user_id: Set(new.user_id),
            channel_id: Set(new.channel_id),
            account_id: Set(new.account_id),
            t: Set(new.t),
            lock_status: Set(super::state::LOCK_NONE),
            num: Set(0),
            last_reissue_time: Set(0),
            sign_key: Set(new.sign_key.clone()),
            account: Set(new.app_id.clone()),
            attach: Set(new.attach.clone()),
            product_name: Set(new.product_name.clone()),
            ..Default::default()
        };
        row.insert(&self.db).await.map_err(|e| {
            if is_unique_violation(&e) {
                GatewayError::BadRequest("订单已存在".into())
            } else {
                crate::state::db_err(e)
            }
        })
    }

    /// Load one order by its unique id (query endpoint support).
    pub async fn find_order(&self, order_id: &str) -> GatewayResult<Option<orders::Model>> {
        Ok(orders::Entity::find()
            .filter(orders::Column::OrderId.eq(order_id))
            .one(&self.db)
            .await?)
    }

    /// Unified settle for callback / reissue (`spec/02` §4). Assembles
    /// [`SettleInput`] from the frozen order row + live balances, runs the
    /// pure [`settle::settle`] kernel, then replays [`SettleWrites`] in one
    /// transaction. A duplicate / raced callback rolls back and returns
    /// [`SettleOutcome::AlreadySettled`] with zero accounting.
    ///
    /// The brokerage rates are re-read live via the §5.2 priority chain
    /// (merchant `userrate` ?: channel default, on the frozen `channel_id`
    /// — the pid-口径 unified per §5.2's refactor note), matching legacy
    /// `huoqufeilv`; amounts themselves come from the FROZEN order row, so a
    /// rate edit between order and settle never rewrites the arrival.
    pub async fn settle_order(&self, order_id: &str) -> GatewayResult<SettleOutcome> {
        let order = self
            .find_order(order_id)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("订单不存在".into()))?;

        // Plan-assembly reads (hints; replay overrides them from RETURNING).
        let merchant = member(&self.db, order.user_id).await?.ok_or_else(|| {
            GatewayError::Internal(format!("settle: merchant {} missing", order.user_id))
        })?;
        let channel = channel_rate(&self.db, order.channel_id).await?;
        let cycle = Cycle::from_t(order.t);
        let chain = build_chain(&self.db, &order, &merchant, &channel, cycle).await?;
        // The complaints-deposit rule (`spec/02` §4.4): the merchant's own
        // ACTIVE row wins, else the platform `is_system` row decides. The
        // release schedule (`now + freeze_time`) is frozen here like the
        // legacy's `time() + rule.freeze_time` write.
        let now_ts = crate::data::now_ts();
        let deposit_cfg = load_deposit_rule(&self.db, order.user_id).await?;
        let deposit_rule = deposit_cfg
            .as_ref()
            .map(|c| DepositRule {
                active: true,
                ratio_pct: c.ratio_pct,
            })
            .unwrap_or(DepositRule::NONE);
        let deposit_unfreeze_at = deposit_cfg
            .as_ref()
            .map(|c| now_ts + c.freeze_time)
            .unwrap_or(0);
        let input = SettleInput {
            current: PayStatus::from_code(order.status).ok_or_else(|| {
                GatewayError::Internal(format!("settle: bad status {}", order.status))
            })?,
            order_amount: order.amount,
            actual_before: order.actual_amount,
            t: order.t,
            deposit_rule: &deposit_rule,
            merchant_user_id: order.user_id,
            merchant_buckets: MerchantBuckets {
                available: merchant.balance,
                blocked: merchant.blocked_balance,
            },
            chain: &chain,
            blocked_thaw_at: t1_thaw_ts(),
            deposit_unfreeze_at,
            trans_id: Some(order.order_id.clone()),
            out_order_id: Some(order.order_id.clone()),
        };
        let outcome = settle::settle(&input)
            .map_err(|e| GatewayError::Internal(format!("ledger.settle: {e:?}")))?;
        let writes = match outcome {
            SettleOutcome::AlreadySettled => return Ok(SettleOutcome::AlreadySettled),
            SettleOutcome::Settled(w) => w,
        };

        let txn = self.db.begin().await?;

        // Iron rule #4: the merchant credit first locks the Member row;
        // iron rule #1: it is one atomic UPDATE ... RETURNING.
        let net = writes.merchant_flow.money;
        let blocked = writes.blocked_log.is_some();
        let after = if blocked {
            credit(&txn, "blocked_balance", order.user_id, net).await?
        } else {
            credit(&txn, "balance", order.user_id, net).await?
        };

        // Iron rule #3: the 0 -> 1 CAS is the入账 dedupe; losing it rolls the
        // credit back and the callback is a no-op on money.
        let cas = txn
            .execute_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "UPDATE orders SET status = 1, success_date = $2 \
                 WHERE order_id = $1 AND status = 0",
                vec![
                    Value::from(order.order_id.clone()),
                    Value::from(crate::data::now_ts()),
                ],
            ))
            .await?;
        if cas.rows_affected() == 0 {
            txn.rollback().await?;
            return Ok(SettleOutcome::AlreadySettled);
        }

        // Iron rule #2: flows (with RETURNING-exact snapshots) in the same tx.
        let merchant_flow = FlowIntent {
            y_money: after - net,
            g_money: after,
            ..writes.merchant_flow.clone()
        };
        insert_flow(
            &txn,
            &merchant_flow,
            Some(format!("settle:{}", order.order_id)),
        )
        .await?;

        if let Some(log) = &writes.blocked_log {
            blocked_logs::ActiveModel {
                user_id: Set(log.user_id),
                order_id: Set(log.order_id.clone()),
                amount: Set(log.amount),
                status: Set(0),
                thaw_time: Set(log.thaw_at),
                create_time: Set(crate::data::now_ts()),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(crate::state::db_err)?;
        }

        // The complaints-deposit freeze ledger row (`spec/02` §4.4, PM:126-142
        // / P:338-353): the withheld money left the arrival (the credit above
        // is the NET), it lives only in this ledger until its scheduled
        // release (`run_deposit_unfreeze`).
        if let Some(d) = &writes.deposit {
            complaints_deposits::ActiveModel {
                user_id: Set(order.user_id),
                pay_orderid: Set(order.order_id.clone()),
                out_trade_id: Set(order.order_id.clone()),
                freeze_money: Set(d.amount),
                unfreeze_time: Set(d.unfreeze_at),
                real_unfreeze_time: Set(0),
                is_pause: Set(0),
                status: Set(0),
                create_at: Set(now_ts),
                update_at: Set(now_ts),
                ..Default::default()
            }
            .insert(&txn)
            .await
            .map_err(crate::state::db_err)?;
        }

        // Ancestor credits last (they are separate members; the merchant and
        // order are already locked in this tx's order).
        let mut brokerage_flows = Vec::with_capacity(writes.brokerage_flows.len());
        for bf in &writes.brokerage_flows {
            let after = credit(&txn, "balance", bf.user_id, bf.money).await?;
            let row = FlowIntent {
                y_money: after - bf.money,
                g_money: after,
                ..bf.clone()
            };
            insert_flow(
                &txn,
                &row,
                Some(format!("profit:{}:{}", order.order_id, bf.user_id)),
            )
            .await?;
            brokerage_flows.push(row);
        }

        txn.commit().await?;
        Ok(SettleOutcome::Settled(SettleWrites {
            merchant_flow,
            brokerage_flows,
            ..writes
        }))
    }

    /// 1 -> 2 after the merchant's reply carried `"ok"` (§2.1,
    /// [`crate::ledger::state::notify_acked`]). CAS-guarded; `false` means the
    /// order was not in `Paid`.
    pub async fn mark_order_notified(&self, order_id: &str) -> GatewayResult<bool> {
        let res = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE orders SET status = 2 WHERE order_id = $1 AND status = 1",
                vec![Value::from(order_id.to_string())],
            ))
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// The reissue sweep's candidate list (§7.1's `maps`): `Paid` orders
    /// under the attempt cap whose last attempt is older than the 10s gap,
    /// oldest id first, capped at `batch` rows. The returned ids are only
    /// hints — [`Self::claim_reissue`] is what (re)admits each atomically.
    pub async fn due_reissues(
        &self,
        max_attempts: i32,
        now: i64,
        batch: u64,
    ) -> GatewayResult<Vec<String>> {
        let rows = orders::Entity::find()
            .filter(orders::Column::Status.eq(PayStatus::Paid.code()))
            .filter(orders::Column::Num.lt(max_attempts))
            .filter(
                orders::Column::LastReissueTime
                    .lt(now.saturating_sub(super::state::MIN_REISSUE_INTERVAL_SECS)),
            )
            .order_by_asc(orders::Column::Id)
            .limit(batch)
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(|o| o.order_id).collect())
    }

    /// Claim one reissue attempt (§7.1's `num+1 / last_reissue_time=now`
    /// write, hardened into the admission CAS): the whole
    /// [`crate::ledger::reissue_admit`] predicate runs inside one
    /// conditional UPDATE, so racing sweepers
    /// (multiple cron hits — the legacy's §7 concurrency gap) can never
    /// consume the same attempt twice. `false` = lost the race or no longer
    /// due; the caller sends nothing.
    pub async fn claim_reissue(
        &self,
        order_id: &str,
        max_attempts: i32,
        now: i64,
    ) -> GatewayResult<bool> {
        let res = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE orders SET num = num + 1, last_reissue_time = $2 \
                 WHERE order_id = $1 AND status = 1 AND num < $3 \
                 AND last_reissue_time < $4",
                vec![
                    Value::from(order_id.to_string()),
                    Value::from(now),
                    Value::from(max_attempts),
                    Value::from(now.saturating_sub(super::state::MIN_REISSUE_INTERVAL_SECS)),
                ],
            ))
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// One scheduled-thaw release (`spec/02` §6.2/§6.3/§6.4). The sweep
    /// SELECTs the due row(s) DB-side; this replays [`thaw::apply`] with the
    /// guarded atomic UPDATE as the authoritative overdraft check — `Ok(None)`
    /// means "skip" (0 rows matched: overdraft guard or a raced `status = 0`
    /// CAS), exactly like the legacy guarded UPDATE that silently matched
    /// nothing. `log_id` (a `blocked_logs` row) is flipped to thawed in the
    /// SAME tx by its own CAS.
    pub async fn run_thaw(
        &self,
        kind: ThawKind,
        user_id: i64,
        amount: i64,
        log_id: Option<i64>,
    ) -> GatewayResult<Option<ThawResult>> {
        let merchant = member(&self.db, user_id).await?.ok_or_else(|| {
            GatewayError::Internal(format!("ledger.run_thaw: member {user_id} missing"))
        })?;
        let plan = thaw::apply(
            kind,
            user_id,
            ThawBuckets {
                available: merchant.balance,
                blocked: merchant.blocked_balance,
            },
            amount,
            log_id.map(|i| i.to_string()),
        )
        .map_err(|_| GatewayError::BadRequest("冻结余额不足".into()))?;

        let txn = self.db.begin().await?;

        // Iron rule #1: the debit side is the guarded atomic UPDATE — the
        // plan's hint check is reproduced authoritatively in SQL.
        let after = if kind.draws_blocked() {
            returning_i64(
                &txn,
                "UPDATE members SET balance = balance + $1, blocked_balance = blocked_balance - $1 \
                 WHERE id = $2 AND blocked_balance >= $1 RETURNING balance",
                "balance",
                amount,
                user_id,
            )
            .await?
        } else {
            Some(credit(&txn, "balance", user_id, amount).await?)
        };
        let Some(after) = after else {
            // 0 rows: the blocked guard failed (or the member vanished).
            txn.rollback().await?;
            return Ok(None);
        };

        // The freeze-ledger CAS rides the same tx (iron rule #3).
        if let Some(log_id) = log_id {
            let cas = txn
                .execute_raw(Statement::from_sql_and_values(
                    txn.get_database_backend(),
                    "UPDATE blocked_logs SET status = 1 WHERE id = $1 AND status = 0",
                    vec![Value::from(log_id)],
                ))
                .await?;
            if cas.rows_affected() == 0 {
                txn.rollback().await?;
                return Ok(None); // already swept by a concurrent runner
            }
        }

        let flow = FlowIntent {
            y_money: after - amount,
            g_money: after,
            ..plan.flow.clone()
        };
        insert_flow(&txn, &flow, log_id.map(|i| format!("thaw:{}", i))).await?;
        txn.commit().await?;
        Ok(Some(ThawResult {
            buckets_after: plan.buckets_after, // hint-derived; sweep callers re-read for display
            flow,
        }))
    }

    /// The §6.2 due list: un-thawed `blocked_logs` whose `thaw_time` landed
    /// (with the 7200s buffer) and that were created strictly before today's
    /// midnight, oldest id first, capped at [`thaw::T1_THAW_BATCH`]. The
    /// selection mirrors the pure [`thaw::t1_blockedlog_due`] exactly.
    pub async fn due_t1_thaws(
        &self,
        today_midnight: i64,
    ) -> GatewayResult<Vec<blocked_logs::Model>> {
        Ok(blocked_logs::Entity::find()
            .filter(blocked_logs::Column::Status.eq(0))
            .filter(blocked_logs::Column::ThawTime.lte(today_midnight + thaw::T1_THAW_BUFFER_SECS))
            .filter(blocked_logs::Column::CreateTime.lt(today_midnight))
            .order_by_asc(blocked_logs::Column::Id)
            .limit(thaw::T1_THAW_BATCH)
            .all(&self.db)
            .await?)
    }

    /// One T+1 cron run (§6.2): scan the due freeze ledger and release each
    /// row through [`Self::run_thaw`] (per-row tx — a skipped row never
    /// aborts the batch, the legacy per-item commit). The `blocked_logs`
    /// `status = 0` CAS inside `run_thaw` is what keeps two overlapping
    /// crons from double-releasing the same freeze.
    pub async fn run_t1_thaw_sweep(&self) -> GatewayResult<thaw::SweepReport> {
        let due = self.due_t1_thaws(today_midnight_ts()).await?;
        let mut report = thaw::SweepReport {
            scanned: due.len(),
            ..Default::default()
        };
        for log in due {
            let released = self
                .run_thaw(ThawKind::T1Blocked, log.user_id, log.amount, Some(log.id))
                .await?;
            match released {
                Some(_) => report.released += 1,
                None => report.skipped += 1,
            }
        }
        Ok(report)
    }

    /// The §6.3 due list: un-released complaints deposits whose
    /// `unfreeze_time` has landed and that are not paused, oldest id first,
    /// capped at [`thaw::DEPOSIT_UNFREEZE_BATCH`] (the legacy `select` had no
    /// cap; the batch keeps one runaway day from an unbounded tx storm).
    pub async fn due_deposit_unfreezes(
        &self,
        now: i64,
        batch: u64,
    ) -> GatewayResult<Vec<complaints_deposits::Model>> {
        Ok(complaints_deposits::Entity::find()
            .filter(complaints_deposits::Column::Status.eq(0))
            .filter(complaints_deposits::Column::IsPause.eq(0))
            .filter(complaints_deposits::Column::UnfreezeTime.lte(now))
            .order_by_asc(complaints_deposits::Column::Id)
            .limit(batch)
            .all(&self.db)
            .await?)
    }

    /// One complaints-deposit release (`spec/02` §6.3, `UC::doUnfreeze`): the
    /// withheld money was never part of `balance`, so the release is a pure
    /// credit + the ledger-row CAS + the `lx = 13` flow, one tx. The CAS
    /// (`WHERE id = ? AND status = 0`) keeps overlapping sweeps from
    /// double-releasing; `Ok(None)` = the row was already taken (or is gone).
    pub async fn run_deposit_unfreeze(
        &self,
        deposit: complaints_deposits::Model,
    ) -> GatewayResult<Option<ThawResult>> {
        let merchant = member(&self.db, deposit.user_id).await?.ok_or_else(|| {
            GatewayError::Internal(format!(
                "ledger.run_deposit_unfreeze: member {} missing",
                deposit.user_id
            ))
        })?;
        // The pure kernel plans the credit (never overdrafts — it adds).
        let plan = thaw::apply(
            ThawKind::ComplaintsDeposit,
            deposit.user_id,
            ThawBuckets {
                available: merchant.balance,
                blocked: merchant.blocked_balance,
            },
            deposit.freeze_money,
            None,
        )
        .map_err(|_| GatewayError::BadRequest("冻结余额不足".into()))?;

        let txn = self.db.begin().await?;
        let after = credit(&txn, "balance", deposit.user_id, deposit.freeze_money).await?;
        let now = crate::data::now_ts();
        let cas = txn
            .execute_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "UPDATE complaints_deposits SET status = 1, real_unfreeze_time = $2, \
                 update_at = $2 WHERE id = $1 AND status = 0",
                vec![Value::from(deposit.id), Value::from(now)],
            ))
            .await?;
        if cas.rows_affected() == 0 {
            txn.rollback().await?;
            return Ok(None); // already released by a concurrent runner
        }
        // The legacy flow carries the deposit's own order refs (UC:53-61).
        let flow = FlowIntent {
            y_money: after - deposit.freeze_money,
            g_money: after,
            trans_id: Some(deposit.pay_orderid.clone()),
            ..plan.flow
        };
        insert_flow(&txn, &flow, Some(format!("deposit:{}", deposit.id))).await?;
        txn.commit().await?;
        Ok(Some(ThawResult {
            buckets_after: plan.buckets_after,
            flow,
        }))
    }

    /// One deposit-unfreeze cron run (`spec/02` §6.3): scan the due ledger,
    /// release each row through [`Self::run_deposit_unfreeze`] (per-row tx —
    /// a skipped row never aborts the batch, the legacy per-item commit).
    pub async fn run_deposit_unfreeze_sweep(&self) -> GatewayResult<thaw::SweepReport> {
        let due = self
            .due_deposit_unfreezes(crate::data::now_ts(), thaw::DEPOSIT_UNFREEZE_BATCH)
            .await?;
        let mut report = thaw::SweepReport {
            scanned: due.len(),
            ..Default::default()
        };
        for deposit in due {
            match self.run_deposit_unfreeze(deposit).await? {
                Some(_) => report.released += 1,
                None => report.skipped += 1,
            }
        }
        Ok(report)
    }

    /// The admin manual balance moves (`spec/05` §11, `incrMoney` /
    /// `frozenMoney`): lx 3 手动增加 / 4 手动减少 / 7 冻结 / 8 解冻. One tx per
    /// move — the guarded atomic UPDATE (iron rule #1, the overdraft guard is
    /// the WHERE clause) plus its `money_changes` row (iron rule #2). The
    /// flow tracks the AVAILABLE bucket (`ymoney`/`gmoney` are the available
    /// before/after), so a freeze records `-amount` and an unfreeze `+amount`.
    /// `Ok(None)` = the guard failed (0 rows) — the caller answers
    /// 可用/冻结余额不足; the member row itself never went missing.
    pub async fn admin_move(
        &self,
        mv: AdminMove,
        user_id: i64,
        amount: i64,
    ) -> GatewayResult<Option<BalanceAfter>> {
        let (sql, lx, delta) = match mv {
            AdminMove::ManualAdd => (
                "UPDATE members SET balance = balance + $1 \
                 WHERE id = $2 RETURNING balance, blocked_balance",
                snapshot::lx::MANUAL_ADD,
                amount,
            ),
            AdminMove::ManualSub => (
                "UPDATE members SET balance = balance - $1 \
                 WHERE id = $2 AND balance >= $1 RETURNING balance, blocked_balance",
                snapshot::lx::MANUAL_SUB,
                -amount,
            ),
            AdminMove::Freeze => (
                "UPDATE members SET balance = balance - $1, blocked_balance = blocked_balance + $1 \
                 WHERE id = $2 AND balance >= $1 RETURNING balance, blocked_balance",
                snapshot::lx::FREEZE,
                -amount,
            ),
            AdminMove::Unfreeze => (
                "UPDATE members SET balance = balance + $1, blocked_balance = blocked_balance - $1 \
                 WHERE id = $2 AND blocked_balance >= $1 RETURNING balance, blocked_balance",
                snapshot::lx::UNFREEZE,
                amount,
            ),
        };
        let txn = self.db.begin().await?;
        let stmt = Statement::from_sql_and_values(
            txn.get_database_backend(),
            sql,
            vec![Value::from(amount), Value::from(user_id)],
        );
        let row = txn
            .query_one_raw(stmt)
            .await
            .map_err(crate::state::db_err)?;
        let Some(row) = row else {
            txn.rollback().await?;
            return Ok(None); // the guarded UPDATE matched nothing
        };
        let (available, blocked) = (
            row.try_get::<i64>("", "balance")?,
            row.try_get::<i64>("", "blocked_balance")?,
        );
        let flow = snapshot::flow(user_id, available - delta, delta, lx, None, None);
        insert_flow(&txn, &flow, None).await?;
        txn.commit().await?;
        Ok(Some(BalanceAfter { available, blocked }))
    }

    /// The manual reversal ([`spec/02-funds-order.md`] §6.5/§11.2 `Redo`) —
    /// the write side the legacy never built (its `redo_order` table had
    /// only the statistics READ side). One tx: the guarded atomic balance
    /// move + the `money_changes` flow + the `redo_orders` ledger row the
    /// merchant-income formula aggregates (`type` 1=增加 / 2=减少). The
    /// flow reuses the manual lx 3/4 vocabulary (the legacy lx table has no
    /// reversal code; the `redo_orders` row is what statistics key on), and
    /// the request_id stays unset like [`Self::admin_move`] — a reversal is
    /// an explicit, non-idempotent operator action.
    ///
    /// `period` is the 冲正周期 the row counts into (legacy `date`); `None`
    /// return = the decrease overdrawed the available balance (0 rows).
    pub async fn redo_balance(
        &self,
        user_id: i64,
        amount: i64,
        typ: RedoType,
        remark: &str,
        period: chrono::NaiveDateTime,
        admin_id: i64,
    ) -> GatewayResult<Option<BalanceAfter>> {
        if amount <= 0 {
            return Err(GatewayError::BadRequest("金额必须大于0".into()));
        }
        let (sql, lx, delta) = match typ {
            RedoType::Increase => (
                "UPDATE members SET balance = balance + $1 \
                 WHERE id = $2 RETURNING balance, blocked_balance",
                snapshot::lx::MANUAL_ADD,
                amount,
            ),
            RedoType::Decrease => (
                "UPDATE members SET balance = balance - $1 \
                 WHERE id = $2 AND balance >= $1 RETURNING balance, blocked_balance",
                snapshot::lx::MANUAL_SUB,
                -amount,
            ),
        };
        let txn = self.db.begin().await?;
        let stmt = Statement::from_sql_and_values(
            txn.get_database_backend(),
            sql,
            vec![Value::from(amount), Value::from(user_id)],
        );
        let row = txn
            .query_one_raw(stmt)
            .await
            .map_err(crate::state::db_err)?;
        let Some(row) = row else {
            txn.rollback().await?;
            return Ok(None); // overdrawed (or the member vanished)
        };
        let (available, blocked) = (
            row.try_get::<i64>("", "balance")?,
            row.try_get::<i64>("", "blocked_balance")?,
        );
        let flow = snapshot::flow(user_id, available - delta, delta, lx, None, None);
        insert_flow(&txn, &flow, None).await?;
        redo_orders::ActiveModel {
            user_id: Set(user_id),
            admin_id: Set(admin_id),
            money: Set(amount),
            redo_type: Set(typ.code()),
            remark: Set(remark.to_string()),
            date: Set(period),
            ctime: Set(crate::data::now_ts()),
            ..Default::default()
        }
        .insert(&txn)
        .await
        .map_err(crate::state::db_err)?;
        txn.commit().await?;
        Ok(Some(BalanceAfter { available, blocked }))
    }
}

// --- shared atomic-SQL helpers -------------------------------------------

/// The reversal direction ([`LedgerService::redo_balance`], `spec/02` §6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedoType {
    /// 1 增加 — credit the merchant's available balance.
    Increase,
    /// 2 减少 — guarded debit.
    Decrease,
}

impl RedoType {
    /// The `redo_orders.type` code the statistics aggregate on.
    pub fn code(self) -> i32 {
        match self {
            RedoType::Increase => 1,
            RedoType::Decrease => 2,
        }
    }
}

/// The admin manual move family ([`LedgerService::admin_move`], `spec/05`
/// §11) — the legacy `cztype` arms 3/4/7/8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminMove {
    /// 3 手动增加 (available +).
    ManualAdd,
    /// 4 手动减少 (available -, guarded).
    ManualSub,
    /// 7 冻结 (available → blocked, guarded).
    Freeze,
    /// 8 解冻 (blocked → available, guarded).
    Unfreeze,
}

/// A member's two buckets after an [`LedgerService::admin_move`] (the
/// RETURNING-exact post-write values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceAfter {
    pub available: i64,
    pub blocked: i64,
}

/// Resolves the complaints-deposit rule for one merchant (`spec/02` §4.4,
/// `PayModel::getComplaintsDepositRule`): the merchant's own row counts only
/// when ACTIVE — an inactive own row falls through to the platform
/// `is_system` row, whose own status then decides. `None` = no active rule.
async fn load_deposit_rule(
    db: &sea_orm::DatabaseConnection,
    user_id: i64,
) -> Result<Option<DepositCfg>, sea_orm::DbErr> {
    use complaints_deposit_rules as rules;
    let own = rules::Entity::find()
        .filter(rules::Column::UserId.eq(user_id))
        .one(db)
        .await?;
    let chosen = match own {
        Some(r) if r.status == 1 => Some(r),
        _ => {
            rules::Entity::find()
                .filter(rules::Column::IsSystem.eq(1))
                .one(db)
                .await?
        }
    };
    Ok(chosen.filter(|r| r.status == 1).map(|r| DepositCfg {
        ratio_pct: r.ratio_pct as i64,
        freeze_time: r.freeze_time,
    }))
}

/// The active deposit-rule projection: the withholding percent and the
/// freeze duration.
struct DepositCfg {
    ratio_pct: i64,
    freeze_time: i64,
}

/// `UPDATE members SET <col> = <col> + $amount WHERE id = $id RETURNING <col>`
/// — the iron-rule-#1 credit, also the row lock for that member.
pub(crate) async fn credit<C: ConnectionTrait>(
    conn: &C,
    col: &'static str,
    user_id: i64,
    amount: i64,
) -> Result<i64, GatewayError> {
    let sql = format!("UPDATE members SET {col} = {col} + $1 WHERE id = $2 RETURNING {col}");
    let after = returning_i64(conn, &sql, col, amount, user_id)
        .await
        .map_err(crate::state::db_err)?;
    after.ok_or_else(|| GatewayError::Internal(format!("ledger: member {user_id} missing")))
}

/// Runs a two-value (`$1 = amount`, `$2 = user_id`) UPDATE … RETURNING one
/// i64 column; `Ok(None)` when no row matched (guarded-UPDATE skip semantics).
async fn returning_i64<C: ConnectionTrait>(
    conn: &C,
    sql: &str,
    col: &str,
    amount: i64,
    user_id: i64,
) -> Result<Option<i64>, sea_orm::DbErr> {
    let stmt = Statement::from_sql_and_values(
        conn.get_database_backend(),
        sql,
        vec![Value::from(amount), Value::from(user_id)],
    );
    let row = conn.query_one_raw(stmt).await?;
    Ok(match row {
        Some(r) => Some(r.try_get::<i64>("", col)?),
        None => None,
    })
}

/// Writes one `money_changes` row (iron rule #2 — same tx as its move).
pub(crate) async fn insert_flow<C: ConnectionTrait>(
    conn: &C,
    f: &FlowIntent,
    request_id: Option<String>,
) -> Result<(), GatewayError> {
    money_changes::ActiveModel {
        user_id: Set(f.user_id),
        y_money: Set(f.y_money),
        money: Set(f.money),
        g_money: Set(f.g_money),
        datetime: Set(crate::data::now()),
        lx: Set(f.lx),
        trans_id: Set(f.trans_id.clone()),
        order_id: Set(f.order_id.clone()),
        request_id: Set(request_id),
        ..Default::default()
    }
    .insert(conn)
    .await
    .map_err(crate::state::db_err)?;
    Ok(())
}

async fn member<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
) -> Result<Option<members::Model>, sea_orm::DbErr> {
    members::Entity::find_by_id(user_id).one(conn).await
}

async fn channel_rate<C: ConnectionTrait>(
    conn: &C,
    channel_id: i64,
) -> Result<ChannelRate, GatewayError> {
    channels::Entity::find_by_id(channel_id)
        .one(conn)
        .await
        .map_err(crate::state::db_err)?
        .as_ref()
        .map(ChannelRate::from_model)
        .ok_or_else(|| GatewayError::Internal(format!("ledger: channel {channel_id} missing")))
}

/// The `parentid` walk feeding [`settle`]: `[merchant, p1, ..= up to
/// DEFAULT_MAX_LEVELS ancestors]`, stopping at the platform boundary
/// (`parentid <= 1`, §5.1). Each node's fee rate is the §5.2 live read
/// (`userrate` ?: channel default) on the order's frozen `channel_id`.
async fn build_chain(
    db: &sea_orm::DatabaseConnection,
    order: &orders::Model,
    merchant: &members::Model,
    channel: &ChannelRate,
    cycle: Cycle,
) -> Result<Vec<AgentLevel>, GatewayError> {
    let mut chain = Vec::with_capacity(DEFAULT_MAX_LEVELS + 1);
    let mut current = merchant.clone();
    loop {
        let feilv = node_feilv(db, current.id, order.channel_id, cycle, channel).await?;
        chain.push(AgentLevel {
            user_id: current.id,
            feilv,
            balance_before: current.balance,
        });
        // §5.1 stops: platform boundary (`parentid <= 1`) and the 3-level
        // recursion cap; a dangling parentid ends the walk like the legacy
        // missing-parent `return false`, without panicking.
        if current.parentid <= 1 || chain.len() > DEFAULT_MAX_LEVELS {
            break;
        }
        let Some(parent) = member(db, current.parentid).await? else {
            break;
        };
        current = parent;
    }
    Ok(chain)
}

/// One node's effective rate via the §3.2/§5.2 priority chain.
async fn node_feilv(
    db: &sea_orm::DatabaseConnection,
    user_id: i64,
    channel_id: i64,
    cycle: Cycle,
    channel: &ChannelRate,
) -> Result<i64, GatewayError> {
    let user = rate::load_user_rate(db, user_id, channel_id)
        .await
        .map_err(crate::state::db_err)?;
    Ok(rate::resolve(cycle, user.as_ref(), channel).feilv)
}

/// `strtotime('today')` — the sweep's midnight anchor (§6.2's `curtime`).
pub(crate) fn today_midnight_ts() -> i64 {
    let now = chrono::Local::now();
    let day = now.date_naive().and_hms_opt(0, 0, 0).expect("midnight");
    day.and_local_timezone(chrono::Local)
        .single()
        .expect("local midnight")
        .timestamp()
}

/// `strtotime('tomorrow') + rand(0, 7200)` — the T+1 release schedule the
/// legacy freeze writes (§4.3 / §6.2's `thawtime <= today + 7200` pair).
fn t1_thaw_ts() -> i64 {
    use rand::Rng;
    today_midnight_ts() + 86_400 + rand::rng().random_range(0..7_200)
}

/// SeaORM's portable unique-constraint error check.
pub(crate) fn is_unique_violation(e: &sea_orm::DbErr) -> bool {
    matches!(
        e.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    )
}
