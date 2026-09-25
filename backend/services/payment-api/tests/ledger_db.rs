//! DB-gated integration tests for the Phase-3 ledger seams (`spec/02` §11).
//! They run against a real Postgres only when `PAYMENT_TEST_DATABASE_URL` is
//! set — otherwise every test prints a skip note and returns, keeping plain
//! `cargo test` offline-green. The schema is migrated once per process via a
//! `tokio::sync::OnceCell`; ids come from a high atomic sequence so reruns
//! and shared databases never collide.
//!
//! Local harness:
//! ```text
//! docker run -d --name juhepay-pg -e POSTGRESQL_USERNAME=postgres \
//!   -e POSTGRESQL_PASSWORD='*Abcd123456' -e POSTGRESQL_DATABASE=juhepay \
//!   -p 5432:5432 bitnami/postgresql:latest
//! PAYMENT_TEST_DATABASE_URL='postgres://postgres:*Abcd123456@127.0.0.1:5432/juhepay' \
//!   cargo test -p payment-api --test ledger_db
//! ```

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter, Set,
};

use payment_api::data::{blocked_logs, channels, members, money_changes, user_rates};
use payment_api::ledger::{LedgerService, NewOrder, SettleOutcome, ThawKind};
use payment_api::migration;
use payment_api::rate::ResolvedRate;
use payment_api::state::GatewayError;

struct Suite {
    db: Arc<DatabaseConnection>,
    ledger: Arc<LedgerService>,
}

/// Each test owns its pool (a sqlx pool created on one `#[tokio::test]`
/// runtime dies with it, so it must never be shared across tests); the
/// one-shot schema migration is serialised with a Postgres advisory lock.
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
    Some(Suite { db, ledger })
}

/// Distinct high ids, based on the process clock so reruns never collide
/// with rows a previous run left in a shared database.
fn uid() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let base = *BASE.get_or_init(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 * 1_000 + d.subsec_millis() as i64)
            .unwrap_or(0);
        9_100_000_000_000 + now % 900_000_000_000
    });
    base + SEQ.fetch_add(1, Ordering::SeqCst)
}

