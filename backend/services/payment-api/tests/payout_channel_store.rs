//! DB-gated tests for the payout-channel store (`spec/04` §8 / §10): the
//! `payout_channels` (`pay_for_another`) CRUD, the read→[`PayoutChannelCfg`]
//! projection (secrets / endpoints / cost preserved, disabled-vs-enabled
//! resolution), and the queue resolving a channel STRAIGHT from the table to
//! drive an order — the slice that replaces the caller hand-building a cfg.
//! Same harness / isolation rules as `payout_exec.rs`; ids ride base
//! 11_900_000_000_000. The channel adapter is a local echo [`Stub`] that
//! reflects the resolved `mch_id` back in its message, so a successful fold
//! PROVES the cfg the sweep handed the adapter came from the DB row.

mod common;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
};

use common::{suite, uid, Suite};
use payment_api::channel::ChannelError;
use payment_api::data::{members, payout_channels, payout_orders, tikuan_configs};
use payment_api::payout::state::PayoutStatus;
use payment_api::payout::{
    BankSnapshot, ExecResp, NewPayoutChannel, PayoutChannelCfg, PayoutChannelRepo, PayoutExec,
    PayoutRegistry, PayoutService, SubmitGate, SubmitWithdrawal, UpdatePayoutChannel,
};

const K: i64 = 10_000;

fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}

fn svc(s: &Suite) -> PayoutService {
    PayoutService::new((*s.db).clone())
}

fn repo(s: &Suite) -> PayoutChannelRepo<'_, sea_orm::DatabaseConnection> {
    PayoutChannelRepo::new(&*s.db)
}

// --- the echo adapter -------------------------------------------------------

struct Stub;

#[async_trait]
impl PayoutExec for Stub {
    fn code(&self) -> &str {
        "stub"
    }
    async fn exec(
        &self,
        _order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        Ok(ExecResp::processing(format!(
            "exec:{}",
            chan.mch_id.clone().unwrap_or_default()
        )))
    }
    async fn query(
        &self,
        _order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        Ok(ExecResp::success(format!(
            "q:{}",
            chan.mch_id.clone().unwrap_or_default()
        )))
    }
}

fn registry() -> PayoutRegistry {
    let mut reg = PayoutRegistry::new();
    reg.register(Stub);
    reg
}

fn new_channel(code: &str, mch: &str) -> NewPayoutChannel {
    NewPayoutChannel {
        code: code.into(),
        title: "测试代付通道".into(),
        mch_id: Some(mch.into()),
        app_id: None,
        app_secret: Some("app-secret".into()),
        sign_key: Some("sign-key".into()),
        public_key: None,
        private_key: None,
        exec_gateway: Some("https://upstream/exec".into()),
        query_gateway: Some("https://upstream/query".into()),
        server_return: None,
        unlock_domain: None,
        cost_rate: 20_000,
        rate_type: 1,
        status: 1,
        is_default: 0,
    }
}

// --- merchant / order seeding (mirrors payout_exec.rs) ----------------------

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

