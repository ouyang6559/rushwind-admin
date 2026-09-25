//! DB-gated payout execution-queue tests (`spec/04` §8 / §10): the submit
//! sweep (claim → exec → fold → release), the §8.3 failure→4 待确认 trap, the
//! query re-confirm, the §10.1 retry valve and the §8.2 `result === FALSE`
//! lock-release. Same harness as `payout_db.rs`; ids ride base
//! 11_400_000_000_000. The channel adapters are the local [`Stub`] / [`Broken`]
//! fakes (the real Yibao / Ali / … adapters are a future slice).

mod common;

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

use common::{suite, uid, Suite};
use payment_api::channel::ChannelError;
use payment_api::data::{members, payout_orders, tikuan_configs};
use payment_api::payout::state::PayoutStatus;
use payment_api::payout::{
    BankSnapshot, ExecResp, PayoutChannelCfg, PayoutExec, PayoutRegistry, PayoutService,
    SubmitGate, SubmitOutcome, SubmitWithdrawal,
};

const K: i64 = 10_000; // 1 元 in money units

fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}

fn svc(s: &Suite) -> PayoutService {
    PayoutService::new((*s.db).clone())
}

// --- the channel test doubles ----------------------------------------------

/// Replays a canned answer for `exec` and another for `query`.
struct Stub {
    exec: ExecResp,
    query: ExecResp,
}

impl Stub {
    fn always(resp: ExecResp) -> Self {
        Self {
            exec: resp.clone(),
            query: resp,
        }
    }
    fn then(exec: ExecResp, query: ExecResp) -> Self {
        Self { exec, query }
    }
}

#[async_trait]
impl PayoutExec for Stub {
    fn code(&self) -> &str {
        "stub"
    }
    async fn exec(
        &self,
        _order: &payout_orders::Model,
        _chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        Ok(self.exec.clone())
    }
    async fn query(
        &self,
        _order: &payout_orders::Model,
        _chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        Ok(self.query.clone())
    }
}

/// The §8.2 `result === FALSE` path — a transport fault, no fold.
struct Broken;

#[async_trait]
impl PayoutExec for Broken {
    fn code(&self) -> &str {
        "stub"
    }
    async fn exec(
        &self,
        _order: &payout_orders::Model,
        _chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        Err(ChannelError::Upstream("connection refused".into()))
    }
    async fn query(
        &self,
        _order: &payout_orders::Model,
        _chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        Err(ChannelError::Upstream("connection refused".into()))
    }
}

fn registry_with(adapter: impl PayoutExec + 'static) -> PayoutRegistry {
    let mut reg = PayoutRegistry::new();
    reg.register(adapter);
    reg
}

/// A proportional 2% channel: `cost = money × 2%`.
fn chan() -> PayoutChannelCfg {
    PayoutChannelCfg {
        id: 77,
        code: "stub".into(),
        name: "测试代付通道".into(),
        mch_id: Some("MCH77".into()),
        rate_type: 1,
        cost_rate: 20_000,
        ..Default::default()
    }
}

// --- seeding (mirrors payout_db.rs) ----------------------------------------

async fn ensure_system_row(s: &Suite) {
    s.db.execute_unprepared(
        "INSERT INTO tikuan_configs \
             (id, user_id, t1zt, tkzt, systemxz, issystem, tkzx_money, tkzd_money, \
              dayzd_money, dayzd_num, allow_start, allow_end, daycardzd_money, \
              tk_type, sx_rate, sxf_fixed, tk_charge_type) \
             VALUES (1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0) \
             ON CONFLICT (id) DO NOTHING",
    )
    .await
    .unwrap();
}

