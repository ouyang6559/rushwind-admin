//! DB-gated tests for the §6.4 downline ORDER detail (`merchant::downline_order`)
//! — the per-order page (scope + 今日/累计 vs windowed stats) and the
//! `exportorder` CSV. Same harness as `agent_profit_report.rs`; ids base
//! 180_000_000_000_000.
#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::{members, orders};
use payment_api::merchant::downline_order::{self, DownlineOrderFilter};
use payment_api::reconcile::day_window;

const BASE: i64 = 180_000_000_000_000;

async fn seed_member(s: &Suite, id: i64, parentid: i64, groupid: i32, username: &str) {
    members::ActiveModel {
        id: Set(id),
        username: Set(username.to_string()),
        password: Set("x".into()),
        groupid: Set(groupid),
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

/// A timestamp comfortably inside the caller's local "today" (the 今日 leg's
/// window), so the summary roll-up is deterministic.
fn today_ts() -> i64 {
    let (from, _to) = day_window(chrono::Local::now().naive_local().date());
    from + 60
}

#[allow(clippy::too_many_arguments)]
async fn seed_order(
    s: &Suite,
    user: i64,
    order_id: &str,
    amount: i64,
    poundage: i64,
    actual: i64,
    status: i32,
    apply_date: i64,
    success_date: Option<i64>,
) {
    orders::ActiveModel {
        mch_id: Set(format!("{}", user + 10000)),
        order_id: Set(order_id.to_string()),
        amount: Set(amount),
        poundage: Set(poundage),
        actual_amount: Set(actual),
        cost: Set(0),
        apply_date: Set(apply_date),
        success_date: Set(success_date),
        bank_code: Set("903".into()),
        notify_url: Set("http://x/n".into()),
        callback_url: Set(String::new()),
        status: Set(status),
        channel_code: Set(Some("WxSm".into())),
        out_trade_id: Set(Some(format!("UP{order_id}"))),
        product_name: Set(Some("扫码收款".into())),
        user_id: Set(user),
        channel_id: Set(1),
        account_id: Set(1),
        t: Set(0),
        lock_status: Set(0),
        num: Set(0),
        last_reissue_time: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// No window → the page scopes to the agent's DIRECT children (status 0/1/2
/// only), and the summary stats fold today's successes + the cumulative
/// successes, excluding the platform / a foreign child / an out-of-scope status.
#[tokio::test]
async fn order_page_scopes_to_children_with_summary_stats() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let other_agent = uid(BASE);
    let c1 = uid(BASE);
    let c2 = uid(BASE);
    let outsider = uid(BASE);
    let tag = uid(BASE);

    seed_member(&s, agent, 1, 7, &format!("ag{tag}")).await;
    seed_member(&s, other_agent, 1, 7, &format!("ob{tag}")).await;
    seed_member(&s, c1, agent, 4, &format!("c1{tag}")).await;
    seed_member(&s, c2, agent, 4, &format!("c2{tag}")).await;
    seed_member(&s, outsider, other_agent, 4, &format!("ou{tag}")).await;

    let today = today_ts();
    // c1: a settled success (status 2) + a still-unpaid (status 0) + a scope-out.
    seed_order(
        &s,
        c1,
        &format!("a{tag}1"),
        1_000_000,
        6_000,
        994_000,
        2,
        1_000,
        Some(today),
    )
    .await;
    seed_order(&s, c1, &format!("a{tag}2"), 300_000, 0, 0, 0, 1_100, None).await;
    seed_order(
        &s,
        c1,
        &format!("a{tag}3"),
        77,
        7,
        70,
        5,
        1_200,
        Some(today),
    )
    .await;
    // c2: one success (status 1) today.
    seed_order(
        &s,
        c2,
        &format!("a{tag}4"),
        500_000,
        3_000,
        497_000,
        1,
        1_300,
        Some(today),
    )
    .await;
    // outsider: must never surface for this agent.
    seed_order(
        &s,
        outsider,
        &format!("a{tag}5"),
        999_999,
        0,
        0,
        2,
        1_400,
        Some(today),
    )
    .await;

    let page =
        downline_order::downline_order_page(&s.db, agent, &DownlineOrderFilter::default(), 1, 15)
            .await
            .unwrap();

    // 3 in-scope orders (c1: status 2 + 0; c2: status 1); status-5 + outsider out.
    assert_eq!(page.total, 3);
    assert_eq!(page.orders.len(), 3);
    // Newest id first → c2's order (seeded last of the in-scope) leads.
    assert_eq!(page.orders[0].order_id, format!("a{tag}4"));
    assert!(page.orders.iter().all(|o| o.user_id != outsider));

    // Summary: today's successes = c1(1_000_000) + c2(500_000); cumulative the
    // same (all successes are today, status IN (1,2)); the status-0 row counts
    // in the LIST but not the success stats.
    match page.stats {
        downline_order::OrderStats::Summary {
            today_amount,
            today_count,
            total_amount,
            total_count,
        } => {
            assert_eq!(today_amount, 1_500_000);
            assert_eq!(today_count, 2);
            assert_eq!(total_amount, 1_500_000);
            assert_eq!(total_count, 2);
        }
        other => panic!("expected summary stats, got {other:?}"),
    }
}

/// A posted window switches the stats to the filtered total (over the SAME
/// `status IN (0,1,2)` list predicate) and the `memberid` leg narrows to one
/// child; a non-child collapses to the empty scope.
#[tokio::test]
async fn order_page_windowed_total_and_memberid_narrowing() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let other_agent = uid(BASE);
    let c1 = uid(BASE);
    let outsider = uid(BASE);
    let tag = uid(BASE);

    seed_member(&s, agent, 1, 7, &format!("ag{tag}")).await;
    seed_member(&s, other_agent, 1, 7, &format!("ob{tag}")).await;
    seed_member(&s, c1, agent, 4, &format!("c1{tag}")).await;
    seed_member(&s, outsider, other_agent, 4, &format!("ou{tag}")).await;

    seed_order(
        &s,
        c1,
        &format!("b{tag}1"),
        1_000_000,
        6_000,
        994_000,
        2,
        1_000,
        Some(9_000),
    )
    .await;
    seed_order(
        &s,
        c1,
        &format!("b{tag}2"),
        400_000,
        2_000,
        398_000,
        0,
        5_000,
        None,
    )
    .await;
    seed_order(
        &s,
        outsider,
        &format!("b{tag}3"),
        8_888_888,
        0,
        0,
        2,
        1_000,
        Some(9_000),
    )
    .await;

    // apply window [4000, 6000] keeps only b2 (400_000, status 0).
    let windowed = downline_order::downline_order_page(
        &s.db,
        agent,
        &DownlineOrderFilter {
            apply_start: Some(4_000),
            apply_end: Some(6_000),
            ..Default::default()
        },
        1,
        15,
    )
    .await
    .unwrap();
    assert_eq!(windowed.total, 1);
    match windowed.stats {
        downline_order::OrderStats::Windowed {
            amount,
            actual_amount,
            count,
        } => {
            assert_eq!(amount, 400_000);
            assert_eq!(actual_amount, 398_000);
            assert_eq!(count, 1);
        }
        other => panic!("expected windowed stats, got {other:?}"),
    }

    // memberid narrowing: c1's wire id → only c1's two orders.
    let scoped = downline_order::downline_order_page(
        &s.db,
        agent,
        &DownlineOrderFilter {
            memberid: Some(c1 + 10_000),
            ..Default::default()
        },
        1,
        15,
    )
    .await
    .unwrap();
    assert_eq!(scoped.total, 2);

    // A memberid outside the downline → empty scope, zeroed summary.
    let foreign = downline_order::downline_order_page(
        &s.db,
        agent,
        &DownlineOrderFilter {
            memberid: Some(outsider + 10_000),
            ..Default::default()
        },
        1,
        15,
    )
    .await
    .unwrap();
    assert_eq!(foreign.total, 0);
    assert!(foreign.orders.is_empty());
}

/// The export read keeps only successes (`status IN (1,2)`) and the CSV renders
/// them newest-first with the legacy column shape (订单号 prefers out_trade_id).
#[tokio::test]
async fn export_renders_success_orders_as_csv() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let c1 = uid(BASE);
    let tag = uid(BASE);
    seed_member(&s, agent, 1, 7, &format!("ag{tag}")).await;
    seed_member(&s, c1, agent, 4, &format!("c1{tag}")).await;

    seed_order(
        &s,
        c1,
        &format!("c{tag}1"),
        1_000_000,
        6_000,
        994_000,
        2,
        1_000,
        Some(9_000),
    )
    .await;
    seed_order(&s, c1, &format!("c{tag}2"), 400_000, 0, 0, 0, 1_100, None).await; // unpaid → excluded

    let rows = downline_order::downline_order_export(&s.db, agent, &DownlineOrderFilter::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "only successes export");

    let csv = String::from_utf8(downline_order::render_order_csv(&rows)).unwrap();
    let body = csv.trim_start_matches('\u{FEFF}');
    let mut lines = body.lines();
    assert_eq!(
        lines.next().unwrap(),
        "订单号,商户编号,交易金额,手续费,实际金额,提交时间,成功时间,支付通道,支付状态"
    );
    let row = lines.next().unwrap();
    // out_trade_id (UP…) wins as 订单号; amounts are raw units; status labelled.
    assert!(row.starts_with(&format!("UPc{tag}1,{},1000000,6000,994000,", c1 + 10_000)));
    assert!(row.ends_with("WxSm,成功，已返回"));
}