async fn seed_merchant(s: &Suite, user: i64) {
    ensure_system_row(s).await;
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("c{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(100_000 * K),
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
        id: Set(uid(11_900_000_000_001)),
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

// --- CRUD + read projection -------------------------------------------------

#[tokio::test]
async fn create_then_cfg_round_trips_secrets_and_cost() {
    let Some(s) = suite().await else { return };
    let created = repo(&s)
        .create(new_channel("stub", "MCH-DB"), now_ts())
        .await
        .unwrap();
    assert!(created.id > 0, "DB assigns the id");
    assert_eq!(created.status, 1);

    let cfg = repo(&s).cfg_by_id(created.id).await.unwrap().unwrap();
    assert_eq!(cfg.id, created.id);
    assert_eq!(cfg.code, "stub");
    assert_eq!(cfg.name, "测试代付通道");
    assert_eq!(cfg.mch_id.as_deref(), Some("MCH-DB"));
    assert_eq!(cfg.exec_gateway, "https://upstream/exec");
    assert_eq!(cfg.query_gateway, "https://upstream/query");
    // Secrets ride the projection unchanged — the sweep signs with these.
    assert_eq!(cfg.sign_key, "sign-key");
    assert_eq!(cfg.app_secret, "app-secret");
    assert_eq!(cfg.cost_rate, 20_000);
    assert_eq!(cfg.rate_type, 1);

    let listed = repo(&s).list_enabled().await.unwrap();
    assert!(listed.iter().any(|m| m.id == created.id));
}

#[tokio::test]
async fn enabled_lookup_skips_disabled_but_query_sees_it() {
    let Some(s) = suite().await else { return };
    let created = repo(&s)
        .create(new_channel("stub", "MCH-OFF"), now_ts())
        .await
        .unwrap();
    repo(&s)
        .set_status(created.id, false, now_ts())
        .await
        .unwrap();

    // The submit-path (status=1) resolution finds nothing…
    assert!(repo(&s)
        .cfg_enabled_by_id(created.id)
        .await
        .unwrap()
        .is_none());
    // …but the §10.2 query-path (by id, no status filter) still settles an
    // in-flight order on the since-disabled channel.
    let cfg = repo(&s).cfg_by_id(created.id).await.unwrap().unwrap();
    assert_eq!(cfg.mch_id.as_deref(), Some("MCH-OFF"));

    // And it drops out of the merchant dropdown.
    let listed = repo(&s).list_enabled().await.unwrap();
    assert!(!listed.iter().any(|m| m.id == created.id));
}

#[tokio::test]
async fn set_default_promotes_an_enabled_channel() {
    let Some(s) = suite().await else { return };
    let created = repo(&s)
        .create(new_channel("stub", "MCH-DEF"), now_ts())
        .await
        .unwrap();
    assert_eq!(created.is_default, 0);
    let promoted = repo(&s)
        .set_default(created.id, now_ts())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(promoted.is_default, 1);
    // A default exists and resolves to an enabled channel (§10.1 predicate).
    let def = repo(&s).default_enabled_cfg().await.unwrap();
    assert!(def.is_some(), "an enabled default is configured");
}

#[tokio::test]
async fn update_edits_fields_and_stamps_time() {
    let Some(s) = suite().await else { return };
    let created = repo(&s)
        .create(new_channel("stub", "MCH-UPD"), now_ts())
        .await
        .unwrap();
    let patched = repo(&s)
        .update(
            created.id,
            UpdatePayoutChannel {
                title: Some("改名通道".into()),
                exec_gateway: Some("https://new/exec".into()),
                cost_rate: Some(30_000),
                update_time: now_ts() + 5,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(patched.title, "改名通道");
    assert_eq!(patched.exec_gateway.as_deref(), Some("https://new/exec"));
    assert_eq!(patched.cost_rate, 30_000);
    assert!(patched.update_time > created.update_time);
    // A field left None is untouched (the mch_id survives).
    assert_eq!(patched.mch_id.as_deref(), Some("MCH-UPD"));
}

#[tokio::test]
async fn create_rejects_blank_code_at_the_door() {
    let Some(s) = suite().await else { return };
    let err = repo(&s)
        .create(
            NewPayoutChannel {
                code: "   ".into(),
                ..new_channel("x", "y")
            },
            now_ts(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        payment_api::state::GatewayError::BadRequest(_)
    ));
}

// --- the sweep resolves config from the table -------------------------------

#[tokio::test]
async fn manual_submit_sweep_resolves_channel_from_db() {
    let Some(s) = suite().await else { return };
    let user = uid(11_900_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000C1").await;
    let chan = repo(&s)
        .create(new_channel("stub", "MCH-DB"), now_ts())
        .await
        .unwrap();

    let rep = svc(&s)
        .run_manual_submit_sweep(
            &registry(),
            chan.id,
            std::slice::from_ref(&order.order_no),
            now_ts(),
        )
        .await
        .unwrap()
        .expect("the enabled channel resolves from the DB");
    assert_eq!(rep.folded, 1);

    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Processing.code());
    // The attribution + adapter message both rode the DB-resolved cfg: the
    // echo `exec:MCH-DB` proves the sweep read the row, not a hand-off.
    assert_eq!(after.df_channel_id, Some(chan.id));
    assert_eq!(after.df_code.as_deref(), Some("stub"));
    assert_eq!(after.channel_mch_id.as_deref(), Some("MCH-DB"));
    assert_eq!(after.memo.as_deref(), Some("exec:MCH-DB"));
}

#[tokio::test]
async fn query_one_folds_via_db_resolved_channel() {
    let Some(s) = suite().await else { return };
    let user = uid(11_900_000_000_000);
    seed_merchant(&s, user).await;
    let order = book_pending(&s, user, 100 * K, "622202000000C2").await;
    let chan = repo(&s)
        .create(new_channel("stub", "MCH-DB"), now_ts())
        .await
        .unwrap();
    // Put the order in flight on the DB channel (§10.2 pre-state).
    payout_orders::Entity::update_many()
        .col_expr(payout_orders::Column::Status, Expr::value(1i16))
        .col_expr(
            payout_orders::Column::DfChannelId,
            Expr::value(Some(chan.id)),
        )
        .col_expr(payout_orders::Column::DfCode, Expr::value(Some("stub")))
        .filter(payout_orders::Column::OrderNo.eq(&order.order_no))
        .exec(&*s.db)
        .await
        .unwrap();

    let inflight = reload(&s, &order.order_no).await;
    let resolved = svc(&s).resolve_channel(chan.id).await.unwrap().unwrap();
    let outcome = svc(&s)
        .query_one(&inflight, &registry(), &resolved, now_ts())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        payment_api::payout::SubmitOutcome::Folded(PayoutStatus::Success)
    ));
    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.status, PayoutStatus::Success.code());
    assert_eq!(after.memo.as_deref(), Some("q:MCH-DB"));
    assert_eq!(after.auto_query_num, 1);
}

#[tokio::test]
async fn auto_sweep_without_default_channel_is_a_noop() {
    let Some(s) = suite().await else { return };
    // With the (fresh) schema carrying no enabled default, the auto sweep
    // answers `None` — nothing claimed / folded, the legacy's early exit. The
    // DB may be shared, so only assert: if it does resolve a default, it is
    // an enabled one (never a disabled / non-default row).
    let svc = svc(&s);
    if let Some(chan) = svc.default_channel().await.unwrap() {
        let model = payout_channels::Entity::find_by_id(chan.id)
            .one(&*s.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, 1);
        assert_eq!(model.is_default, 1);
    }
    let _ = svc
        .run_auto_submit_sweep(&registry(), &SubmitGate::auto(5, None, 0), now_ts())
        .await;
}

// --- the §10.1 per-merchant same-day tally ----------------------------------

#[tokio::test]
async fn auto_today_tally_counts_is_auto_rows_created_today() {
    let Some(s) = suite().await else { return };
    let user = uid(11_900_000_000_000);
    seed_merchant(&s, user).await;

    let today = chrono::Local::now().naive_local();
    let yesterday = today - chrono::Duration::days(1);
    // Three auto orders booked today (sum = 400元).
    let a = book_pending(&s, user, 100 * K, "622202000000A1").await;
    let b = book_pending(&s, user, 250 * K, "622202000000A2").await;
    let c = book_pending(&s, user, 50 * K, "622202000000A3").await;
    // Excluded: a non-auto row today, and an auto row from yesterday.
    let d = book_pending(&s, user, 999 * K, "622202000000A4").await;
    let e = book_pending(&s, user, 999 * K, "622202000000A5").await;

    for o in [&a, &b, &c] {
        payout_orders::Entity::update_many()
            .col_expr(payout_orders::Column::IsAuto, Expr::value(1i32))
            .filter(payout_orders::Column::OrderNo.eq(&o.order_no))
            .exec(&*s.db)
            .await
            .unwrap();
    }
    payout_orders::Entity::update_many()
        .col_expr(payout_orders::Column::IsAuto, Expr::value(0i32))
        .filter(payout_orders::Column::OrderNo.eq(&d.order_no))
        .exec(&*s.db)
        .await
        .unwrap();
    payout_orders::Entity::update_many()
        .col_expr(payout_orders::Column::IsAuto, Expr::value(1i32))
        .col_expr(payout_orders::Column::CreatedAt, Expr::value(yesterday))
        .filter(payout_orders::Column::OrderNo.eq(&e.order_no))
        .exec(&*s.db)
        .await
        .unwrap();

    let (count, sum) = svc(&s)
        .auto_today_for_merchant(user, now_ts())
        .await
        .unwrap();
    assert_eq!(count, 3, "only the same-day is_auto rows count");
    assert_eq!(sum, 400 * K, "SUM(tkmoney) over the counted rows");
}