/// A merchant with a fat balance and an all-unlimited personal config row so
/// any 100元 withdrawal books cleanly and lands `status=0` ready for the queue.
async fn seed_merchant(s: &Suite, user: i64) {
    ensure_system_row(s).await;
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("e{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(1_000 * K),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    tikuan_configs::ActiveModel {
        id: Set(uid(11_400_000_000_000)),
        user_id: Set(user),
        t1zt: Set(0),
        tkzt: Set(1),
        systemxz: Set(1),
        issystem: Set(0),
        tkzx_money: Set(K),
        tkzd_money: Set(1_000_000 * K),
        dayzd_money: Set(0),
        dayzd_num: Set(0),
        allow_start: Set(0),
        allow_end: Set(0),
        daycardzd_money: Set(0),
        tk_type: Set(0),
        sx_rate: Set(0),
        sxf_fixed: Set(0),
        tk_charge_type: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// Books one pending (status=0) settlement withdrawal and returns it.
async fn book_pending(s: &Suite, user: i64, amount: i64, card: &str) -> payout_orders::Model {
    let out = svc(s)
        .submit_withdrawal(
            &SubmitWithdrawal {
                user_id: user,
                amount,
                out_trade_no: None,
                bank: BankSnapshot {
                    bankname: Some("工商银行".into()),
                    subbranch: Some("测试支行".into()),
                    accountname: Some("张三".into()),
                    cardnumber: Some(card.into()),
                    province: Some("北京".into()),
                    city: Some("北京".into()),
                },
            },
            now_ts(),
        )
        .await
        .unwrap();
    out.order
}

async fn reload(s: &Suite, order_no: &str) -> payout_orders::Model {
    payout_orders::Entity::find()
        .filter(payout_orders::Column::OrderNo.eq(order_no))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
}

// --- the submit sweep -------------------------------------------------------

#[tokio::test]
async fn submit_success_settles_and_books_channel_cost() {
    let Some(s) = suite().await else { return };
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E1").await;
    // Fee-free config → the full 100元 arrives (the channel COST is separate).
    assert_eq!(order.money, 100 * K);

    let reg = registry_with(Stub::always(ExecResp::success("代付成功")));
    let rep = svc(&s)
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    assert_eq!(rep.folded, 1);
    assert_eq!(rep.succeeded, 1);

    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Success.code());
    assert!(after.settled_at.is_some(), "success must stamp cldatetime");
    assert_eq!(after.memo.as_deref(), Some("代付成功"));
    // §8.2 attribution snapshot; cost = money(100元) × 2% = 2元.
    assert_eq!(after.df_channel_id, Some(77));
    assert_eq!(after.df_code.as_deref(), Some("stub"));
    assert_eq!(after.channel_mch_id.as_deref(), Some("MCH77"));
    assert_eq!(after.cost, 20_000);
    assert_eq!(after.rate_type, 1);
    // The lock is always released.
    assert_eq!(after.df_lock, 0);
}

#[tokio::test]
async fn channel_failure_degrades_to_unconfirmed_not_terminal() {
    let Some(s) = suite().await else { return };
    // §8.3 THE TRAP: a channel `3`(失败) answer lands status=4 待确认, never
    // terminal 3, and stamps no settle time — only the query loop can settle.
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E2").await;

    let reg = registry_with(Stub::always(ExecResp::failed("余额不足")));
    let rep = svc(&s)
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    assert_eq!(rep.unconfirmed, 1);
    assert_eq!(rep.succeeded, 0);

    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Unconfirmed.code());
    assert_ne!(after.status, PayoutStatus::Failed.code());
    assert!(after.settled_at.is_none());
    assert_eq!(after.memo.as_deref(), Some("余额不足"));
    assert_eq!(after.df_lock, 0);
}

#[tokio::test]
async fn submit_processing_then_query_success_settles() {
    let Some(s) = suite().await else { return };
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E3").await;

    // exec → 处理中 (status 1), not yet settled.
    let reg = registry_with(Stub::then(
        ExecResp::processing("申请成功"),
        ExecResp::success("代付成功"),
    ));
    svc(&s)
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    let mid = reload(&s, &order.order_no).await;
    assert_eq!(mid.status, PayoutStatus::Processing.code());
    assert!(mid.settled_at.is_none());
    assert_eq!(mid.df_code.as_deref(), Some("stub"));

    // The §10.2 query sweep now reads it (status=1) and settles to 2. Scope
    // the batch to THIS order — the DB is shared across parallel test binaries.
    let due = svc(&s).due_queries(100_000).await.unwrap();
    let mine: Vec<_> = due
        .into_iter()
        .filter(|o| o.order_no == order.order_no)
        .collect();
    assert_eq!(mine.len(), 1, "the processing order must be query-due");
    let mut channels = std::collections::BTreeMap::new();
    channels.insert(77_i64, chan());
    let rep = svc(&s)
        .run_query_batch(&mine, &reg, &channels, now_ts())
        .await
        .unwrap();
    assert_eq!(rep.succeeded, 1);

    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Success.code());
    assert!(after.settled_at.is_some());
    assert_eq!(after.auto_query_num, 1);
}

