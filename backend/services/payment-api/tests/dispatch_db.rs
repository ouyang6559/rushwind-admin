//! DB-gated integration tests for the Phase-3 channel dispatch tail
//! (`spec/03` §2.3–2.4, §4.2) — the same harness rules as `ledger_db.rs`:
//! they need `PAYMENT_TEST_DATABASE_URL` set (skipped otherwise), each test
//! owns its pool, and ids come from a clock-based high sequence.
//!
//! Local harness:
//! ```text
//! docker run -d --name juhepay-pg -e POSTGRESQL_USERNAME=postgres \
//!   -e POSTGRESQL_PASSWORD='*Abcd123456' -e POSTGRESQL_DATABASE=juhepay \
//!   -p 5432:5432 bitnami/postgresql:latest
//! PAYMENT_TEST_DATABASE_URL='postgres://postgres:*Abcd123456@127.0.0.1:5432/juhepay' \
//!   cargo test -p payment-api --test dispatch_db
//! ```

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter, Set,
};

use payment_api::channel::{ChannelRegistry, PayOut};
use payment_api::data::{channel_accounts, channels, orders, product_users, products};
use payment_api::gateway::dispatch::{dispatch_core, DispatchReq};
use payment_api::ledger::LedgerService;
use payment_api::migration;
use payment_api::risk::counters::ymd;
use payment_api::risk::RiskGate;
use payment_api::state::GatewayError;
use redis::aio::ConnectionManager;

struct Suite {
    db: Arc<DatabaseConnection>,
    ledger: Arc<LedgerService>,
    registry: Arc<ChannelRegistry>,
}

async fn suite() -> Option<Suite> {
    let url = std::env::var("PAYMENT_TEST_DATABASE_URL").ok()?;
    let db = Arc::new(
        Database::connect(url.as_str())
            .await
            .expect("test postgres reachable"),
    );
    db.execute_unprepared("SELECT pg_advisory_lock(778811)")
        .await
        .expect("advisory lock");
    migration::migrate(&db).await.expect("migrate test schema");
    db.execute_unprepared("SELECT pg_advisory_unlock(778811)")
        .await
        .expect("advisory unlock");
    let ledger = Arc::new(LedgerService::new((*db).clone()));
    let registry = Arc::new(ChannelRegistry::assemble(["WxSm"]));
    Some(Suite {
        db,
        ledger,
        registry,
    })
}

fn uid() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let base = *BASE.get_or_init(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 * 1_000 + d.subsec_millis() as i64)
            .unwrap_or(0);
        9_300_000_000_000 + now % 900_000_000_000
    });
    base + SEQ.fetch_add(1, Ordering::SeqCst)
}

