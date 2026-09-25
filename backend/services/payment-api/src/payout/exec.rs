//! The payout execution queue (`spec/04` §8 / §10): the runner that takes a
//! booked order (`status=0`) out to the payout channel, folds the channel's
//! `['status'=>1..4,'msg']` answer back onto the order through
//! [`apply_outcome`] (the §8.3 **failure→4 待确认** trap lives there), and
//! re-queries the in-flight ones (§10.2).
//!
//! The legacy spread this over three entry points — the Admin panel
//! `submitDf`, the manual `Payment/Index/index` sweep (§8) and the cron
//! `Cli/AutodfController` (auto-submit + `dfQuery`, §10) — each re-implementing
//! the same `flock` + `df_lock` claim and the same `handle` fold. This module
//! converges them behind ONE contract ([`PayoutExec`]) and ONE fold, keeping
//! the parts worth keeping and closing the §11/§12 defects:
//!
//! - §8.2 / §11 the process `flock` + soft `df_lock` becomes an atomic
//!   [`PayoutService::claim_for_submit`] (`df_lock 0→1 WHERE status=0`); the
//!   `auto_submit_try < 5` retry valve (§10.1) is a [`SubmitGate`] filter;
//! - §12.3 `handle` wrote status with **no old-status guard** (a late or
//!   replayed channel answer could clobber a settled order) → [`fold_exec`]
//!   CASes on the read status, so a raced fold is a no-op;
//! - §8.3 the trap is faithful: channel `3`(失败) folds to `status=4`
//!   (待确认), never terminal, and only the query loop can settle it (§12.7
//!   notes `dfQuery` reads only `status=1`, so a `4` needs a re-confirm sweep).
//!
//! The live HTTP adapters that satisfy [`PayoutExec`] for concrete upstreams
//! (`MGZF` / `Yibao`) live in [`super::channel`]; [`PayoutRegistry`] is the
//! injection seam the [`PayoutService`] sweeps drive, so the queue runs against
//! either a real adapter (round-tripping [`PayoutChannelCfg`]'s gateways +
//! secrets) or a fake in tests.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Statement,
    Value as SeaValue,
};

use crate::channel::ChannelError;
use crate::data::payout_orders;
use crate::money::scale_units;
use crate::state::{GatewayError, GatewayResult};

use super::order::PayoutService;
use super::state::{apply_outcome, ChannelOutcome, PayoutStatus};

/// The resolved payout channel — the legacy `$pfa_list` (`pay_for_another`),
/// not yet a modelled table, so the caller (panel / CLI) hands one in. It
/// carries what the adapter needs to sign and what [`fold_exec`] books as the
/// channel cost snapshot. The `*_gateway` / secret fields are the wiring the
/// real HTTP adapters (`super::channel`) POST to — never stored on the order,
/// only carried per drive.
#[derive(Debug, Clone, Default)]
pub struct PayoutChannelCfg {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub mch_id: Option<String>,
    /// Cost basis: `1` proportional (`money × cost_rate`) / `0` fixed.
    pub rate_type: i32,
    /// RATE_SCALE-scaled rate (proportional) or the fixed cost, money units.
    pub cost_rate: i64,
    /// Submit endpoint (`pay_for_another.exec_gateway`).
    pub exec_gateway: String,
    /// Query endpoint (`pay_for_another.query_gateway`).
    pub query_gateway: String,
    /// The MD5 signing secret (`pay_for_another.signkey`).
    pub sign_key: String,
    /// The trade password (`pay_for_another.appsecret`, Yibao `advPasswordMd5`).
    pub app_secret: String,
}

/// A channel's normalised exec / query answer (§9.1): `status` rides the
/// [`ChannelOutcome`] codes (`1`处理中 / `2`成功 / `3`失败 / `4`待确认).
#[derive(Debug, Clone)]
pub struct ExecResp {
    pub status: i16,
    pub msg: String,
}

impl ExecResp {
    /// `1` 提交成功 / 处理中.
    pub fn processing(msg: impl Into<String>) -> Self {
        Self {
            status: 1,
            msg: msg.into(),
        }
    }
    /// `2` 代付成功.
    pub fn success(msg: impl Into<String>) -> Self {
        Self {
            status: 2,
            msg: msg.into(),
        }
    }
    /// `3` 失败 — folded to `status=4` by the §8.3 trap, NOT terminal.
    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            status: 3,
            msg: msg.into(),
        }
    }
    /// `4` 待确认 / 未知 — folds to no change.
    pub fn unconfirmed(msg: impl Into<String>) -> Self {
        Self {
            status: 4,
            msg: msg.into(),
        }
    }
}

