//! DB-gated tests for the §6.2 agent downline rate write
//! (`merchant::agent_rate::apply_agent_rates` — the persistence core behind
//! `User/AgentController::saveUserRate`). Identity / ownership / session are
//! the handler's job and are exercised by the panel unit tests; here we drive
//! the service fn directly: the per-product agent-cost floor (all-or-nothing
//! validation ahead of any write), the missing-cost pass-through caveat, the
//! upsert (insert vs update of the downline's `(userid, payapiid)` row), and
//! batch atomicity. Same harness as `payout_dfpay.rs`; ids base
//! 11_700_000_000_000.

mod common;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use common::{suite, uid, Suite};
use payment_api::data::{members, product_users, products, user_rates};
use payment_api::merchant::agent_rate::{
    apply_agent_rates, downline_rate_edit, RateRow, MSG_T0_BELOW_COST, MSG_T1_BELOW_COST,
};

const BASE: i64 = 11_700_000_000_000;

async fn seed_member(s: &Suite, user: i64, parentid: i64) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("ar{user}")),
        password: Set("x".into()),
        groupid: Set(5),
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

/// Inserts a `userrate` row (the agent's OWN cost, or a pre-existing downline
/// row to be updated).
async fn seed_rate(s: &Suite, user: i64, product: i64, rate: i64, t0_rate: i64) {
    user_rates::ActiveModel {
        id: Set(uid(BASE)),
        user_id: Set(user),
        channel_id: Set(product),
        rate: Set(rate),
        fengding: Set(0),
        t0_rate: Set(t0_rate),
        t0_fengding: Set(0),
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn rate_of(s: &Suite, user: i64, product: i64) -> Option<user_rates::Model> {
    user_rates::Entity::find()
        .filter(user_rates::Column::UserId.eq(user))
        .filter(user_rates::Column::ChannelId.eq(product))
        .one(&*s.db)
        .await
        .unwrap()
}

fn row(product: i64, rate: i64, t0_rate: i64) -> RateRow {
    RateRow {
        product_id: product,
        rate,
        fengding: 20_000,
        t0_rate,
        t0_fengding: 0,
    }
}

#[tokio::test]
async fn inserts_a_new_downline_rate_when_at_or_above_cost() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let downline = uid(BASE);
    let product = 900;
    seed_member(&s, agent, 1).await;
    seed_member(&s, downline, agent).await;
    seed_rate(&s, agent, product, 6_000, 8_000).await;

    let res = apply_agent_rates(&s.db, agent, downline, &[row(product, 6_500, 8_500)])
        .await
        .unwrap();
    assert!(res.is_ok(), "expected clean write, got {res:?}");

    let written = rate_of(&s, downline, product).await.expect("row written");
    assert_eq!(written.rate, 6_500);
    assert_eq!(written.t0_rate, 8_500);
    assert_eq!(written.fengding, 20_000);
}

#[tokio::test]
async fn updates_an_existing_downline_rate_row() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let downline = uid(BASE);
    let product = 901;
    seed_member(&s, agent, 1).await;
    seed_member(&s, downline, agent).await;
    seed_rate(&s, agent, product, 6_000, 6_000).await;
    seed_rate(&s, downline, product, 7_000, 7_000).await;

    let before_id = rate_of(&s, downline, product).await.unwrap().id;
    apply_agent_rates(&s.db, agent, downline, &[row(product, 9_000, 9_000)])
        .await
        .unwrap()
        .expect("clean write");

    let after = rate_of(&s, downline, product).await.unwrap();
    assert_eq!(after.id, before_id, "the existing row is updated in place");
    assert_eq!(after.rate, 9_000);
    assert_eq!(after.t0_rate, 9_000);
}

#[tokio::test]
async fn rejects_a_t1_rate_below_the_agent_cost_writing_nothing() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let downline = uid(BASE);
    let product = 902;
    seed_member(&s, agent, 1).await;
    seed_member(&s, downline, agent).await;
    seed_rate(&s, agent, product, 6_000, 8_000).await;

    let res = apply_agent_rates(&s.db, agent, downline, &[row(product, 5_000, 8_000)])
        .await
        .unwrap();
    match res {
        Err(v) => {
            assert_eq!(v.product_id, product);
            assert_eq!(v.msg, MSG_T1_BELOW_COST);
        }
        Ok(()) => panic!("expected a cost-floor rejection"),
    }
    assert!(rate_of(&s, downline, product).await.is_none());
}

