//! DB-gated manual-reversal tests (`spec/02` §6.5/§11 `Redo`): the write
//! side the legacy never built. One tx must land the guarded balance move,
//! the `money_changes` flow (lx 3/4) and the `redo_orders` ledger row the
//! merchant-income formula aggregates. Runs only with
//! `PAYMENT_TEST_DATABASE_URL` set; base 9.9e12 keeps ids disjoint.

mod common;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use common::{suite, uid, Suite};
use payment_api::data::{members, money_changes, redo_orders};
use payment_api::ledger::RedoType;

const BASE: i64 = 9_900_000_000_000;

async fn seed_member(s: &Suite, available: i64) -> i64 {
    use sea_orm::{ActiveModelTrait, Set};
    let user = uid(BASE);
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("rd{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(available),
        blocked_balance: Set(0),
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

#[tokio::test]
async fn reversal_lands_ledger_row_flow_and_balance_in_one_tx() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let period = chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    // ── type=1 增加: credit + lx=3 flow + the ledger row ─────────────────
    let user = seed_member(&s, 100_000).await; // 10元
    let after = s
        .ledger
        .redo_balance(user, 50_000, RedoType::Increase, "补差错账", period, 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((after.available, after.blocked), (150_000, 0));

    let row = redo_orders::Entity::find()
        .filter(redo_orders::Column::UserId.eq(user))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.redo_type, 1);
    assert_eq!(row.money, 50_000);
    assert_eq!(row.admin_id, 7);
    assert_eq!(row.remark, "补差错账");
    assert_eq!(row.date, period);
    assert!(row.ctime > 0);

    let flow = money_changes::Entity::find()
        .filter(money_changes::Column::UserId.eq(user))
        .filter(money_changes::Column::Lx.eq(3))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(flow.money, 50_000);
    assert_eq!(flow.g_money, flow.y_money + flow.money);

    // ── type=2 减少: guarded debit + lx=4 flow + a type=2 row ────────────
    let after = s
        .ledger
        .redo_balance(user, 30_000, RedoType::Decrease, "冲正多入账", period, 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.available, 120_000);
    let dec = redo_orders::Entity::find()
        .filter(redo_orders::Column::UserId.eq(user))
        .filter(redo_orders::Column::RedoType.eq(2))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dec.money, 30_000);
    let flow4 = money_changes::Entity::find()
        .filter(money_changes::Column::UserId.eq(user))
        .filter(money_changes::Column::Lx.eq(4))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(flow4.money, -30_000);

    // The statistics read-side (`spec/02` §6.5): income = type1 - type2.
    let (inc, dec): (i64, i64) = (
        redo_orders::Entity::find()
            .filter(redo_orders::Column::UserId.eq(user))
            .filter(redo_orders::Column::RedoType.eq(1))
            .all(&*s.db)
            .await
            .unwrap()
            .iter()
            .map(|r| r.money)
            .sum(),
        redo_orders::Entity::find()
            .filter(redo_orders::Column::UserId.eq(user))
            .filter(redo_orders::Column::RedoType.eq(2))
            .all(&*s.db)
            .await
            .unwrap()
            .iter()
            .map(|r| r.money)
            .sum(),
    );
    assert_eq!(inc - dec, 20_000);

    // ── guards: non-positive refused up front, overdraft returns None ────
    let err = s
        .ledger
        .redo_balance(user, 0, RedoType::Increase, "", period, 7)
        .await
        .unwrap_err();
    assert_eq!(err.message(), "金额必须大于0");
    let err = s
        .ledger
        .redo_balance(user, -1, RedoType::Decrease, "", period, 7)
        .await
        .unwrap_err();
    assert_eq!(err.message(), "金额必须大于0");
    assert_eq!(
        s.ledger
            .redo_balance(user, 1_000_000, RedoType::Decrease, "", period, 7)
            .await
            .unwrap(),
        None,
        "decrease over the balance writes nothing"
    );
    let rows = redo_orders::Entity::find()
        .filter(redo_orders::Column::UserId.eq(user))
        .order_by_asc(redo_orders::Column::Id)
        .all(&*s.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "the guards wrote no ledger rows");
}