/// The upstream payout-adapter contract, mirroring the收款 [`crate::channel`]
/// trait: `PaymentExec` submits, `PaymentQuery` re-confirms. Both return the
/// uniform [`ExecResp`]; only a transport / signature fault yields `Err`
/// (the legacy's `result === FALSE`), which releases the lock without folding.
#[async_trait]
pub trait PayoutExec: Send + Sync {
    /// The channel code, matching the order's `df_code`.
    fn code(&self) -> &str;

    /// Submit one payout upstream (§PaymentExec).
    async fn exec(
        &self,
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError>;

    /// Re-query one in-flight payout (§PaymentQuery).
    async fn query(
        &self,
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError>;
}

/// The startup-built registry of payout adapters, keyed by lower-cased code.
/// It is empty by default (no built-in adapter is ported yet) and filled by
/// `register` — the same config-driven seam as [`crate::channel::ChannelRegistry`],
/// but injectable so the sweeps can be driven by a fake in tests.
#[derive(Default, Clone)]
pub struct PayoutRegistry {
    adapters: BTreeMap<String, Arc<dyn PayoutExec>>,
}

impl PayoutRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds an adapter to its own [`PayoutExec::code`] (case-insensitive).
    pub fn register<A: PayoutExec + 'static>(&mut self, adapter: A) -> &mut Self {
        let arc: Arc<dyn PayoutExec> = Arc::new(adapter);
        self.adapters.insert(arc.code().to_ascii_lowercase(), arc);
        self
    }

    /// The adapter for a channel code, if registered.
    pub fn get(&self, code: &str) -> Option<Arc<dyn PayoutExec>> {
        self.adapters.get(&code.to_ascii_lowercase()).cloned()
    }

    /// Whether a code has a live adapter.
    pub fn contains(&self, code: &str) -> bool {
        self.get(code).is_some()
    }

    /// The registered codes (diagnostics).
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys().map(String::as_str)
    }
}

/// The channel attribution [`fold_exec`] books alongside the status, from the
/// legacy `handle` `$data` (§8.2: `df_id` / `code` / `df_name` /
/// `channel_mch_id` / `cost` / `cost_rate` / `rate_type`). `cost` rides the
/// §8.2 `money` basis (IndexController); the auto-CLI's `tkmoney` basis is
/// registered as the §10.1口径不一致 quirk this snapshot leaves to the caller.
#[derive(Debug, Clone)]
pub struct ExecAttribution {
    pub df_channel_id: i64,
    pub df_code: String,
    pub df_name: String,
    pub channel_mch_id: Option<String>,
    pub cost: i64,
    pub cost_rate: i64,
    pub rate_type: i32,
}

impl ExecAttribution {
    /// Snapshots the channel and computes the cost off the order's arrival
    /// amount ([`cost_of`]).
    pub fn from_channel(chan: &PayoutChannelCfg, money: i64) -> Self {
        Self {
            df_channel_id: chan.id,
            df_code: chan.code.clone(),
            df_name: chan.name.clone(),
            channel_mch_id: chan.mch_id.clone(),
            cost: cost_of(chan.rate_type, chan.cost_rate, money),
            cost_rate: chan.cost_rate,
            rate_type: chan.rate_type,
        }
    }
}

/// The channel cost (§8.2): `rate_type` proportional → `money × cost_rate`
/// (cost_rate RATE_SCALE-scaled), else the fixed `cost_rate` (money units).
pub fn cost_of(rate_type: i32, cost_rate: i64, money: i64) -> i64 {
    if rate_type != 0 {
        scale_units(money, cost_rate)
    } else {
        cost_rate
    }
}

/// The submit-sweep filter (§10.1). The manual panel has no cap (`try_cap`
/// unbounded, no money ceiling); the auto CLI gates on `auto_df_maxmoney` and
/// the `auto_submit_try < 5` retry valve, plus the per-merchant same-day count
/// / amount caps (`auto_df_max_count` / `auto_df_max_sum`, `0` = unlimited)
/// enforced one order at a time inside [`PayoutService::run_auto_submit_sweep`].
#[derive(Debug, Clone, Copy)]
pub struct SubmitGate {
    /// Pull only orders with `auto_submit_try < try_cap`.
    pub try_cap: i32,
    /// Optional per-order arrival-amount ceiling (`auto_df_maxmoney`).
    pub max_money: Option<i64>,
    /// Batch size (§8.2 caps at 15, §10.1 at 10).
    pub limit: u64,
    /// Per-merchant same-day auto count cap (`auto_df_max_count`, 0 = off).
    pub max_count: i64,
    /// Per-merchant same-day auto amount cap, money units
    /// (`auto_df_max_sum`, 0 = off).
    pub max_sum: i64,
}

