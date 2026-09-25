//! DB coverage for the §10 console首页 aggregation (read side). The pure
//! branch (`audience_groups`) is covered offline in `article.rs`; here we drive
//! the three real-DB reads the `main` block folds:
//!
//! - [`console::today_stats`] over seeded `orders` (four lines, mixed windows)
//!   together with the frozen `complaints_deposits` balance and the `lx = 9`
//!   ledger share, checking `today_income = actual_sum + share`;
//! - [`deposit::frozen_sum`] counting only `status = 0` rows;
//! - [`article::list_visible`] audience filtering — a merchant sees
//!   `{all, merchant}` (status 1), an agent `{all, agent}`, verified by
//!   membership so concurrent article rows from other tests can't skew it.
//!
//! Harness mirrors `panel_charges.rs`; ids base 110_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use chrono::NaiveDate;
use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::{articles, complaints_deposits, members, money_changes, orders};
use payment_api::merchant::{article, console, deposit};

const BASE: i64 = 110_000_000_000_000;

async fn seed_member(s: &Suite, user: i64) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("cs{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        df_auto_check: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn seed_order(
    s: &Suite,
    user: i64,
    no: &str,
    apply_date: i64,
    success_date: Option<i64>,
    status: i32,
    actual_amount: i64,
) {
    orders::ActiveModel {
        id: Set(uid(BASE + 7_000_000)), // distinct high ids in the orders space
        user_id: Set(user),
        order_id: Set(no.into()),
        mch_id: Set(format!("{}", user + 10000)),
        bank_code: Set("903".into()),
        notify_url: Set("http://127.0.0.1/notify".into()),
        callback_url: Set(String::new()),
        amount: Set(0),
        poundage: Set(0),
        cost: Set(0),
        actual_amount: Set(actual_amount),
        apply_date: Set(apply_date),
        success_date: Set(success_date),
        status: Set(status),
        num: Set(0),
        last_reissue_time: Set(0),
        channel_id: Set(0),
        account_id: Set(0),
        t: Set(0),
        lock_status: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn seed_deposit(s: &Suite, user: i64, no: &str, freeze: i64, status: i32) {
    complaints_deposits::ActiveModel {
        id: Set(uid(BASE + 8_000_000)),
        user_id: Set(user),
        pay_orderid: Set(no.into()),
        out_trade_id: Set(no.into()),
        freeze_money: Set(freeze),
        unfreeze_time: Set(0),
        real_unfreeze_time: Set(0),
        is_pause: Set(0),
        status: Set(status),
        create_at: Set(0),
        update_at: Set(0),
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn seed_money_change(s: &Suite, user: i64, money: i64, lx: i32, at: chrono::NaiveDateTime) {
    money_changes::ActiveModel {
        id: Set(uid(BASE + 9_000_000)),
        user_id: Set(user),
        y_money: Set(0),
        money: Set(money),
        g_money: Set(0),
        lx: Set(lx),
        datetime: Set(at),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn seed_article(s: &Suite, catid: i32, groupid: i32, status: i32) -> i64 {
    let model = articles::ActiveModel {
        id: Set(uid(BASE + 10_000_000)),
        catid: Set(i64::from(catid)),
        groupid: Set(groupid),
        title: Set(format!("t{groupid}-{status}")),
        description: Set(String::new()),
        createtime: Set(1_700_000_000),
        updatetime: Set(1_700_000_000),
        status: Set(status),
        content: Set(None),
    }
    .insert(&*s.db)
    .await
    .unwrap();
    model.id
}

#[tokio::test]
async fn today_stats_folds_orders_deposit_and_income() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user).await;
    // A fixed clock so the day window is deterministic.
    let date = NaiveDate::from_ymd_opt(2026, 9, 24).unwrap();
    let day_start = date.and_hms_opt(0, 0, 0).unwrap();
    let from = payment_api::reconcile::day_window(date).0;

    // In-window orders: 1 paid, 1 unpaid, plus a paid order applied yesterday
    // but settled today (moves the money + paid-count lines only).
    seed_order(
        &s,
        user,
        &format!("{user}-a"),
        from + 100,
        Some(from + 200),
        1,
        5000,
    )
    .await;
    seed_order(&s, user, &format!("{user}-b"), from + 150, None, 0, 0).await;
    seed_order(
        &s,
        user,
        &format!("{user}-c"),
        from - 100,
        Some(from + 300),
        2,
        3000,
    )
    .await;
    // Frozen deposit balance + a released row that must not count.
    seed_deposit(&s, user, &format!("{user}-d1"), 1200, 0).await;
    seed_deposit(&s, user, &format!("{user}-d2"), 9999, 1).await;
    // lx=9 income booked today (+) counts; lx=9 outside the day and lx=1 ignore.
    seed_money_change(&s, user, 800, 9, day_start + chrono::Duration::hours(12)).await;
    seed_money_change(&s, user, 777, 9, day_start - chrono::Duration::days(1)).await;
    seed_money_change(&s, user, 555, 1, day_start + chrono::Duration::hours(12)).await;

    let stat = console::today_stats(&s.db, user, date).await.unwrap();
    // todayordercount: apply_date in window → a, b = 2
    assert_eq!(stat.today_order_count, 2);
    // paid: success_date in window & status 1/2 → a, c = 2
    assert_eq!(stat.today_order_paid_count, 2);
    // unpaid: apply_date in window & status 0 → b = 1
    assert_eq!(stat.today_order_unpaid_count, 1);
    // actual sum: success in window status 1/2 → a(5000) + c(3000) = 8000
    assert_eq!(stat.today_order_actual_sum, 8000);
    assert_eq!(stat.complaints_deposit, 1200);
    // income = 8000 + lx9-in-day(800) = 8800
    assert_eq!(stat.today_income, 8800);
}

#[tokio::test]
async fn frozen_sum_counts_only_status_zero() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user).await;
    seed_deposit(&s, user, &format!("{user}-f1"), 300, 0).await;
    seed_deposit(&s, user, &format!("{user}-f2"), 500, 0).await;
    seed_deposit(&s, user, &format!("{user}-f3"), 999, 1).await;
    assert_eq!(deposit::frozen_sum(&s.db, user).await.unwrap(), 800);
}

#[tokio::test]
async fn article_visibility_branches_on_audience() {
    let Some(s) = suite().await else { return };
    // group0 visible to all; group1 merchant-only; group2 agent-only; the last
    // is group1 but hidden (status 0).
    let all = seed_article(&s, 1, 0, 1).await;
    let merchant = seed_article(&s, 1, 1, 1).await;
    let agent = seed_article(&s, 1, 2, 1).await;
    let hidden = seed_article(&s, 1, 1, 0).await;

    // Membership assertions (robust to concurrent article rows from other runs).
    let merchant_ids: Vec<i64> = article::list_visible(&s.db, true, 0, 200)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert!(
        merchant_ids.contains(&all),
        "all-audience visible to merchant"
    );
    assert!(
        merchant_ids.contains(&merchant),
        "merchant-audience visible to merchant"
    );
    assert!(
        !merchant_ids.contains(&agent),
        "agent-audience hidden from merchant"
    );
    assert!(
        !merchant_ids.contains(&hidden),
        "status 0 hidden from merchant"
    );

    let agent_ids: Vec<i64> = article::list_visible(&s.db, false, 0, 200)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert!(agent_ids.contains(&all));
    assert!(agent_ids.contains(&agent));
    assert!(
        !agent_ids.contains(&merchant),
        "merchant-audience hidden from agent"
    );

    // Newest-first: among my seeded merchant-visible ids, the higher id leads.
    let pos = |id: i64| merchant_ids.iter().position(|&x| x == id).unwrap();
    assert!(pos(merchant) < pos(all), "higher id sorts first (id desc)");
}
