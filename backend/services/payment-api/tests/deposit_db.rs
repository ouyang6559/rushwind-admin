//! DB-gated complaints-deposit tests (`spec/02` §4.4 withholding + §6.3
//! release): the rule resolution chain (own active row ?: system fallback),
//! the in-tx freeze-ledger write, the reduced net arrival, and the scheduled
//! unfreeze sweep with its `lx = 13` flow. Scenarios run SEQUENTIALLY inside
//! one test because the system-rule row is global state in the shared DB.
//! Runs only with `PAYMENT_TEST_DATABASE_URL` set; base 9.7e12.

mod common;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use common::{create_only, order_row, seed_world, suite, uid, Suite};
use payment_api::data::{complaints_deposit_rules, complaints_deposits, members, money_changes};
use payment_api::ledger::SettleOutcome;

const BASE: i64 = 9_700_000_000_000;

async fn rule(
    s: &Suite,
    user_id: i64,
    is_system: i32,
    ratio_pct: i32,
    freeze_time: i64,
    status: i32,
) {
    complaints_deposit_rules::ActiveModel {
        user_id: Set(user_id),
        is_system: Set(is_system),
        ratio_pct: Set(ratio_pct),
        freeze_time: Set(freeze_time),
        status: Set(status),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// Settles one fresh 100元 T+0 order (0.8% fee → 99.20元 arrival) and
/// returns the settle outcome.
async fn settle_fresh_order(
    s: &Suite,
    user: i64,
    channel: i64,
) -> payment_api::ledger::SettleOutcome {
    let oid = format!("D{}", uid(BASE));
    create_only(s, user, channel, "http://127.0.0.1:1/notify", "", &oid).await;
    s.ledger.settle_order(&oid).await.unwrap()
}

async fn member_balance(s: &Suite, user: i64) -> i64 {
    members::Entity::find_by_id(user)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
        .balance
}

async fn deposit_row_for(s: &Suite, order_id: &str) -> complaints_deposits::Model {
    complaints_deposits::Entity::find()
        .filter(complaints_deposits::Column::PayOrderid.eq(order_id))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
}

async fn flow_lx_count(s: &Suite, user: i64, lx: i32) -> usize {
    money_changes::Entity::find()
        .filter(money_changes::Column::UserId.eq(user))
        .filter(money_changes::Column::Lx.eq(lx))
        .all(&*s.db)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn rule_chain_withholding_and_the_release_sweep() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };

    // ── no rules at all: nothing withheld ────────────────────────────────
    complaints_deposit_rules::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
    complaints_deposits::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
    let (user_a, channel_a) = (uid(BASE), uid(BASE));
    seed_world(&s, user_a, channel_a).await;
    match settle_fresh_order(&s, user_a, channel_a).await {
        SettleOutcome::Settled(w) => assert!(w.deposit.is_none()),
        other => panic!("{other:?}"),
    }
    assert_eq!(member_balance(&s, user_a).await, 992_000); // full net arrival

    // ── system rule active: 5% withheld into the freeze ledger ───────────
    rule(&s, 0, 1, 5, 3_600, 1).await;
    let (user_b, channel_b) = (uid(BASE), uid(BASE));
    seed_world(&s, user_b, channel_b).await;
    let oid_b = format!("D{}", uid(BASE));
    create_only(
        &s,
        user_b,
        channel_b,
        "http://127.0.0.1:1/notify",
        "",
        &oid_b,
    )
    .await;
    let before = chrono::Local::now().timestamp();
    match s.ledger.settle_order(&oid_b).await.unwrap() {
        SettleOutcome::Settled(w) => {
            let deposit = w.deposit.expect("the system rule withholds 5%");
            assert_eq!(deposit.amount, 49_600); // 5% of 99.20元, exact
            assert_eq!(deposit.unfreeze_at, before + 3_600);
        }
        other => panic!("{other:?}"),
    }
    let row_b = deposit_row_for(&s, &oid_b).await;
    assert_eq!(row_b.freeze_money, 49_600);
    assert_eq!(row_b.status, 0);
    assert_eq!(row_b.is_pause, 0);
    assert!(row_b.unfreeze_time >= before + 3_600);
    // The merchant holds the NET-of-deposit arrival; the deposit itself
    // never touched `balance` (§4.4).
    assert_eq!(member_balance(&s, user_b).await, 942_400);
    assert_eq!(flow_lx_count(&s, user_b, 1).await, 1);
    let b_flow = money_changes::Entity::find()
        .filter(money_changes::Column::UserId.eq(user_b))
        .filter(money_changes::Column::Lx.eq(1))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b_flow.money, 942_400, "the lx=1 flow records the NET");

    // ── the merchant's own ACTIVE rule wins over the system row ──────────
    let (user_c, channel_c) = (uid(BASE), uid(BASE));
    seed_world(&s, user_c, channel_c).await;
    rule(&s, user_c, 0, 10, 60, 1).await;
    match settle_fresh_order(&s, user_c, channel_c).await {
        SettleOutcome::Settled(w) => {
            assert_eq!(w.deposit.expect("own rule").amount, 99_200); // 10%
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(member_balance(&s, user_c).await, 892_800);

    // ── an INACTIVE own row falls through to the system row (legacy
    //    getComplaintsDepositRule semantics) ──────────────────────────────
    let (user_d, channel_d) = (uid(BASE), uid(BASE));
    seed_world(&s, user_d, channel_d).await;
    rule(&s, user_d, 0, 90, 60, 0).await; // own row present but disabled
    match settle_fresh_order(&s, user_d, channel_d).await {
        SettleOutcome::Settled(w) => {
            assert_eq!(w.deposit.expect("system fallback applies").amount, 49_600);
        }
        other => panic!("{other:?}"),
    }

    // ── the release sweep: backdate, sweep, verify the lx=13 credit ──────
    let mut am: complaints_deposits::ActiveModel = deposit_row_for(&s, &oid_b).await.into();
    am.unfreeze_time = Set(before - 1);
    am.update(&*s.db).await.unwrap();
    let report = s.ledger.run_deposit_unfreeze_sweep().await.unwrap();
    assert_eq!(report.released, 1, "only the backdated row was due");
    assert_eq!(report.scanned, 1);

    let released = deposit_row_for(&s, &oid_b).await;
    assert_eq!(released.status, 1);
    assert!(released.real_unfreeze_time > 0);
    // The deposit money lands back in `balance` (§6.3).
    assert_eq!(member_balance(&s, user_b).await, 942_400 + 49_600);
    assert_eq!(flow_lx_count(&s, user_b, 13).await, 1);

    // The order row closed and the T+0 arrival reconciles: net + deposit
    // == the stored actual_amount.
    let order_b = order_row(&s.db, &oid_b).await;
    assert_eq!(order_b.status, 1);
    assert_eq!(942_400 + 49_600, order_b.actual_amount);

    // Best-effort cleanup: the SYSTEM rule is global state in the shared
    // test DB — other binaries settle orders and assert exact balances.
    complaints_deposit_rules::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
    complaints_deposits::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
}