impl SubmitGate {
    /// The manual sweep: every free pending order, up to `limit`.
    pub fn manual(limit: u64) -> Self {
        Self {
            try_cap: i32::MAX,
            max_money: None,
            limit,
            max_count: 0,
            max_sum: 0,
        }
    }
    /// The auto sweep (§10.1): retry valve + money ceiling, no daily caps.
    pub fn auto(try_cap: i32, max_money: Option<i64>, limit: u64) -> Self {
        Self {
            try_cap,
            max_money,
            limit,
            max_count: 0,
            max_sum: 0,
        }
    }
}

/// What happened to one order during a submit / query drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The `df_lock` claim was lost (already claimed / left pending) — skipped.
    NotClaimed,
    /// No adapter is registered for the channel — released, no fold.
    NoAdapter,
    /// The adapter faulted (transport / signature) — §8.2 `result === FALSE`:
    /// released without folding, order stays at its prior status.
    TransportFailed,
    /// The answer folded but changed nothing (channel `4` 未知).
    Unchanged,
    /// The answer folded onto a new status (`Success` / `Processing` /
    /// `Unconfirmed` — the §8.3 failure→4 trap surfaces here).
    Folded(PayoutStatus),
}

/// A batch sweep tally.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchReport {
    /// Orders offered to the sweep.
    pub processed: usize,
    /// Claim lost (concurrent executor held the lock).
    pub skipped: usize,
    /// No adapter for the channel.
    pub no_adapter: usize,
    /// Transport faults (lock released, no fold).
    pub failed: usize,
    /// Held back by a §10.1 per-merchant daily cap (attempt bumped, not sent).
    pub capped: usize,
    /// Folded but unchanged (channel `4`).
    pub unchanged: usize,
    /// Status moved.
    pub folded: usize,
    /// Of those, settled to `2` 成功.
    pub succeeded: usize,
    /// Of those, degraded to `4` 待确认 (the trap).
    pub unconfirmed: usize,
}

/// The fold write-decision, split out so the §8.3 mapping is unit-testable
/// without a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldPlan {
    /// Leave the row untouched (channel `4` / unknown).
    NoChange,
    /// Persist `status` and, on success, stamp `settled_at`.
    Write {
        status: PayoutStatus,
        settled: Option<i64>,
    },
}

fn plan_fold(current: PayoutStatus, outcome: ChannelOutcome, now_ts: i64) -> FoldPlan {
    let effect = apply_outcome(current, outcome);
    if !effect.changed {
        return FoldPlan::NoChange;
    }
    FoldPlan::Write {
        status: effect.status,
        settled: effect.settled.then_some(now_ts),
    }
}

// --- the queue, hung off the order service ---------------------------------

/// Books one [`SubmitOutcome`] onto a running [`BatchReport`] tally (the
/// submit and auto-submit loops share this mapping).
fn tally_submit(rep: &mut BatchReport, outcome: SubmitOutcome) {
    match outcome {
        SubmitOutcome::NotClaimed => rep.skipped += 1,
        SubmitOutcome::NoAdapter => rep.no_adapter += 1,
        SubmitOutcome::TransportFailed => rep.failed += 1,
        SubmitOutcome::Unchanged => rep.unchanged += 1,
        SubmitOutcome::Folded(status) => {
            rep.folded += 1;
            match status {
                PayoutStatus::Success => rep.succeeded += 1,
                PayoutStatus::Unconfirmed => rep.unconfirmed += 1,
                _ => {}
            }
        }
    }
}

