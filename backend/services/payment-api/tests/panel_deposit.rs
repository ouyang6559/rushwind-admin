//! DB coverage for the §10 保证金明细 (complaintsDeposit) read side. The pure
//! filter/date-parse legs live in the handler; here we drive the three real-DB
//! reads against seeded `complaints_deposits` rows:
//!
//! - [`deposit::count_filtered`] / [`deposit::list_filtered`] applying the
//!   optional `out_trade_id` / `status` / `create_at` range filters and always
//!   the `user_id` ownership scope (a foreign row never shows);
//! - [`deposit::list_filtered`] newest-id-first ordering;
//! - [`deposit::stats`] the `all` / `freezed` (status 1) / `unfreezed`
//!   (status 0) three-way summary, scoped to `user_id` + the create range only
//!   (the legacy `$map`, ignoring the list-only `orderid` / `status` legs).
//!
//! Harness mirrors `panel_console.rs`; ids base 140_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::complaints_deposits;
use payment_api::merchant::deposit::{self, DepositFilter};

const BASE: i64 = 140_000_000_000_000;
// A fixed unix clock for the create_at range legs.
const T0: i64 = 1_700_000_000;

/// Seeds one deposit row verbatim (all NOT-NULL columns set explicitly) and
/// returns its id.
async fn seed(
    s: &Suite,
    id: i64,
    user: i64,
    out_trade_id: &str,
    freeze: i64,
    status: i32,
    create_at: i64,
) -> i64 {
    complaints_deposits::ActiveModel {
        id: Set(id),
        user_id: Set(user),
        pay_orderid: Set(out_trade_id.into()),
        out_trade_id: Set(out_trade_id.into()),
        freeze_money: Set(freeze),
        unfreeze_time: Set(create_at + 3600),
        real_unfreeze_time: Set(if status == 1 { create_at + 7200 } else { 0 }),
        is_pause: Set(0),
        status: Set(status),
        create_at: Set(create_at),
        update_at: Set(create_at),
    }
    .insert(&*s.db)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn filtered_reads_scope_to_owner_and_honour_filters() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let other = uid(BASE + 1_000_000);
    let id_a = seed(&s, uid(BASE + 2_000_000), user, "OT-A", 1000, 0, T0).await;
    let id_b = seed(&s, uid(BASE + 3_000_000), user, "OT-B", 2000, 1, T0 + 100).await;
    let id_c = seed(&s, uid(BASE + 4_000_000), user, "OT-C", 300, 0, T0 + 500).await;
    // A foreign merchant's frozen row: must never surface for `user`.
    seed(&s, uid(BASE + 5_000_000), other, "OT-X", 999_999, 0, T0).await;

    // No list filters: only the owner's 3 rows.
    let none = DepositFilter::default();
    assert_eq!(
        deposit::count_filtered(&s.db, user, &none).await.unwrap(),
        3
    );

    // status = 0 → A, C.
    let frozen = DepositFilter {
        status: Some(0),
        ..Default::default()
    };
    assert_eq!(
        deposit::count_filtered(&s.db, user, &frozen).await.unwrap(),
        2
    );

    // out_trade_id exact → B only.
    let by_order = DepositFilter {
        out_trade_id: Some("OT-B".into()),
        ..Default::default()
    };
    assert_eq!(
        deposit::count_filtered(&s.db, user, &by_order)
            .await
            .unwrap(),
        1
    );

    // create range [T0, T0+100] inclusive → A, B (C at T0+500 drops out).
    let ranged = DepositFilter {
        create_start: Some(T0),
        create_end: Some(T0 + 100),
        ..Default::default()
    };
    assert_eq!(
        deposit::count_filtered(&s.db, user, &ranged).await.unwrap(),
        2
    );

    // Newest-id-first ordering across the whole owner ledger.
    let rows = deposit::list_filtered(&s.db, user, &none, 1, 50)
        .await
        .unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids[0], id_c, "highest id leads (id desc)");
    assert_eq!(ids[ids.len() - 1], id_a, "lowest id trails");
    assert!(ids.contains(&id_b));
    // Pagination: one per page, page 2 is the middle row.
    let page2 = deposit::list_filtered(&s.db, user, &none, 2, 1)
        .await
        .unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].id, id_b);
}

#[tokio::test]
async fn stats_summarise_by_status_and_range() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE + 20_000_000);
    seed(&s, uid(BASE + 21_000_000), user, "SA", 1000, 0, T0).await;
    seed(&s, uid(BASE + 22_000_000), user, "SB", 2000, 1, T0 + 100).await;
    seed(&s, uid(BASE + 23_000_000), user, "SC", 300, 0, T0 + 500).await;
    // A foreign row that must not enter the summary.
    seed(
        &s,
        uid(BASE + 24_000_000),
        uid(BASE + 25_000_000),
        "SX",
        999_999,
        0,
        T0,
    )
    .await;

    // Whole ledger: all = 3300, freezed(status1) = 2000, unfreezed(status0) = 1300.
    let full = deposit::stats(&s.db, user, None, None).await.unwrap();
    assert_eq!(full.all, 3300);
    assert_eq!(full.freezed, 2000);
    assert_eq!(full.unfreezed, 1300);

    // Date-scoped to [T0, T0+100]: excludes SC(300), so all=3000, unfreezed=1000.
    let scoped = deposit::stats(&s.db, user, Some(T0), Some(T0 + 100))
        .await
        .unwrap();
    assert_eq!(scoped.all, 3000);
    assert_eq!(scoped.freezed, 2000);
    assert_eq!(scoped.unfreezed, 1000);
}

#[tokio::test]
async fn stats_ignores_list_only_filters() {
    let Some(s) = suite().await else { return };
    // The stats summary deliberately has no out_trade_id / status leg — assert
    // a status-filtered LIST (2 frozen) over the same ledger while the date
    // summary still reports every row (all), confirming the $where / $map split.
    let user = uid(BASE + 30_000_000);
    seed(&s, uid(BASE + 31_000_000), user, "MA", 1000, 0, T0).await;
    seed(&s, uid(BASE + 32_000_000), user, "MB", 2000, 0, T0 + 10).await;
    seed(&s, uid(BASE + 33_000_000), user, "MC", 5000, 1, T0 + 20).await;

    let only_frozen = DepositFilter {
        status: Some(0),
        ..Default::default()
    };
    assert_eq!(
        deposit::count_filtered(&s.db, user, &only_frozen)
            .await
            .unwrap(),
        2
    );
    // …but stats over the same owner (no range) report all three rows.
    let full = deposit::stats(&s.db, user, None, None).await.unwrap();
    assert_eq!(full.all, 8000);
    assert_eq!(full.unfreezed, 3000);
    assert_eq!(full.freezed, 5000);
}