async fn seed_member(s: &Suite, id: i64, parentid: i64) {
    members::ActiveModel {
        id: Set(id),
        username: Set(format!("u{id}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(parentid),
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

async fn seed_channel(s: &Suite, id: i64, default_rate: i64, t0_default_rate: i64) {
    channels::ActiveModel {
        id: Set(id),
        code: Set("TEST".into()),
        title: Set("test channel".into()),
        default_rate: Set(default_rate),
        fengding: Set(0),
        t0_default_rate: Set(t0_default_rate),
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

async fn seed_user_rate(s: &Suite, user_id: i64, channel_id: i64, rate: i64) {
    user_rates::ActiveModel {
        user_id: Set(user_id),
        channel_id: Set(channel_id),
        rate: Set(rate),
        fengding: Set(0),
        t0_rate: Set(rate),
        t0_fengding: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// A 100元 order at 0.6% (fee 6_000, arrival 994_000), 0.4% cost.
fn new_order(user_id: i64, channel_id: i64, t: i32) -> NewOrder {
    NewOrder {
        user_id,
        order_id: format!("TEST{channel_id}T{t}{}", uid()),
        amount_units: 1_000_000,
        rate: ResolvedRate {
            feilv: 6_000,
            fengding: 0,
        },
        cost_rate: 4_000,
        t,
        bank_code: "7".into(),
        channel_code: "TEST".into(),
        notify_url: "http://merchant/notify".into(),
        callback_url: "http://merchant/back".into(),
        channel_id,
        account_id: 0,
        sign_key: None,
        app_id: None,
        attach: None,
        product_name: None,
    }
}

async fn member_balance(s: &Suite, id: i64) -> (i64, i64) {
    let m = members::Entity::find_by_id(id)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    (m.balance, m.blocked_balance)
}

async fn flows(s: &Suite, trans_id: &str) -> Vec<money_changes::Model> {
    money_changes::Entity::find()
        .filter(money_changes::Column::TransId.eq(trans_id))
        .all(&*s.db)
        .await
        .unwrap()
}

fn skip() {
    eprintln!("skip: PAYMENT_TEST_DATABASE_URL is not set");
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn create_order_persists_frozen_snapshot_and_rejects_dup() {
    let Some(s) = suite().await else {
        return skip();
    };
    let user = uid();
    let ch = uid();
    seed_member(&s, user, 1).await;
    seed_channel(&s, ch, 5_000, 5_000).await;

    let order = s
        .ledger
        .create_order(&new_order(user, ch, 0))
        .await
        .unwrap();
    assert_eq!(order.status, 0);
    assert_eq!(order.amount, 1_000_000);
    assert_eq!(order.poundage, 6_000);
    assert_eq!(order.actual_amount, 994_000);
    assert_eq!(order.cost, 4_000);
    assert_eq!(order.mch_id, (user + 10_000).to_string());

    // The unique index turns a replayed order id into the legacy-style 4xx.
    let dup = new_order(user, ch, 0);
    let dup = NewOrder {
        order_id: order.order_id.clone(),
        ..dup
    };
    let err = s.ledger.create_order(&dup).await.unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "订单已存在"),
        "{err:?}"
    );

    // Admission still guards the kernel (zero amount never reaches INSERT).
    let bad = NewOrder {
        amount_units: 0,
        ..new_order(user, ch, 0)
    };
    let err = s.ledger.create_order(&bad).await.unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "金额错误"),
        "{err:?}"
    );
}

#[tokio::test]
async fn settle_t0_credits_available_once() {
    let Some(s) = suite().await else {
        return skip();
    };
    let user = uid();
    let ch = uid();
    seed_member(&s, user, 1).await; // parentid 1 → platform, no brokerage
    seed_channel(&s, ch, 5_000, 5_000).await;
    let order = s
        .ledger
        .create_order(&new_order(user, ch, 0))
        .await
        .unwrap();

    let outcome = s.ledger.settle_order(&order.order_id).await.unwrap();
    assert!(matches!(outcome, SettleOutcome::Settled(_)), "{outcome:?}");
    assert_eq!(member_balance(&s, user).await, (994_000, 0));
    let row = s.ledger.find_order(&order.order_id).await.unwrap().unwrap();
    assert_eq!(row.status, 1);
    assert!(row.success_date.is_some());

    let fs = flows(&s, &order.order_id).await;
    assert_eq!(fs.len(), 1);
    assert_eq!(fs[0].lx, 1);
    assert_eq!(fs[0].money, 994_000);
    assert_eq!(fs[0].g_money, 994_000);

    // A duplicate callback is a no-op on money (CAS dedupe).
    let again = s.ledger.settle_order(&order.order_id).await.unwrap();
    assert_eq!(again, SettleOutcome::AlreadySettled);
    assert_eq!(member_balance(&s, user).await, (994_000, 0));
    assert_eq!(flows(&s, &order.order_id).await.len(), 1);
}

#[tokio::test]
async fn settle_t1_blocks_then_thaw_releases() {
    let Some(s) = suite().await else {
        return skip();
    };
    let user = uid();
    let ch = uid();
    seed_member(&s, user, 1).await;
    seed_channel(&s, ch, 5_000, 5_000).await;
    let order = s
        .ledger
        .create_order(&new_order(user, ch, 1))
        .await
        .unwrap();

    let outcome = s.ledger.settle_order(&order.order_id).await.unwrap();
    assert!(matches!(outcome, SettleOutcome::Settled(_)));
    assert_eq!(member_balance(&s, user).await, (0, 994_000));
    let log = blocked_logs::Entity::find()
        .filter(blocked_logs::Column::OrderId.eq(&order.order_id))
        .one(&*s.db)
        .await
        .unwrap()
        .expect("T+1 settle wrote a freeze ledger");
    assert_eq!((log.status, log.amount), (0, 994_000));

    let released = s
        .ledger
        .run_thaw(ThawKind::T1Blocked, user, log.amount, Some(log.id))
        .await
        .unwrap()
        .expect("due log released");
    assert_eq!(released.flow.lx, 8);
    assert_eq!(member_balance(&s, user).await, (994_000, 0));
    let log = blocked_logs::Entity::find_by_id(log.id)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(log.status, 1);
    // The flow rides the release (lx 8, available-bucket snapshot).
    let fs = money_changes::Entity::find()
        .filter(money_changes::Column::RequestId.eq(format!("thaw:{}", log.id)))
        .all(&*s.db)
        .await
        .unwrap();
    assert_eq!(fs.len(), 1);
    assert_eq!((fs[0].y_money, fs[0].g_money), (0, 994_000));

    // Sweeping again hits the hint overdraft guard (blocked is now 0).
    let err = s
        .ledger
        .run_thaw(ThawKind::T1Blocked, user, log.amount, Some(log.id))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "冻结余额不足"),
        "{err:?}"
    );
}

#[tokio::test]
async fn settle_credits_agent_chain_by_rate_diff() {
    let Some(s) = suite().await else {
        return skip();
    };
    let merchant = uid();
    let parent = uid();
    let ch = uid();
    seed_member(&s, parent, 1).await;
    seed_member(&s, merchant, parent).await;
    // Channel default 0.5% (the parent's rate); merchant override 0.6%.
    seed_channel(&s, ch, 5_000, 5_000).await;
    seed_user_rate(&s, merchant, ch, 6_000).await;
    let order = s
        .ledger
        .create_order(&new_order(merchant, ch, 0))
        .await
        .unwrap();

    let outcome = s.ledger.settle_order(&order.order_id).await.unwrap();
    let SettleOutcome::Settled(w) = outcome else {
        panic!("expected settled");
    };
    assert_eq!(w.brokerage_flows.len(), 1);
    // 100元 × (0.6% − 0.5%) = 0.10元 = 1_000 units to the parent.
    assert_eq!(w.brokerage_flows[0].user_id, parent);
    assert_eq!(w.brokerage_flows[0].money, 1_000);
    assert_eq!(member_balance(&s, merchant).await, (994_000, 0));
    assert_eq!(member_balance(&s, parent).await, (1_000, 0));
    let fs = flows(&s, &order.order_id).await;
    let profit = fs.iter().find(|f| f.lx == 9).expect("lx 9 flow");
    assert_eq!(profit.money, 1_000);
    assert_eq!(profit.user_id, parent);
}

#[tokio::test]
async fn mark_order_notified_is_a_cas() {
    let Some(s) = suite().await else {
        return skip();
    };
    let user = uid();
    let ch = uid();
    seed_member(&s, user, 1).await;
    seed_channel(&s, ch, 5_000, 5_000).await;
    let order = s
        .ledger
        .create_order(&new_order(user, ch, 0))
        .await
        .unwrap();

    // 0 -> 2 is not a legal transition.
    assert!(!s.ledger.mark_order_notified(&order.order_id).await.unwrap());
    s.ledger.settle_order(&order.order_id).await.unwrap();
    // 1 -> 2 once, then the CAS idles.
    assert!(s.ledger.mark_order_notified(&order.order_id).await.unwrap());
    assert!(!s.ledger.mark_order_notified(&order.order_id).await.unwrap());
    let row = s.ledger.find_order(&order.order_id).await.unwrap().unwrap();
    assert_eq!(row.status, 2);
}

#[tokio::test]
async fn settle_rejects_unknown_order() {
    let Some(s) = suite().await else {
        return skip();
    };
    let err = s.ledger.settle_order("TEST-nope").await.unwrap_err();
    assert!(
        matches!(&err, GatewayError::BadRequest(m) if m == "订单不存在"),
        "{err:?}"
    );
}