impl PayoutService {
    /// The §8.1 / §10.1 due set: free pending orders (`status=0`, `df_lock=0`)
    /// under the [`SubmitGate`], oldest first by `id` then `auto_submit_try`.
    /// A downstream payout-API order (`source=3`) only becomes queue-eligible
    /// once the merchant has passed review — the sweep skips any row still
    /// carrying a `check_status` other than approved (§6.3: pending `0` /
    /// rejected `2` must not execute; `NULL` = a settlement/entrusted order,
    /// which has no review gate).
    pub async fn due_submits(&self, gate: &SubmitGate) -> GatewayResult<Vec<payout_orders::Model>> {
        let mut q = payout_orders::Entity::find()
            .filter(payout_orders::Column::Status.eq(0))
            .filter(payout_orders::Column::DfLock.eq(0))
            .filter(payout_orders::Column::AutoSubmitTry.lt(gate.try_cap))
            .filter(
                sea_orm::sea_query::Condition::any()
                    .add(payout_orders::Column::CheckStatus.is_null())
                    .add(payout_orders::Column::CheckStatus.eq(1)),
            )
            .order_by_asc(payout_orders::Column::Id)
            .order_by_asc(payout_orders::Column::AutoSubmitTry);
        if let Some(max) = gate.max_money {
            q = q.filter(payout_orders::Column::Money.lte(max));
        }
        Ok(q.limit(gate.limit).all(self.conn()).await?)
    }

    /// The §8.1 manual sweep by explicit order nos (`status=0` only).
    pub async fn due_submits_for(
        &self,
        order_nos: &[String],
    ) -> GatewayResult<Vec<payout_orders::Model>> {
        Ok(payout_orders::Entity::find()
            .filter(payout_orders::Column::Status.eq(0))
            .filter(payout_orders::Column::OrderNo.is_in(order_nos.iter()))
            .order_by_asc(payout_orders::Column::Id)
            .all(self.conn())
            .await?)
    }

    /// The §10.2 query due set: in-flight orders (`status=1`), least-queried
    /// first. `status=4` is deliberately NOT swept here (§12.7 — the legacy
    /// leaves待确认 orders for a manual re-confirm; wiring that is a policy
    /// call for the auto-CLI slice, not this runner).
    pub async fn due_queries(&self, limit: u64) -> GatewayResult<Vec<payout_orders::Model>> {
        Ok(payout_orders::Entity::find()
            .filter(payout_orders::Column::Status.eq(1))
            .order_by_asc(payout_orders::Column::Id)
            .order_by_asc(payout_orders::Column::AutoQueryNum)
            .limit(limit)
            .all(self.conn())
            .await?)
    }

    /// §8.2 / §10.1 the atomic claim: `df_lock 0→1` on a still-pending row.
    /// `false` means a concurrent executor holds it — the legacy's `flock` +
    /// `setField df_lock=1` pair, collapsed to one guarded UPDATE.
    pub async fn claim_for_submit(&self, order_no: &str) -> GatewayResult<bool> {
        let res = self
            .conn()
            .execute_raw(Statement::from_sql_and_values(
                self.conn().get_database_backend(),
                "UPDATE payout_orders SET df_lock = 1 \
                 WHERE order_no = $1 AND status = 0 AND df_lock = 0",
                [SeaValue::from(order_no.to_string())],
            ))
            .await
            .map_err(crate::state::db_err)?;
        Ok(res.rows_affected() == 1)
    }

    /// Releases the submit lock after a drive. Manual (§8.2:103) only frees
    /// `df_lock`; auto (§10.1:114) also books the attempt — `is_auto=1`,
    /// `auto_submit_try + 1`, `last_submit_time` — and frees the lock.
    async fn finish_submit(&self, order_no: &str, is_auto: bool, now_ts: i64) -> GatewayResult<()> {
        let (sql, params) = if is_auto {
            (
                "UPDATE payout_orders SET df_lock = 0, is_auto = 1, last_submit_time = $2, \
                 auto_submit_try = auto_submit_try + 1 WHERE order_no = $1",
                vec![SeaValue::from(order_no.to_string()), SeaValue::from(now_ts)],
            )
        } else {
            (
                "UPDATE payout_orders SET df_lock = 0, last_submit_time = $2 WHERE order_no = $1",
                vec![SeaValue::from(order_no.to_string()), SeaValue::from(now_ts)],
            )
        };
        self.conn()
            .execute_raw(Statement::from_sql_and_values(
                self.conn().get_database_backend(),
                sql,
                params,
            ))
            .await
            .map_err(crate::state::db_err)?;
        Ok(())
    }

