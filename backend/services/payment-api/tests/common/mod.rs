//! Shared harness for the DB-gated notify/reissue integration tests:
//! the per-test pool + advisory-locked migration (same isolation rules as
//! `ledger_db.rs`), a clock-based id sequence parameterised per binary
//! (parallel test binaries must not collide), a one-shot loopback mock
//! merchant server, and the settle-to-status-1 world seeding.

#![allow(dead_code)] // each binary uses a subset

use std::io::{Read, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter, Set,
};

use payment_api::data::{channels, members, orders};
use payment_api::gateway::notify::MerchantNotifier;
use payment_api::ledger::{LedgerService, NewOrder, SettleOutcome};
use payment_api::migration;
use payment_api::rate::ResolvedRate;

pub struct Suite {
    pub db: Arc<DatabaseConnection>,
    pub ledger: Arc<LedgerService>,
    pub notifier: Arc<MerchantNotifier>,
}

pub async fn suite() -> Option<Suite> {
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
    let notifier = Arc::new(MerchantNotifier::new((*db).clone(), ledger.clone()));
    Some(Suite {
        db,
        ledger,
        notifier,
    })
}

/// Distinct high ids over a per-binary `base` (parallel binaries share the
/// database, so each passes a disjoint base); the clock mix is fixed once
/// per process, keeping the sequence strictly increasing within a binary.
pub fn uid(base: i64) -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static MIX: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let mix = *MIX.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 * 1_000 + d.subsec_millis() as i64)
            .unwrap_or(0)
            % 900_000_000_000
    });
    base + mix + SEQ.fetch_add(1, Ordering::SeqCst)
}

/// A one-shot loopback HTTP server answering the merchant notify with
/// `reply`; resolves to (base URL, join handle yielding the request bytes).
pub fn mock_merchant(reply: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock bind");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("mock accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until the header terminator; the small form body rides the
        // same or a following packet (best effort, the socket is drained
        // on close either way).
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let resp = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            reply.len(),
            reply
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        buf
    });
    (format!("http://127.0.0.1:{port}/notify"), handle)
}

/// A merchant (apikey set, `parentid = 1` platform boundary) plus a live
/// channel for the settle rate read.
pub async fn seed_world(s: &Suite, user: i64, channel: i64) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("n{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        apikey: Set(Some("32charapikey000000000000000000aa".into())),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    channels::ActiveModel {
        id: Set(channel),
        code: Set("TEST".into()),
        title: Set("t".into()),
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

/// Create and settle a T+0 order, returning its id — status is exactly 1
/// afterwards, where the §7.1 sweep set begins.
pub async fn settled_order(s: &Suite, user: i64, channel: i64, notify_url: &str, order_id: &str) {
    settled_order_with_return(s, user, channel, notify_url, "", order_id).await;
}

/// The same, with the sync page-return address stored too (the dispatch's
/// `cred_for` pair), for the `/callback/{code}` route tests.
pub async fn settled_order_with_return(
    s: &Suite,
    user: i64,
    channel: i64,
    notify_url: &str,
    callback_url: &str,
    order_id: &str,
) {
    create_only(s, user, channel, notify_url, callback_url, order_id).await;
    let outcome = s.ledger.settle_order(order_id).await.unwrap();
    assert!(matches!(outcome, SettleOutcome::Settled(_)));
}

/// Create the order WITHOUT settling — status stays 0 (the sync-callback
/// unpaid path, the query NOTPAY shape).
pub async fn create_only(
    s: &Suite,
    user: i64,
    channel: i64,
    notify_url: &str,
    callback_url: &str,
    order_id: &str,
) {
    let new = NewOrder {
        user_id: user,
        order_id: order_id.into(),
        amount_units: 1_000_000,
        rate: ResolvedRate {
            feilv: 8_000,
            fengding: 0,
        },
        cost_rate: 8_000,
        t: 0,
        bank_code: "903".into(),
        channel_code: "TEST".into(),
        notify_url: notify_url.into(),
        callback_url: callback_url.into(),
        channel_id: channel,
        account_id: 1,
        sign_key: None,
        app_id: None,
        attach: Some("meta".into()),
        product_name: None,
    };
    s.ledger.create_order(&new).await.unwrap();
}

pub async fn order_row(db: &DatabaseConnection, order_id: &str) -> orders::Model {
    orders::Entity::find()
        .filter(orders::Column::OrderId.eq(order_id))
        .one(db)
        .await
        .unwrap()
        .unwrap()
}
