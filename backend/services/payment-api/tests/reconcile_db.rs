//! Reconciliation statement integration (`spec/02` §8). Needs a real
//! Postgres; runs alone:
//!
//! ```sh
//! docker start juhepay-pg
//! cargo test -p payment-api --test reconcile_db
//! ```
//!
//! Orders are seeded as plain rows (the statement domain only reads the
//! order table's windows and status — no funds ride here), with
//! apply/success stamps placed deliberately across the two windows the
//! legacy mixes.
#![allow(dead_code)]

use chrono::{Local, NaiveDate};
use payment_api::data::{orders, reconciliations};
use payment_api::reconcile::{day, day_window};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

mod common;
use common::{seed_world, suite, uid};

const BASE: i64 = 10_700_000_000_000;

fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// Insert one order row with explicit window stamps; `money` is
/// (actual_amount, poundage).
async fn order_on(
    db: &sea_orm::DatabaseConnection,
    user: i64,
    oid: &str,
    apply_ts: i64,
    success_ts: Option<i64>,
    status: i32,
    money: (i64, i64),
) {
    let (actual, poundage) = money;
    let model = orders::ActiveModel {
        user_id: Set(user),
        order_id: Set(oid.to_string()),
        mch_id: Set((user + 10_000).to_string()),
        amount: Set(actual + poundage),
        poundage: Set(poundage),
        actual_amount: Set(actual),
        cost: Set(poundage),
        apply_date: Set(apply_ts),
        success_date: Set(success_ts),
        bank_code: Set("903".into()),
        notify_url: Set(String::new()),
        callback_url: Set(String::new()),
        status: Set(status),
        channel_id: Set(1),
        account_id: Set(1),
        t: Set(0),
        lock_status: Set(0),
        num: Set(0),
        last_reissue_time: Set(0),
        ..Default::default()
    };
    model.insert(db).await.unwrap();
}

async fn rows(
    db: &sea_orm::DatabaseConnection,
    user: i64,
    date: NaiveDate,
) -> Vec<reconciliations::Model> {
    reconciliations::Entity::find()
        .filter(reconciliations::Column::UserId.eq(user))
        .filter(reconciliations::Column::Date.eq(date))
        .all(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn aggregates_the_day_in_the_legacy_mixed_windows() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;
    let t = today();
    let noon = day_window(t).0 + 43_200;
    let yesterday_noon = noon - 86_400;

    // Today-created unpaid; today-created-and-settled; yesterday's order
    // settling TODAY — the money lines must pick it up, the counts must
    // not (the §8 window mix).
    let u1 = format!("D{}", uid(BASE));
    let u2 = format!("D{}", uid(BASE));
    let u3 = format!("D{}", uid(BASE));
    order_on(&s.db, user, &u1, noon, None, 0, (900, 9)).await;
    order_on(&s.db, user, &u2, noon, Some(noon), 1, (800, 8)).await;
    order_on(&s.db, user, &u3, yesterday_noon, Some(noon), 2, (700, 7)).await;

    let row = day(&s.db, user, t, t).await.unwrap();
    assert_eq!(row.order_total_count, 2); // create window, all statuses
    assert_eq!(row.order_success_count, 1); // create window, 1/2 — NOT success window
    assert_eq!(row.order_fail_count, 1);
    assert_eq!(row.order_total_amount, 900 + 800); // create window, all
    assert_eq!(row.order_success_amount, 800 + 700); // SUCCESS window, 1/2
    assert_eq!(row.order_success0_amount, 900); // create window, status 0
    assert_eq!(row.order_poundage_amount, 8 + 7); // SUCCESS window, 1/2
    assert_eq!(row.user_id, user);
    assert_eq!(row.date, t);
}

#[tokio::test]
async fn snapshot_is_one_row_per_day_and_stays_fresh_within_the_horizon() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;
    let t = today();
    let noon = day_window(t).0 + 43_200;
    let a = format!("D{}", uid(BASE));
    order_on(&s.db, user, &a, noon, Some(noon), 1, (500, 5)).await;

    let first = day(&s.db, user, t, t).await.unwrap();
    assert_eq!(first.order_total_count, 1);

    // A new order lands; the next read (still inside 30 days) recomputes
    // and UPSERTs the SAME row — never a twin (the unique-index hardening).
    let b = format!("D{}", uid(BASE));
    order_on(&s.db, user, &b, noon, None, 0, (400, 4)).await;
    let second = day(&s.db, user, t, t).await.unwrap();
    assert_eq!(second.id, first.id, "upsert must reuse the row");
    assert_eq!(second.order_total_count, 2);
    assert_eq!(second.order_fail_count, 1);
    assert_eq!(rows(&s.db, user, t).await.len(), 1);
}

#[tokio::test]
async fn snapshots_beyond_the_horizon_are_frozen() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;
    let t = today();
    let old = t - chrono::Duration::days(31);
    let noon = day_window(old).0 + 43_200;

    // First sight of the (31-days-old) day: computed and snapshotted.
    let a = format!("D{}", uid(BASE));
    order_on(&s.db, user, &a, noon, Some(noon), 1, (600, 6)).await;
    let snap = day(&s.db, user, old, t).await.unwrap();
    assert_eq!(snap.order_total_count, 1);
    assert_eq!(snap.order_success_amount, 600);

    // Late orders on the frozen day never move the snapshot (AC:1155).
    let b = format!("D{}", uid(BASE));
    order_on(&s.db, user, &b, noon, Some(noon), 2, (300, 3)).await;
    let again = day(&s.db, user, old, t).await.unwrap();
    assert_eq!(again.order_total_count, 1);
    assert_eq!(again.order_success_amount, 600);
    assert_eq!(rows(&s.db, user, old).await.len(), 1);
}