    /// §8.3 `handle` fold with the failure→4 trap. Reads the current status,
    /// maps the channel answer through [`apply_outcome`], and CAS-persists on
    /// that read status (§12.3 guard the legacy lacked). `attribution` writes
    /// the channel cost snapshot alongside (the submit path); `None` folds the
    /// status / memo only (the query path). Returns the new status, or `None`
    /// when nothing changed (channel `4`, or a lost CAS).
    pub async fn fold_exec(
        &self,
        order_no: &str,
        resp: &ExecResp,
        attribution: Option<&ExecAttribution>,
        now_ts: i64,
    ) -> GatewayResult<Option<PayoutStatus>> {
        let order = self
            .find(order_no)
            .await?
            .ok_or_else(|| GatewayError::BadRequest("提现单不存在".to_string()))?;
        let current = PayoutStatus::from_code(order.status).ok_or_else(|| {
            GatewayError::Internal(format!("payout: bad status {}", order.status))
        })?;
        let outcome = ChannelOutcome::from_code(resp.status).ok_or_else(|| {
            GatewayError::Internal(format!("payout: bad channel status {}", resp.status))
        })?;
        let (new_status, settled_at) = match plan_fold(current, outcome, now_ts) {
            FoldPlan::NoChange => return Ok(None),
            FoldPlan::Write { status, settled } => (status, settled),
        };
        let backend = self.conn().get_database_backend();
        let rows = match attribution {
            Some(a) => self
                .conn()
                .execute_raw(Statement::from_sql_and_values(
                    backend,
                    "UPDATE payout_orders SET status = $2, memo = $3, \
                         settled_at = COALESCE($4, settled_at), df_channel_id = $5, \
                         df_code = $6, df_name = $7, channel_mch_id = $8, cost = $9, \
                         cost_rate = $10, rate_type = $11 \
                         WHERE order_no = $1 AND status = $12",
                    vec![
                        SeaValue::from(order_no.to_string()),
                        SeaValue::from(new_status.code()),
                        SeaValue::from(resp.msg.clone()),
                        SeaValue::BigInt(settled_at),
                        SeaValue::from(a.df_channel_id),
                        SeaValue::from(a.df_code.clone()),
                        SeaValue::from(a.df_name.clone()),
                        SeaValue::from(a.channel_mch_id.clone()),
                        SeaValue::from(a.cost),
                        SeaValue::from(a.cost_rate),
                        SeaValue::from(a.rate_type),
                        SeaValue::from(order.status),
                    ],
                ))
                .await
                .map_err(crate::state::db_err)?,
            None => self
                .conn()
                .execute_raw(Statement::from_sql_and_values(
                    backend,
                    "UPDATE payout_orders SET status = $2, memo = $3, \
                         settled_at = COALESCE($4, settled_at) \
                         WHERE order_no = $1 AND status = $5",
                    vec![
                        SeaValue::from(order_no.to_string()),
                        SeaValue::from(new_status.code()),
                        SeaValue::from(resp.msg.clone()),
                        SeaValue::BigInt(settled_at),
                        SeaValue::from(order.status),
                    ],
                ))
                .await
                .map_err(crate::state::db_err)?,
        };
        // A lost CAS (§12.3 guard) means a concurrent op already moved the row.
        Ok(rows.rows_affected().eq(&1).then_some(new_status))
    }

    /// Drives ONE order through the §8.2 discipline: claim, submit via the
    /// channel's adapter, fold the answer, then release / bookkeeping. Never
    /// leaves `df_lock` held on any exit path.
    pub async fn submit_one(
        &self,
        order: &payout_orders::Model,
        registry: &PayoutRegistry,
        chan: &PayoutChannelCfg,
        is_auto: bool,
        now_ts: i64,
    ) -> GatewayResult<SubmitOutcome> {
        if !self.claim_for_submit(&order.order_no).await? {
            return Ok(SubmitOutcome::NotClaimed);
        }
        let adapter = match registry.get(&chan.code) {
            Some(a) => a,
            None => {
                self.finish_submit(&order.order_no, is_auto, now_ts).await?;
                return Ok(SubmitOutcome::NoAdapter);
            }
        };
        match adapter.exec(order, chan).await {
            Err(_) => {
                // §8.2 result === FALSE: drop the lock, do not fold.
                self.finish_submit(&order.order_no, is_auto, now_ts).await?;
                Ok(SubmitOutcome::TransportFailed)
            }
            Ok(resp) => {
                let attr = ExecAttribution::from_channel(chan, order.money);
                let folded = self
                    .fold_exec(&order.order_no, &resp, Some(&attr), now_ts)
                    .await?;
                self.finish_submit(&order.order_no, is_auto, now_ts).await?;
                Ok(match folded {
                    Some(s) => SubmitOutcome::Folded(s),
                    None => SubmitOutcome::Unchanged,
                })
            }
        }
    }