#[tokio::test]
async fn a_t0_rate_below_cost_reports_the_t0_message() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let downline = uid(BASE);
    let product = 903;
    seed_member(&s, agent, 1).await;
    seed_member(&s, downline, agent).await;
    seed_rate(&s, agent, product, 6_000, 8_000).await;

    let res = apply_agent_rates(&s.db, agent, downline, &[row(product, 6_000, 7_000)])
        .await
        .unwrap();
    match res {
        Err(v) => assert_eq!(v.msg, MSG_T0_BELOW_COST),
        Ok(()) => panic!("expected a T+0 cost-floor rejection"),
    }
}

#[tokio::test]
async fn a_missing_agent_cost_lets_any_rate_through() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let downline = uid(BASE);
    let product = 904;
    seed_member(&s, agent, 1).await;
    seed_member(&s, downline, agent).await;
    // No agent cost row for `product` → floor 0 → even a 0 submitted rate passes.

    let res = apply_agent_rates(&s.db, agent, downline, &[row(product, 0, 0)])
        .await
        .unwrap();
    assert!(res.is_ok(), "missing cost must not reject, got {res:?}");
    assert!(rate_of(&s, downline, product).await.is_some());
}

#[tokio::test]
async fn one_violation_aborts_the_whole_batch() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let downline = uid(BASE);
    let ok_prod = 905;
    let bad_prod = 906;
    seed_member(&s, agent, 1).await;
    seed_member(&s, downline, agent).await;
    seed_rate(&s, agent, ok_prod, 6_000, 6_000).await;
    seed_rate(&s, agent, bad_prod, 7_000, 7_000).await;

    let rows = [row(ok_prod, 6_500, 6_500), row(bad_prod, 5_000, 5_000)];
    let res = apply_agent_rates(&s.db, agent, downline, &rows)
        .await
        .unwrap();
    assert!(matches!(res, Err(v) if v.product_id == bad_prod));
    // All-or-nothing: the FIRST (valid) product was NOT written either.
    assert!(rate_of(&s, downline, ok_prod).await.is_none());
    assert!(rate_of(&s, downline, bad_prod).await.is_none());
}

// --- §6.2 read side (`userRateEdit`) ----------------------------------------

/// A system product row with the given live / display flags.
async fn seed_product(s: &Suite, id: i64, name: &str, status: i32, isdisplay: i32) {
    products::ActiveModel {
        id: Set(id),
        name: Set(name.into()),
        code: Set("c".into()),
        polling: Set(0),
        paytype: Set(1),
        status: Set(status),
        isdisplay: Set(isdisplay),
        channel: Set(1),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// Assigns `pid` to `user` with the given enablement status.
async fn seed_pu(s: &Suite, user: i64, pid: i64, status: i32) {
    product_users::ActiveModel {
        user_id: Set(user),
        pid: Set(pid),
        polling: Set(0),
        status: Set(status),
        channel: Set(1),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn downline_rate_edit_loads_opened_products_with_current_rates() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let child = uid(BASE);
    seed_member(&s, agent, 1).await;
    seed_member(&s, child, agent).await;

    // A: opened + displayed, has a userrate override.
    let pa = uid(BASE);
    seed_product(&s, pa, "产品A", 1, 1).await;
    seed_pu(&s, child, pa, 1).await;
    seed_rate(&s, child, pa, 6_500, 8_500).await;
    // B: opened + displayed, no userrate → the '0.000' placeholder.
    let pb = uid(BASE);
    seed_product(&s, pb, "产品B", 1, 1).await;
    seed_pu(&s, child, pb, 1).await;
    // C: assigned but DISABLED (product_user.status 0) → excluded.
    let pc = uid(BASE);
    seed_product(&s, pc, "产品C", 1, 1).await;
    seed_pu(&s, child, pc, 0).await;
    // D: opened but NOT displayed → excluded.
    let pd = uid(BASE);
    seed_product(&s, pd, "产品D", 1, 0).await;
    seed_pu(&s, child, pd, 1).await;

    let rows = downline_rate_edit(&s.db, child).await.unwrap();
    // Only A and B remain, ordered by product id ascending (A seeded first).
    assert_eq!(rows.len(), 2, "closed / undisplayed products are excluded");
    assert_eq!(rows[0].product_id, pa);
    assert_eq!(rows[0].rate, 6_500);
    assert_eq!(rows[0].t0_rate, 8_500);
    assert_eq!(rows[1].product_id, pb);
    assert_eq!(rows[1].rate, 0, "an unpriced product defaults to 0");
    assert_eq!(rows[1].t0_fengding, 0);
}

#[tokio::test]
async fn downline_rate_edit_is_empty_without_opened_products() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let child = uid(BASE);
    seed_member(&s, agent, 1).await;
    seed_member(&s, child, agent).await;
    let rows = downline_rate_edit(&s.db, child).await.unwrap();
    assert!(
        rows.is_empty(),
        "a child with no product rows yields nothing"
    );
}