async fn seed_member(s: &Suite, id: i64) {
    payment_api::data::members::ActiveModel {
        id: Set(id),
        username: Set(format!("d{id}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// A live `WxSm` channel with a 0.8% T+0 default fee.
async fn seed_channel(s: &Suite, id: i64) {
    channels::ActiveModel {
        id: Set(id),
        code: Set("WxSm".into()),
        title: Set("wx scan".into()),
        mch_id: Set(Some("CH-MCH".into())),
        sign_key: Set(Some("ch-key".into())),
        default_rate: Set(6_000),
        fengding: Set(0),
        t0_default_rate: Set(8_000),
        t0_fengding: Set(0),
        status: Set(1),
        paytype: Set(1),
        control_status: Set(0),
        offline_status: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn seed_account(s: &Suite, id: i64, channel_id: i64, weight: i32, status: i32) {
    channel_accounts::ActiveModel {
        id: Set(id),
        channel_id: Set(channel_id),
        title: Set(Some("acct".into())),
        sign_key: Set(Some(format!("acc-key-{id}"))),
        weight: Set(weight),
        status: Set(status),
        default_rate: Set(0),
        fengding: Set(0),
        t0_default_rate: Set(0),
        t0_fengding: Set(0),
        custom_rate: Set(0),
        control_status: Set(0),
        offline_status: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// An open product bound to `channel_id`, assigned to the merchant with a
/// pinned single channel (polling off).
async fn seed_product(s: &Suite, id: i64, user_id: i64, channel_id: i64) {
    products::ActiveModel {
        id: Set(id),
        name: Set("扫码收款".into()),
        code: Set("WxSm".into()),
        polling: Set(0),
        paytype: Set(1),
        status: Set(1),
        isdisplay: Set(1),
        channel: Set(channel_id),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    product_users::ActiveModel {
        user_id: Set(user_id),
        pid: Set(id),
        polling: Set(0),
        status: Set(1),
        channel: Set(channel_id),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

fn req(user_id: i64, product_id: i64, order_id: &str) -> DispatchReq {
    DispatchReq {
        user_id,
        order_id: order_id.into(),
        amount_units: 1_000_000, // 100.00元
        bank_code: product_id.to_string(),
        notify_url: "https://merchant.test/notify".into(),
        callback_url: "https://merchant.test/return".into(),
        product_name: None,
        attach: None,
    }
}

async fn dispatch(s: &Suite, r: DispatchReq) -> Result<(orders::Model, PayOut), GatewayError> {
    dispatch_core(&s.db, &s.ledger, &s.registry, "http://test.local/", None, r).await
}

/// Happy path: the order lands frozen (channel/account/rate/cost snapshots,
/// status 0) and the WxSm adapter redirects to its gateway carrying the
/// generated platform notify URL and the account-level signing key.
#[tokio::test]
async fn happy_path_stores_order_and_redirects() {
    let Some(s) = suite().await else {
        println!("skip: PAYMENT_TEST_DATABASE_URL not set");
        return;
    };
    let (user, product, channel, account) = (uid(), uid(), uid(), uid());
    seed_member(&s, user).await;
    seed_channel(&s, channel).await;
    seed_account(&s, account, channel, 1, 1).await;
    seed_product(&s, product, user, channel).await;

    let order_id = format!("D{}", uid());
    let (order, payout) = dispatch(&s, req(user, product, &order_id)).await.unwrap();

    // 100元 at the 0.8% T+0 default (no userrate row, no tikuan row → t=0).
    assert_eq!(order.amount, 1_000_000);
    assert_eq!(order.poundage, 8_000);
    assert_eq!(order.actual_amount, 992_000);
    assert_eq!(order.cost, 8_000);
    assert_eq!(order.status, 0);
    assert_eq!(order.t, 0);
    assert_eq!(order.channel_id, channel);
    assert_eq!(order.account_id, account);
    assert_eq!(order.bank_code, product.to_string());
    assert_eq!(order.notify_url, "https://merchant.test/notify");
    // The signing snapshot follows the account-first `?:` chain.
    assert!(order.sign_key.as_deref().unwrap().starts_with("acc-key-"));

    let PayOut::Redirect { url } = payout else {
        panic!("expected a redirect payout");
    };
    assert!(url.starts_with("https://pay.pinyewang.com/submit.php?"));
    assert!(url.contains(&format!("out_trade_no={order_id}")));
    assert!(
        url.contains("notify_url=http%3A%2F%2Ftest.local%2Fnotify%2FWxSm")
            || url.contains("notify_url=http://test.local/notify/WxSm")
    );

    // The row is really in the DB (not just a returned model).
    let stored = orders::Entity::find()
        .filter(orders::Column::OrderId.eq(order_id.clone()))
        .one(&*s.db)
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(stored.status, 0);
}

/// A repeat wire order id hits the unique-index idempotency net.
#[tokio::test]
async fn duplicate_order_id_is_rejected() {
    let Some(s) = suite().await else {
        println!("skip: PAYMENT_TEST_DATABASE_URL not set");
        return;
    };
    let (user, product, channel, account) = (uid(), uid(), uid(), uid());
    seed_member(&s, user).await;
    seed_channel(&s, channel).await;
    seed_account(&s, account, channel, 1, 1).await;
    seed_product(&s, product, user, channel).await;

    let order_id = format!("D{}", uid());
    dispatch(&s, req(user, product, &order_id)).await.unwrap();
    let err = dispatch(&s, req(user, product, &order_id))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "订单已存在"),
        "{err:?}"
    );
}

/// The §2.3 gate messages: closed product, unassigned merchant, dead pool.
#[tokio::test]
async fn routing_gates_carry_the_legacy_messages() {
    let Some(s) = suite().await else {
        println!("skip: PAYMENT_TEST_DATABASE_URL not set");
        return;
    };
    let (user, product, channel) = (uid(), uid(), uid());
    seed_member(&s, user).await;
    seed_channel(&s, channel).await;

    // No product row at all → 通道关闭中.
    let err = dispatch(&s, req(user, product, &format!("D{}", uid())))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "通道关闭中,暂时无法连接!"),
        "{err:?}"
    );

    // Open product but no product_user assignment → 用户未分配通道.
    products::ActiveModel {
        id: Set(product),
        name: Set("p".into()),
        code: Set("WxSm".into()),
        polling: Set(0),
        paytype: Set(1),
        status: Set(1),
        isdisplay: Set(1),
        channel: Set(channel),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    let err = dispatch(&s, req(user, product, &format!("D{}", uid())))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "用户未分配通道,暂时无法连接!"),
        "{err:?}"
    );

    // Assigned, but every sub-account is disabled → 服务器维护中.
    product_users::ActiveModel {
        user_id: Set(user),
        pid: Set(product),
        polling: Set(0),
        status: Set(1),
        channel: Set(channel),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    seed_account(&s, uid(), channel, 1, 0).await;
    let err = dispatch(&s, req(user, product, &format!("D{}", uid())))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "服务器维护中,请稍后再试..."),
        "{err:?}"
    );
}

async fn redis() -> ConnectionManager {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://:*Abcd123456@127.0.0.1:6379/".to_string());
    redis::Client::open(url)
        .expect("redis url")
        .get_connection_manager()
        .await
        .expect("redis reachable")
}

/// The gate-wired CRC screen on the pinned single channel (`spec/06` §4.1):
/// the Redis offline marker answers `通道:已下线`, a breached day cap
/// answers the rule message — and neither stores an order.
#[tokio::test]
async fn gate_screens_the_pinned_channel() {
    let Some(s) = suite().await else {
        return;
    };
    let mut redis = redis().await;
    let gate = RiskGate::new(redis.clone());

    let user = uid();
    let ch = uid();
    let product = uid();
    let acct = uid();
    seed_member(&s, user).await;
    seed_channel(&s, ch).await; // control/offline default 0
    seed_account(&s, acct, ch, 1, 1).await;
    seed_product(&s, product, user, ch).await;
    // Light the CRC screen: controlled + online.
    s.db.execute_unprepared(&format!(
        "UPDATE channels SET control_status = 1, offline_status = 1 WHERE id = {ch}"
    ))
    .await
    .unwrap();

    // ① the trip's offline marker drops the candidate (screen before rules).
    redis::cmd("SET")
        .arg(format!("risk:offline:channel:{ch}"))
        .arg("1")
        .arg("EX")
        .arg(3600)
        .query_async::<redis::Value>(&mut redis)
        .await
        .unwrap();
    let err = dispatch_core(
        &s.db,
        &s.ledger,
        &s.registry,
        "http://test.local/",
        Some(&gate),
        req(user, product, &format!("G1-{}", uid())),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "通道:已下线"),
        "{err:?}"
    );

    // ② back online, but the day bucket already breaches the cap.
    redis::cmd("DEL")
        .arg(format!("risk:offline:channel:{ch}"))
        .query_async::<i64>(&mut redis)
        .await
        .unwrap();
    s.db.execute_unprepared(&format!(
        "UPDATE channels SET all_money = 1 WHERE id = {ch}"
    ))
    .await
    .unwrap();
    let today = ymd(chrono::Utc::now().timestamp());
    redis::cmd("SET")
        .arg(format!("risk:daily:channel:{ch}:{today}"))
        .arg("10000000")
        .arg("EX")
        .arg(86_400)
        .query_async::<redis::Value>(&mut redis)
        .await
        .unwrap();
    let err = dispatch_core(
        &s.db,
        &s.ledger,
        &s.registry,
        "http://test.local/",
        Some(&gate),
        req(user, product, &format!("G2-{}", uid())),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "通道:当天总交易金额超额!"),
        "{err:?}"
    );
}