    /// §8 / §10.1 the submit sweep over a due batch. `is_auto` selects the
    /// §10.1 attempt bookkeeping vs the §8.2 plain release.
    pub async fn run_submit_batch(
        &self,
        orders: &[payout_orders::Model],
        registry: &PayoutRegistry,
        chan: &PayoutChannelCfg,
        is_auto: bool,
        now_ts: i64,
    ) -> GatewayResult<BatchReport> {
        let mut rep = BatchReport::default();
        for order in orders {
            rep.processed += 1;
            let outcome = self
                .submit_one(order, registry, chan, is_auto, now_ts)
                .await?;
            tally_submit(&mut rep, outcome);
        }
        Ok(rep)
    }

    /// §10.2 drives ONE in-flight order through the query fold, always
    /// bumping `auto_query_num`. `chan` is the resolved channel the order was
    /// submitted on (carrying the gateways + secrets the real adapter needs —
    /// the legacy reads `pay_for_another` by the order's `df_id`); the adapter
    /// is looked up by the order's recorded `df_code`.
    pub async fn query_one(
        &self,
        order: &payout_orders::Model,
        registry: &PayoutRegistry,
        chan: &PayoutChannelCfg,
        now_ts: i64,
    ) -> GatewayResult<SubmitOutcome> {
        let code = match &order.df_code {
            Some(c) => c.clone(),
            None => return Ok(SubmitOutcome::NoAdapter),
        };
        let adapter = match registry.get(&code) {
            Some(a) => a,
            None => return Ok(SubmitOutcome::NoAdapter),
        };
        let outcome = match adapter.query(order, chan).await {
            Err(_) => SubmitOutcome::TransportFailed,
            Ok(resp) => match self.fold_exec(&order.order_no, &resp, None, now_ts).await? {
                Some(s) => SubmitOutcome::Folded(s),
                None => SubmitOutcome::Unchanged,
            },
        };
        self.conn()
            .execute_raw(Statement::from_sql_and_values(
                self.conn().get_database_backend(),
                "UPDATE payout_orders SET auto_query_num = auto_query_num + 1 WHERE order_no = $1",
                [SeaValue::from(order.order_no.clone())],
            ))
            .await
            .map_err(crate::state::db_err)?;
        Ok(outcome)
    }

    /// §10.2 the query sweep over a due batch. `channels` maps a
    /// `df_channel_id` to its resolved [`PayoutChannelCfg`] (the caller loads
    /// the `pay_for_another` rows the in-flight orders reference); an order on
    /// an unresolvable channel counts as `no_adapter` and is skipped.
    pub async fn run_query_batch(
        &self,
        orders: &[payout_orders::Model],
        registry: &PayoutRegistry,
        channels: &BTreeMap<i64, PayoutChannelCfg>,
        now_ts: i64,
    ) -> GatewayResult<BatchReport> {
        let mut rep = BatchReport::default();
        for order in orders {
            rep.processed += 1;
            let chan = match channels.get(&order.df_channel_id.unwrap_or(0)) {
                Some(c) => c,
                None => {
                    rep.no_adapter += 1;
                    continue;
                }
            };
            match self.query_one(order, registry, chan, now_ts).await? {
                SubmitOutcome::NotClaimed => rep.skipped += 1,
                SubmitOutcome::NoAdapter => rep.no_adapter += 1,
                SubmitOutcome::TransportFailed => rep.failed += 1,
                SubmitOutcome::Unchanged => rep.unchanged += 1,
                SubmitOutcome::Folded(status) => {
                    rep.folded += 1;
                    match status {
                        PayoutStatus::Success => rep.succeeded += 1,
                        PayoutStatus::Unconfirmed => rep.unconfirmed += 1,
                        _ => {}
                    }
                }
            }
        }
        Ok(rep)
    }
}

// --- DB-driven sweeps: config resolved from `payout_channels` ---------------
//
// The high-level entry points a panel / cron drives the queue through. They
// replace the caller hand-building a [`PayoutChannelCfg`]: the channel
// endpoints + secrets are read straight from the `payout_channels`
// (`pay_for_another`) table, exactly as the legacy resolved them — the auto
// submit through the `status=1 AND is_default=1` row, a manual submit through
// the operator's enabled pick, and the query loop through each order's own
// `df_channel_id` (no status filter, so an offline channel still settles its
// in-flight orders). The lower primitives above stay cfg-in / map-in so the
// fold and claim logic remain driveable by a fake in tests.

