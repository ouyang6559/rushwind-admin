//! DB-gated T+1 thaw-cron integration tests (`spec/02` §6.2): the due-list
//! scan over `blocked_logs` (thaw_time arrived with the 7200s buffer,
//! created before today, status 0), the per-row release through
//! `run_thaw`, and the freeze-ledger CAS that makes overlapping sweeps
//! idempotent. Runs only with `PAYMENT_TEST_DATABASE_URL` set; harness
//! shared via `tests/common` (base 9.9e12 keeps ids disjoint).
//!
//! Like the reissue sweep, this one is a GLOBAL scan — cases serialise on
//! an in-process mutex so no parallel sweep releases another's freeze,
//! and assertions are directional (our log row, our member's buckets).

mod common;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use common::{seed_world, suite, uid};
use payment_api::data::{blocked_logs, members};
use payment_api::ledger::NewOrder;
use payment_api::rate::ResolvedRate;

const BASE: i64 = 9_900_000_000_000;

static SERIALIZED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn alone() -> tokio::sync::MutexGuard<'static, ()> {
    SERIALIZED.lock().await
}

fn today_midnight() -> i64 {
    let now = chrono::Local::now();
    let day = now.date_naive().and_hms_opt(0, 0, 0).expect("midnight");
    day.and_local_timezone(chrono::Local)
        .single()
        .expect("local midnight")
        .timestamp()
}

/// Create and settle a T+1 order: the net arrives in `blocked_balance`
/// and one `blocked_logs` row (status 0) records the freeze.
async fn settle_t1(s: &common::Suite, user: i64, channel: i64, oid: &str) {
    let new = NewOrder {
        user_id: user,
        order_id: oid.into(),
        amount_units: 1_000_000,
        rate: ResolvedRate {
            feilv: 8_000,
            fengding: 0,
        },
        cost_rate: 8_000,
        t: 1,
        bank_code: "903".into(),
        channel_code: "TEST".into(),
        notify_url: String::new(),
        callback_url: String::new(),
        channel_id: channel,
        account_id: 1,
        sign_key: None,
        app_id: None,
        attach: None,
        product_name: None,
    };
    s.ledger.create_order(&new).await.unwrap();
    s.ledger.settle_order(oid).await.unwrap();
}

async fn log_for_user(db: &sea_orm::DatabaseConnection, user: i64) -> blocked_logs::Model {
    blocked_logs::Entity::find()
        .filter(blocked_logs::Column::UserId.eq(user))
        .one(db)
        .await
        .unwrap()
        .expect("the t=1 settle wrote a blocked_log")
}

/// Backdate a freeze so the §6.2 window matches: due yesterday, thaw time
/// exactly at today's midnight (inside the +7200 buffer).
async fn make_due(db: &sea_orm::DatabaseConnection, log: &blocked_logs::Model) {
    let mut am: blocked_logs::ActiveModel = log.clone().into();
    am.thaw_time = Set(today_midnight());
    am.create_time = Set(today_midnight() - 1);
    am.update(db).await.unwrap();
}

async fn member_row(db: &sea_orm::DatabaseConnection, user: i64) -> members::Model {
    members::Entity::find_by_id(user)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn sweep_releases_a_due_t1_freeze() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let oid = format!("T{}", uid(BASE));
    settle_t1(&s, user, channel, &oid).await;

    let log = log_for_user(&s.db, user).await;
    assert_eq!((log.status, log.amount), (0, 992_000)); // 100 元 - 0.8% 手续费
    assert_eq!(
        (
            member_row(&s.db, user).await.balance,
            member_row(&s.db, user).await.blocked_balance
        ),
        (0, 992_000)
    );
    make_due(&s.db, &log).await;

    let report = s.ledger.run_t1_thaw_sweep().await.unwrap();
    assert!(report.released >= 1, "{report:?}");
    assert_eq!(report.released + report.skipped, report.scanned);

    // blocked → available, the freeze ledger flipped, the flow rode the tx.
    let m = member_row(&s.db, user).await;
    assert_eq!((m.balance, m.blocked_balance), (992_000, 0));
    assert_eq!(log_for_user(&s.db, user).await.status, 1);
}

#[tokio::test]
async fn fresh_freezes_are_not_due() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let oid = format!("T{}", uid(BASE));
    settle_t1(&s, user, channel, &oid).await;

    // The natural write (tomorrow + rand, created today) fails BOTH §6.2
    // guards: thaw_time is beyond today+7200 AND create_time is not before
    // today's midnight.
    let log = log_for_user(&s.db, user).await;
    let due = s.ledger.due_t1_thaws(today_midnight()).await.unwrap();
    assert!(!due.iter().any(|r| r.id == log.id), "{due:?}");
    assert_eq!(log_for_user(&s.db, user).await.status, 0);
}

#[tokio::test]
async fn overlapping_sweeps_release_once() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let oid = format!("T{}", uid(BASE));
    settle_t1(&s, user, channel, &oid).await;
    let log = log_for_user(&s.db, user).await;
    make_due(&s.db, &log).await;

    // First sweep releases; the row leaves the due list (status CAS), so a
    // second overlapping cron can never double-credit the member.
    s.ledger.run_t1_thaw_sweep().await.unwrap();
    let after_first = member_row(&s.db, user).await.balance;
    let second = s.ledger.run_t1_thaw_sweep().await.unwrap();
    let due = s.ledger.due_t1_thaws(today_midnight()).await.unwrap();
    assert!(!due.iter().any(|r| r.id == log.id), "{due:?}");
    assert_eq!(second.released, 0, "{second:?}");
    assert_eq!(member_row(&s.db, user).await.balance, after_first);
}
