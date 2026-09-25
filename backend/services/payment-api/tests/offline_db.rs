//! DB-gated integration for the daily offline-reset plan (§offline): a
//! fake-day clock (the Redis marker is keyed by the UTC `Ymd`, so distinct
//! epoch-day numbers give each run its own never-colliding day) over real
//! channels / channel_accounts rows. Row-count assertions target the
//! seeded rows — the UPDATEs themselves are table-wide on a shared DB.

#![allow(dead_code)]

use payment_api::data::{channel_accounts, channels};
use payment_api::risk::offline::{planning, reset_marker};
use redis::aio::ConnectionManager;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

mod common;
use common::uid;

const BASE: i64 = 10_900_000_000_000;

/// A far-future fake civil day (the marker keys on the UTC `Ymd`), so the
/// test never collides with the real cron's markers or with itself.
fn fake_day_ts(day: i64) -> i64 {
    day * 86_400 + 3_600
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

async fn seed_channel(db: &DatabaseConnection, id: i64, control: i32, offline: i32) {
    channels::ActiveModel {
        id: sea_orm::ActiveValue::Set(id),
        code: sea_orm::ActiveValue::Set(format!("OF{id}")),
        title: sea_orm::ActiveValue::Set("offline-test".into()),
        default_rate: sea_orm::ActiveValue::Set(6_000),
        fengding: sea_orm::ActiveValue::Set(0),
        t0_default_rate: sea_orm::ActiveValue::Set(6_000),
        t0_fengding: sea_orm::ActiveValue::Set(0),
        status: sea_orm::ActiveValue::Set(1),
        paytype: sea_orm::ActiveValue::Set(1),
        control_status: sea_orm::ActiveValue::Set(control),
        offline_status: sea_orm::ActiveValue::Set(offline),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

async fn seed_account(db: &DatabaseConnection, id: i64, control: i32, offline: i32) {
    channel_accounts::ActiveModel {
        id: sea_orm::ActiveValue::Set(id),
        channel_id: sea_orm::ActiveValue::Set(id),
        weight: sea_orm::ActiveValue::Set(1),
        status: sea_orm::ActiveValue::Set(1),
        default_rate: sea_orm::ActiveValue::Set(6_000),
        fengding: sea_orm::ActiveValue::Set(0),
        t0_default_rate: sea_orm::ActiveValue::Set(6_000),
        t0_fengding: sea_orm::ActiveValue::Set(0),
        custom_rate: sea_orm::ActiveValue::Set(0),
        control_status: sea_orm::ActiveValue::Set(control),
        offline_status: sea_orm::ActiveValue::Set(offline),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

async fn channel_offline(db: &DatabaseConnection, id: i64) -> i32 {
    channels::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .offline_status
}

async fn account_offline(db: &DatabaseConnection, id: i64) -> i32 {
    channel_accounts::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .offline_status
}

async fn knock_offline(db: &DatabaseConnection, id: i64) {
    channels::Entity::update_many()
        .col_expr(channels::Column::OfflineStatus, Expr::value(0))
        .filter(channels::Column::Id.eq(id))
        .exec(db)
        .await
        .unwrap();
}

/// This test owns its two fake days exclusively (the constants live nowhere
/// else), so wiping their markers up front makes re-runs idempotent.
async fn clear_marker(conn: &mut ConnectionManager, ts: i64) {
    let _: redis::RedisResult<i64> = redis::cmd("DEL")
        .arg(reset_marker(ts))
        .query_async(conn)
        .await;
}

#[tokio::test]
async fn offline_reset_lifecycle() {
    let Some(s) = common::suite().await else {
        return;
    };
    let redis = redis().await;
    let mut cleaner = redis.clone();
    clear_marker(&mut cleaner, fake_day_ts(40_001)).await;
    clear_marker(&mut cleaner, fake_day_ts(40_002)).await;

    let ch_on = uid(BASE);
    let ch_off = uid(BASE);
    let ac_on = uid(BASE);
    let ac_off = uid(BASE);
    seed_channel(&s.db, ch_on, 1, 0).await;
    seed_channel(&s.db, ch_off, 0, 0).await;
    seed_account(&s.db, ac_on, 1, 0).await;
    seed_account(&s.db, ac_off, 0, 0).await;

    // Tick 1 of fake-day A: restore wins the marker and re-opens exactly
    // the control_status=1 rows.
    let day_a = fake_day_ts(40_001);
    let o = planning(&s.db, &redis, day_a).await.unwrap();
    assert!(o.ran);
    assert!(o.channels >= 1 && o.accounts >= 1);
    assert_eq!(channel_offline(&s.db, ch_on).await, 1);
    assert_eq!(account_offline(&s.db, ac_on).await, 1);
    assert_eq!(
        channel_offline(&s.db, ch_off).await,
        0,
        "uncontrolled stays down"
    );
    assert_eq!(account_offline(&s.db, ac_off).await, 0);

    // Tick 2, same fake day: the marker short-circuits the run — a row
    // knocked offline again stays offline until tomorrow.
    knock_offline(&s.db, ch_on).await;
    let o = planning(&s.db, &redis, day_a + 3600).await.unwrap();
    assert!(!o.ran);
    assert_eq!(channel_offline(&s.db, ch_on).await, 0);

    // First tick of fake-day B: a new marker key, the restore runs again.
    assert_ne!(reset_marker(day_a), reset_marker(fake_day_ts(40_002)));
    let o = planning(&s.db, &redis, fake_day_ts(40_002)).await.unwrap();
    assert!(o.ran);
    assert_eq!(channel_offline(&s.db, ch_on).await, 1);
}