#[tokio::test]
async fn unknown_answer_folds_to_no_change() {
    let Some(s) = suite().await else { return };
    // A channel `4`(未知) answer moves nothing (§8.3 row 4): the order stays
    // pending, the lock is simply released.
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E4").await;

    let reg = registry_with(Stub::always(ExecResp::unconfirmed("结果未知")));
    let rep = svc(&s)
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    assert_eq!(rep.unchanged, 1);
    assert_eq!(rep.folded, 0);

    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Pending.code());
    assert_eq!(after.df_lock, 0);
}

#[tokio::test]
async fn broken_channel_releases_lock_without_folding() {
    let Some(s) = suite().await else { return };
    // §8.2 result === FALSE: the transport fault drops the lock and folds
    // nothing — the order stays pending for the next sweep.
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E5").await;

    let reg = registry_with(Broken);
    let rep = svc(&s)
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    assert_eq!(rep.failed, 1);
    assert_eq!(rep.folded, 0);

    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Pending.code());
    assert_eq!(after.df_lock, 0);
    assert!(after.memo.is_none(), "no fold means no memo write");
}

// --- claim exclusivity + the §10.1 retry valve ------------------------------

#[tokio::test]
async fn claim_is_exclusive_and_releases() {
    let Some(s) = suite().await else { return };
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E6").await;
    let svc = svc(&s);

    // First claim wins; a second while held loses (§8.2 df_lock atomicity).
    assert!(svc.claim_for_submit(&order.order_no).await.unwrap());
    assert!(!svc.claim_for_submit(&order.order_no).await.unwrap());
    let held = reload(&s, &order.order_no).await;
    assert_eq!(held.df_lock, 1);
    assert_eq!(held.status, PayoutStatus::Pending.code());

    // A sweep can't claim the still-held lock — it skips, folding nothing.
    let reg = registry_with(Stub::always(ExecResp::processing("ok")));
    let rep = svc
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    assert_eq!(rep.skipped, 1);
    assert_eq!(rep.folded, 0);

    // Once the lock is freed (as the release path does), the sweep claims and
    // folds cleanly.
    payout_orders::Entity::update_many()
        .col_expr(
            payout_orders::Column::DfLock,
            sea_orm::sea_query::Expr::value(0i32),
        )
        .filter(payout_orders::Column::OrderNo.eq(&order.order_no))
        .exec(&*s.db)
        .await
        .unwrap();
    let rep2 = svc
        .run_submit_batch(std::slice::from_ref(&order), &reg, &chan(), false, now_ts())
        .await
        .unwrap();
    assert_eq!(rep2.folded, 1);
    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Processing.code());
    assert_eq!(after.df_lock, 0);
}

#[tokio::test]
async fn auto_sweep_books_attempt_and_retry_valve_caps_pull() {
    let Some(s) = suite().await else { return };
    let user = uid(11_400_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000E7").await;

    // §10.1 auto bookkeeping: is_auto=1, auto_submit_try+1 on every drive.
    let reg = registry_with(Stub::always(ExecResp::processing("ok")));
    let outcome = svc(&s)
        .submit_one(&order, &reg, &chan(), true, now_ts())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        SubmitOutcome::Folded(PayoutStatus::Processing)
    ));
    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.auto_submit_try, 1);
    assert_eq!(after.is_auto, 1);
    assert!(after.last_submit_time > 0);
    assert_eq!(after.status, PayoutStatus::Processing.code());
    assert_eq!(after.df_lock, 0);

    // The `< 5` retry valve: reset this order to pending at 5 attempts — the
    // auto due-set (< 5 valve) must drop it while a manual sweep still sees it.
    payout_orders::Entity::update_many()
        .col_expr(
            payout_orders::Column::AutoSubmitTry,
            sea_orm::sea_query::Expr::value(5i32),
        )
        .col_expr(
            payout_orders::Column::Status,
            sea_orm::sea_query::Expr::value(0i16),
        )
        .filter(payout_orders::Column::OrderNo.eq(&order.order_no))
        .exec(&*s.db)
        .await
        .unwrap();

    let auto = svc(&s)
        .due_submits(&SubmitGate::auto(5, None, 100_000))
        .await
        .unwrap();
    assert!(
        !auto.iter().any(|o| o.order_no == order.order_no),
        "auto_submit_try=5 must fall outside the < 5 valve"
    );
    let manual = svc(&s)
        .due_submits(&SubmitGate::manual(100_000))
        .await
        .unwrap();
    assert!(manual.iter().any(|o| o.order_no == order.order_no));
}
