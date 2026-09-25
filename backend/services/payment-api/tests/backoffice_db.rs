//! DB-gated admin balance-move tests (`spec/05` §11, `incrMoney` /
//! `frozenMoney` through [`LedgerService::admin_move`]): the four guarded
//! atomic moves, their `money_changes` rows (lx 3/4/7/8) and the overdraft
//! guards returning `None`. Runs only with `PAYMENT_TEST_DATABASE_URL` set;
//! base 9.8e12 keeps ids disjoint from the other binaries.

mod common;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use common::{suite, uid, Suite};
use payment_api::data::{members, money_changes};
use payment_api::ledger::AdminMove;

const BASE: i64 = 9_800_000_000_000;

/// A merchant holding `available` / `blocked`.
async fn seed_member(s: &Suite, available: i64, blocked: i64) -> i64 {
    use sea_orm::{ActiveModelTrait, Set};
    let user = uid(BASE);
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("bo{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(available),
        blocked_balance: Set(blocked),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    user
}

async fn buckets(s: &Suite, user: i64) -> (i64, i64) {
    let m = members::Entity::find_by_id(user)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    (m.balance, m.blocked_balance)
}

async fn flows(s: &Suite, user: i64, lx: i32) -> Vec<money_changes::Model> {
    money_changes::Entity::find()
        .filter(money_changes::Column::UserId.eq(user))
        .filter(money_changes::Column::Lx.eq(lx))
        .all(&*s.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_four_moves_land_with_their_flow_rows() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let user = seed_member(&s, 100_000, 20_000).await; // 10元 / 2元

    // Manual add: available +, lx = 3.
    let after = s
        .ledger
        .admin_move(AdminMove::ManualAdd, user, 50_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((after.available, after.blocked), (150_000, 20_000));

    // Manual sub: available -, lx = 4, flow money negative.
    let after = s
        .ledger
        .admin_move(AdminMove::ManualSub, user, 30_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((after.available, after.blocked), (120_000, 20_000));

    // Freeze: available → blocked, lx = 7, flow tracks the available drop.
    let after = s
        .ledger
        .admin_move(AdminMove::Freeze, user, 40_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((after.available, after.blocked), (80_000, 60_000));

    // Unfreeze: blocked → available, lx = 8, flow tracks the available gain.
    let after = s
        .ledger
        .admin_move(AdminMove::Unfreeze, user, 25_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((after.available, after.blocked), (105_000, 35_000));

    // Every move carried its flow row with the exact double-entry split.
    for (lx, delta) in [(3, 50_000), (4, -30_000), (7, -40_000), (8, 25_000)] {
        let rows = flows(&s, user, lx).await;
        assert_eq!(rows.len(), 1, "one lx={lx} flow");
        assert_eq!(rows[0].money, delta);
        assert_eq!(rows[0].g_money, rows[0].y_money + rows[0].money);
    }
}

#[tokio::test]
async fn overdraft_guards_refuse_and_write_nothing() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let user = seed_member(&s, 10_000, 2_000).await; // 1元 / 0.2元

    // Sub below available / unfreeze below blocked: None, no flow, no move.
    assert_eq!(
        s.ledger
            .admin_move(AdminMove::ManualSub, user, 20_000)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        s.ledger
            .admin_move(AdminMove::Freeze, user, 10_001)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        s.ledger
            .admin_move(AdminMove::Unfreeze, user, 2_001)
            .await
            .unwrap(),
        None
    );
    assert_eq!(buckets(&s, user).await, (10_000, 2_000));
    assert!(flows(&s, user, 4).await.is_empty());
    assert!(flows(&s, user, 7).await.is_empty());
    assert!(flows(&s, user, 8).await.is_empty());

    // A missing member surfaces as None too (the guard matched no row).
    assert_eq!(
        s.ledger
            .admin_move(AdminMove::ManualAdd, uid(BASE), 1)
            .await
            .unwrap(),
        None
    );
}