impl PayoutService {
    /// The payout-channel store over the service's own pool.
    fn channels(
        &self,
    ) -> super::payout_channel::PayoutChannelRepo<'_, sea_orm::DatabaseConnection> {
        super::payout_channel::PayoutChannelRepo::new(self.conn())
    }

    /// Resolves a channel cfg by id (no status filter), the §10.2 read path.
    pub async fn resolve_channel(&self, id: i64) -> GatewayResult<Option<PayoutChannelCfg>> {
        self.channels().cfg_by_id(id).await
    }

    /// The auto-submit default channel (§10.1), if one is configured.
    pub async fn default_channel(&self) -> GatewayResult<Option<PayoutChannelCfg>> {
        self.channels().default_enabled_cfg().await
    }

    /// §10.1 / §8.1 the auto submit sweep: pulls the due set under `gate`,
    /// resolves the `is_default` channel from the table, and drives the batch.
    /// `None` when no enabled default is configured (the legacy's
    /// `默认代付通道不存在` early-exit) — nothing is claimed or folded.
    ///
    /// The per-merchant same-day caps (`gate.max_count` / `gate.max_sum`) are
    /// checked one order at a time before the claim, exactly as the legacy
    /// re-counted `is_auto = 1` same-day rows per iteration: a capped order
    /// is never claimed — it just books an attempt (`auto_submit_try + 1`,
    /// `last_submit_time`) and rotates out of the `< 5` valve over time,
    /// counted in [`BatchReport::capped`]. Because each successful submit
    /// flips the order to `is_auto = 1` immediately, the running tally already
    /// counts this batch's own sends, so an intra-batch over-limit is caught
    /// without a second pass.
    pub async fn run_auto_submit_sweep(
        &self,
        registry: &PayoutRegistry,
        gate: &SubmitGate,
        now_ts: i64,
    ) -> GatewayResult<Option<BatchReport>> {
        let Some(chan) = self.default_channel().await? else {
            return Ok(None);
        };
        let orders = self.due_submits(gate).await?;
        let mut rep = BatchReport::default();
        for order in orders {
            rep.processed += 1;
            if gate.max_count > 0 || gate.max_sum > 0 {
                let (count, sum) = self.auto_today_for_merchant(order.user_id, now_ts).await?;
                if super::auto_df::over_cap(gate.max_count, count, gate.max_sum, sum) {
                    self.bump_auto_try(&order.order_no, now_ts).await?;
                    rep.capped += 1;
                    continue;
                }
            }
            let outcome = self
                .submit_one(&order, registry, &chan, true, now_ts)
                .await?;
            tally_submit(&mut rep, outcome);
        }
        Ok(Some(rep))
    }

    /// The merchant's same-day 自动代付 tally (§10.1 caps): `(count, SUM(tkmoney))`
    /// over `is_auto = 1` orders created in the merchant's local day. One
    /// aggregate read, not a per-order scan.
    pub async fn auto_today_for_merchant(
        &self,
        user_id: i64,
        now_ts: i64,
    ) -> GatewayResult<(i64, i64)> {
        use chrono::{DateTime, Local};
        let today = DateTime::from_timestamp(now_ts, 0)
            .map(|dt| dt.with_timezone(&Local).date_naive())
            .unwrap_or_else(|| Local::now().naive_local().date());
        let day_start = today
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always valid");
        let day_end = today
            .and_hms_opt(23, 59, 59)
            .expect("last second is always valid");
        let rows = payout_orders::Entity::find()
            .filter(payout_orders::Column::UserId.eq(user_id))
            .filter(payout_orders::Column::IsAuto.eq(1))
            .filter(payout_orders::Column::CreatedAt.gte(day_start))
            .filter(payout_orders::Column::CreatedAt.lte(day_end))
            .all(self.conn())
            .await?;
        let count = rows.len() as i64;
        let sum: i64 = rows.iter().map(|o| o.tkmoney).sum();
        Ok((count, sum))
    }

    /// The §10.1 over-limit bookkeeping: bump the attempt and stamp the time,
    /// leaving `status` / `df_lock` untouched (the order was never claimed) so
    /// it simply ages out of the retry valve.
    async fn bump_auto_try(&self, order_no: &str, now_ts: i64) -> GatewayResult<()> {
        self.conn()
            .execute_raw(Statement::from_sql_and_values(
                self.conn().get_database_backend(),
                "UPDATE payout_orders SET last_submit_time = $2, \
                 auto_submit_try = auto_submit_try + 1 WHERE order_no = $1",
                [SeaValue::from(order_no.to_string()), SeaValue::from(now_ts)],
            ))
            .await
            .map_err(crate::state::db_err)?;
        Ok(())
    }

    /// §8.1 the manual submit sweep over named order nos: resolves the
    /// operator's channel pick (must be enabled — `findPaymentType(id)`), then
    /// drives just those orders. `None` when the id is unknown or disabled.
    pub async fn run_manual_submit_sweep(
        &self,
        registry: &PayoutRegistry,
        channel_id: i64,
        order_nos: &[String],
        now_ts: i64,
    ) -> GatewayResult<Option<BatchReport>> {
        let Some(chan) = self.channels().cfg_enabled_by_id(channel_id).await? else {
            return Ok(None);
        };
        let orders = self.due_submits_for(order_nos).await?;
        Ok(Some(
            self.run_submit_batch(&orders, registry, &chan, false, now_ts)
                .await?,
        ))
    }

    /// §10.2 the query sweep: pulls the in-flight (`status=1`) due set, loads
    /// every referenced channel row in one read by `df_channel_id`, and folds
    /// the answers. An order on a since-deleted channel counts `no_adapter`.
    pub async fn run_query_sweep(
        &self,
        registry: &PayoutRegistry,
        limit: u64,
        now_ts: i64,
    ) -> GatewayResult<BatchReport> {
        let orders = self.due_queries(limit).await?;
        let ids: Vec<i64> = orders
            .iter()
            .filter_map(|o| o.df_channel_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let channels = self.channels().cfgs_for_ids(&ids).await?;
        self.run_query_batch(&orders, registry, &channels, now_ts)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A no-op channel: enough to prove the registry stores/retrieves adapters
    /// by code without a live round-trip. The richer replay / fault fakes ride
    /// the DB integration tests (`tests/payout_exec.rs`), which implement the
    /// same public [`PayoutExec`] trait.
    struct NullChannel;

    #[async_trait]
    impl PayoutExec for NullChannel {
        fn code(&self) -> &str {
            "Null"
        }
        async fn exec(
            &self,
            _order: &payout_orders::Model,
            _chan: &PayoutChannelCfg,
        ) -> Result<ExecResp, ChannelError> {
            Ok(ExecResp::processing("noop"))
        }
        async fn query(
            &self,
            _order: &payout_orders::Model,
            _chan: &PayoutChannelCfg,
        ) -> Result<ExecResp, ChannelError> {
            Ok(ExecResp::processing("noop"))
        }
    }

    const K: i64 = 10_000;

    #[test]
    fn cost_is_proportional_or_fixed() {
        // rate_type 1: 100元 × 2% (RATE_SCALE 20_000) = 2元.
        assert_eq!(cost_of(1, 20_000, 100 * K), 2 * K);
        // rate_type 0: cost_rate is already a fixed money-units fee.
        assert_eq!(cost_of(0, 5 * K, 100 * K), 5 * K);
    }

    #[test]
    fn fold_plan_maps_channel_answers_through_the_trap() {
        // §8.3: a submit answer of `1` lands处理中, no settle stamp.
        match plan_fold(PayoutStatus::Pending, ChannelOutcome::Processing, 999) {
            FoldPlan::Write { status, settled } => {
                assert_eq!(status, PayoutStatus::Processing);
                assert_eq!(settled, None);
            }
            FoldPlan::NoChange => panic!("processing must move the row"),
        }
        // `2` success stamps settled_at with now.
        match plan_fold(PayoutStatus::Processing, ChannelOutcome::Success, 999) {
            FoldPlan::Write { status, settled } => {
                assert_eq!(status, PayoutStatus::Success);
                assert_eq!(settled, Some(999));
            }
            FoldPlan::NoChange => panic!("success must move the row"),
        }
        // `3` failure → 4 待确认 (the trap), NOT terminal 3, no stamp.
        match plan_fold(PayoutStatus::Processing, ChannelOutcome::Failed, 999) {
            FoldPlan::Write { status, settled } => {
                assert_eq!(status, PayoutStatus::Unconfirmed);
                assert_eq!(settled, None);
            }
            FoldPlan::NoChange => panic!("failure must degrade to待确认"),
        }
        // `4` unknown leaves the row untouched.
        assert_eq!(
            plan_fold(PayoutStatus::Processing, ChannelOutcome::Unknown, 999),
            FoldPlan::NoChange
        );
    }

    #[test]
    fn registry_is_case_insensitive_and_start_empty() {
        let mut reg = PayoutRegistry::new();
        assert!(!reg.contains("null"));
        reg.register(NullChannel);
        assert!(reg.contains("NULL"));
        assert_eq!(reg.get("null").unwrap().code(), "Null");
    }
}
