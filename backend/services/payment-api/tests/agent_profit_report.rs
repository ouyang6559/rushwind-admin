//! DB-gated test for the §7 downline earnings report (`merchant::profit_report`)
//! — the 成交 totals + `lx = 9` 分润 attribution per DIRECT child, joined
//! through `money_changes.order_id ↔ orders.order_id`. Same harness as
//! `agent_downline.rs`; ids base 170_000_000_000_000.
#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::{members, money_changes, orders};
use payment_api::merchant::profit_report::{self, DownlineReportFilter};

const BASE: i64 = 170_000_000_000_000;

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

/// A minimal order for `user` with an explicit status + apply window so the
/// aggregate legs are deterministic.
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
) {
    orders::ActiveModel {
        mch_id: Set(format!("{}", user + 10000)),
        order_id: Set(order_id.to_string()),
        amount: Set(amount),
        poundage: Set(poundage),
        actual_amount: Set(actual),
        cost: Set(0),
        apply_date: Set(apply_date),
        bank_code: Set("903".into()),
        notify_url: Set("http://x/n".into()),
        callback_url: Set(String::new()),
        status: Set(status),
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

/// An `lx`-typed money-change flow credited to `agent`, keyed to `order_id`
/// (the join leg the report reads for per-child 分润).
async fn seed_flow(s: &Suite, agent: i64, order_id: &str, money: i64, lx: i32) {
    money_changes::ActiveModel {
        user_id: Set(agent),
        y_money: Set(0),
        money: Set(money),
        g_money: Set(money),
        datetime: Set(payment_api::data::now()),
        lx: Set(lx),
        order_id: Set(Some(order_id.to_string())),
        request_id: Set(Some(format!("t{order_id}{lx}"))),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// The tree report totals each DIRECT child's in-scope orders, attributes the
/// agent's `lx = 9` 分润 through the order join, excludes the platform / a
/// foreign parent's child / a non `(0,1,2)` order status / a non-profit flow.
#[tokio::test]
async fn downline_report_aggregates_trade_and_profit_per_child() {
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

    // c1: two settled orders (status 1) + one out-of-scope (status 5).
    let c1o1 = format!("p{tag}1");
    let c1o2 = format!("p{tag}2");
    let c1o3 = format!("p{tag}3");
    seed_order(&s, c1, &c1o1, 1_000_000, 6_000, 994_000, 1, 1_000).await;
    seed_order(&s, c1, &c1o2, 500_000, 3_000, 497_000, 1, 5_000).await;
    seed_order(&s, c1, &c1o3, 77, 7, 70, 5, 2_000).await;
    // c2: one order.
    let c2o1 = format!("p{tag}4");
    seed_order(&s, c2, &c2o1, 2_000_000, 12_000, 1_988_000, 2, 3_000).await;
    // outsider (other agent's child): must never surface.
    let oo = format!("p{tag}5");
    seed_order(&s, outsider, &oo, 999_999, 0, 0, 1, 1_000).await;

    // The agent's lx=9 分润 on those orders (+ decoys that must be excluded).
    seed_flow(&s, agent, &c1o1, 1_000, 9).await;
    seed_flow(&s, agent, &c1o2, 500, 9).await;
    seed_flow(&s, agent, &c2o1, 3_000, 9).await;
    seed_flow(&s, agent, &oo, 999, 9).await; // foreign child's order → excluded
    seed_flow(&s, agent, &c1o1, 777, 1).await; // not a profit flow → excluded

    let rows =
        profit_report::downline_profit_report(&s.db, agent, &DownlineReportFilter::default())
            .await
            .unwrap();
    assert_eq!(rows.len(), 2, "only the two direct children surface");
    // c2 has the larger id → newest-first ordering puts it first.
    let r_c2 = rows.iter().find(|r| r.child_id == c2).unwrap();
    let r_c1 = rows.iter().find(|r| r.child_id == c1).unwrap();
    assert_eq!(rows[0].child_id, c2);

    assert_eq!(r_c1.trade_amount, 1_500_000);
    assert_eq!(r_c1.poundage, 9_000);
    assert_eq!(r_c1.actual_amount, 1_491_000);
    assert_eq!(r_c1.order_count, 2, "the status-5 order is out of scope");
    assert_eq!(r_c1.agent_profit, 1_500, "1000 + 500, decoys excluded");

    assert_eq!(r_c2.trade_amount, 2_000_000);
    assert_eq!(r_c2.order_count, 1);
    assert_eq!(r_c2.agent_profit, 3_000);
}

/// `child = Some(x)` narrows to one DIRECT child; the apply window cuts both
/// the order totals and the joined 分润 to the same period. A non-child yields
/// an empty report.
#[tokio::test]
async fn downline_report_scopes_to_one_child_and_window() {
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

    let early = format!("w{tag}1");
    let late = format!("w{tag}2");
    seed_order(&s, c1, &early, 1_000_000, 6_000, 994_000, 1, 1_000).await;
    seed_order(&s, c1, &late, 500_000, 3_000, 497_000, 1, 5_000).await;
    seed_flow(&s, agent, &early, 1_000, 9).await;
    seed_flow(&s, agent, &late, 500, 9).await;

    // Whole window on the single child.
    let all = profit_report::downline_profit_report(
        &s.db,
        agent,
        &DownlineReportFilter {
            child: Some(c1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].trade_amount, 1_500_000);
    assert_eq!(all[0].agent_profit, 1_500);

    // apply_date >= 4000 keeps only the late order (and its 分润).
    let windowed = profit_report::downline_profit_report(
        &s.db,
        agent,
        &DownlineReportFilter {
            child: Some(c1),
            apply_start: Some(4_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(windowed[0].trade_amount, 500_000);
    assert_eq!(windowed[0].order_count, 1);
    assert_eq!(windowed[0].agent_profit, 500);

    // A child that is not ours → empty.
    let foreign = profit_report::downline_profit_report(
        &s.db,
        agent,
        &DownlineReportFilter {
            child: Some(outsider),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(foreign.is_empty());
}
