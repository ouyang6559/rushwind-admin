//! The §10.1 auto-payout back-office CRUD round-trip (`AutoDfRepo`): a full
//! settings form writes the six `auto_df_*` columns onto the platform
//! (`issystem = 1`) row and reads back losslessly, feeding the same exec view
//! the sweep loads. Because every test shares one database and there is exactly
//! one platform row, the write is exercised inside a transaction that is rolled
//! back, so a concurrent seed of the global row can neither observe nor disturb
//! it. Same harness / env-gating rules as `payout_channel_store.rs`.

mod common;

use sea_orm::{ConnectionTrait, TransactionTrait};

use common::suite;
use payment_api::payout::{AutoDfConfig, AutoDfRepo, AutoDfSettings};

const K: i64 = 10_000;

/// Clears any committed platform row within THIS transaction so the repo's
/// create path is deterministic (the committed rows stay untouched outside).
async fn isolate_platform_row<C: ConnectionTrait>(txn: &C) {
    txn.execute_unprepared("DELETE FROM tikuan_configs WHERE issystem = 1")
        .await
        .unwrap();
}

#[tokio::test]
async fn auto_df_settings_round_trip_through_the_platform_row() {
    let Some(s) = suite().await else { return };
    let txn = s.db.begin().await.unwrap();
    isolate_platform_row(&txn).await;

    let repo = AutoDfRepo::new(&txn);
    // No row yet → reads as off, and the exec view is disabled.
    assert_eq!(repo.load().await.unwrap(), AutoDfSettings::off());
    assert!(!AutoDfConfig::load(&txn).await.unwrap().switch);

    let settings = AutoDfSettings {
        switch: true,
        max_money: 200 * K,
        stime: " 09:30 ".into(), // trimmed on write
        etime: "18:45".into(),
        max_count: 5,
        max_sum: 1_000 * K,
    };
    let saved = repo.save(settings.clone()).await.unwrap();
    assert_eq!(saved.issystem, 1, "the created row is the platform row");
    assert_eq!(saved.user_id, 0);

    // The stored row projects straight back to the submitted form.
    let back = repo.load().await.unwrap();
    assert_eq!(
        back,
        AutoDfSettings {
            stime: "09:30".into(),
            ..settings.clone()
        },
        "window strings are trimmed on write"
    );

    // A second save UPDATES (never duplicates) the single platform row.
    let edited = AutoDfSettings {
        switch: false,
        max_money: 0, // 0 = unlimited ceiling
        stime: String::new(),
        etime: String::new(),
        max_count: 0,
        max_sum: 0,
    };
    repo.save(edited.clone()).await.unwrap();
    assert_eq!(repo.load().await.unwrap(), edited);

    // And the sweep's exec view reflects it: switch off, no ceiling.
    let cfg = AutoDfConfig::load(&txn).await.unwrap();
    assert!(!cfg.switch);
    assert_eq!(cfg.max_money, None);

    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn auto_df_save_rejects_a_bad_window_before_touching_the_db() {
    let Some(s) = suite().await else { return };
    let txn = s.db.begin().await.unwrap();
    let repo = AutoDfRepo::new(&txn);
    // Validation runs ahead of any read / write, so no isolation is needed.
    let bad = AutoDfSettings {
        stime: "25:00".into(),
        ..AutoDfSettings::off()
    };
    assert!(repo.save(bad).await.is_err());
    txn.rollback().await.unwrap();
}
