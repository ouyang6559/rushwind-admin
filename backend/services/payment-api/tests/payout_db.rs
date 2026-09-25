//! DB-gated payout-domain tests (`spec/04` §3.3 / §4.2 / §13.3): the
//! settlement withdrawal through [`PayoutService`] — the atomic submit tx
//! (guards + guarded debit + order + chained lx=6/16 flows), the out-trade
//! idempotency net, the reject refund double (lx 11/17) and the paid CAS.
//! Same harness and isolation rules as the other suites; ids ride base
//! 11_300_000_000_000.

mod common;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
};

use common::{suite, uid, Suite};
use payment_api::data::{members, money_changes, payout_orders, tikuan_configs};
use payment_api::payout::state::PayoutStatus;
use payment_api::payout::{
    BankSnapshot, PaidOutcome, PayoutService, RejectOutcome, SubmitWithdrawal,
};

const K: i64 = 10_000; // 1 元 in money units

fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}

fn svc(s: &Suite) -> PayoutService {
    PayoutService::new((*s.db).clone())
}

/// The knobs a payout test varies off the platform defaults.
#[derive(Clone, Copy)]
struct TestCfg {
    tkzx_money: i64,
    tkzd_money: i64,
    dayzd_money: i64,
    tk_type: i32,
    sx_rate: i64,
    sxf_fixed: i64,
    tk_charge_type: i32,
}

impl Default for TestCfg {
    fn default() -> Self {
        TestCfg {
            tkzx_money: 10 * K,
            tkzd_money: 1_000_000 * K,
            dayzd_money: 0,
            tk_type: 0,
            sx_rate: 0,
            sxf_fixed: 0,
            tk_charge_type: 0,
        }
    }
}

/// The one global system row `resolve` requires (a missing `issystem = 1`
/// row means "提款已关闭"). Fixed id + `ON CONFLICT DO NOTHING` so every
/// test (and the parallel suite binaries) converge on a single row; its
/// limits are all-unlimited because the personal row wins on everything but
/// the (disabled) time window.
async fn ensure_system_row(s: &Suite) {
    s.db.execute_unprepared(
        "INSERT INTO tikuan_configs \
             (id, user_id, t1zt, tkzt, systemxz, issystem, tkzx_money, tkzd_money, \
              dayzd_money, dayzd_num, allow_start, allow_end, daycardzd_money, \
              tk_type, sx_rate, sxf_fixed, tk_charge_type) \
             VALUES (1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0) \
             ON CONFLICT (id) DO NOTHING",
    )
    .await
    .unwrap();
}

/// A merchant on its own `tikuan_configs` personal row (`systemxz = 1`), so
/// tests never fight over the global system row. The clock/week gates are
/// disabled (only amount/daily/card/fee rules are exercised here).
async fn seed_merchant(s: &Suite, user: i64, balance: i64, c: TestCfg) {
    ensure_system_row(s).await;
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("p{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(balance),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    tikuan_configs::ActiveModel {
        id: Set(uid(11_300_000_000_000)),
        user_id: Set(user),
        t1zt: Set(0),
        tkzt: Set(1),
        systemxz: Set(1),
        issystem: Set(0),
        tkzx_money: Set(c.tkzx_money),
        tkzd_money: Set(c.tkzd_money),
        dayzd_money: Set(c.dayzd_money),
        dayzd_num: Set(0),
        allow_start: Set(0),
        allow_end: Set(0),
        daycardzd_money: Set(0),
        tk_type: Set(c.tk_type),
        sx_rate: Set(c.sx_rate),
        sxf_fixed: Set(c.sxf_fixed),
        tk_charge_type: Set(c.tk_charge_type),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

fn bank(card: &str) -> BankSnapshot {
    BankSnapshot {
        bankname: Some("工商银行".into()),
        subbranch: Some("测试支行".into()),
        accountname: Some("张三".into()),
        cardnumber: Some(card.into()),
        province: Some("北京".into()),
        city: Some("北京".into()),
    }
}

async fn balance(s: &Suite, user: i64) -> i64 {
    members::Entity::find_by_id(user)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
        .balance
}

async fn flows(s: &Suite, order_no: &str) -> Vec<money_changes::Model> {
    money_changes::Entity::find()
        .filter(money_changes::Column::OrderId.eq(order_no.to_string()))
        .order_by_asc(money_changes::Column::Id)
        .all(&*s.db)
        .await
        .unwrap()
}

fn submit(user: i64, amount: i64, card: &str) -> SubmitWithdrawal<'static> {
    SubmitWithdrawal {
        user_id: user,
        amount,
        out_trade_no: None,
        bank: bank(card),
    }
}

#[tokio::test]
async fn arrival_charged_withdrawal_books_order_and_one_debit_flow() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    let c = TestCfg {
        sx_rate: 20_000,
        ..TestCfg::default()
    }; // 2%, arrival-charged
    seed_merchant(&s, user, 300 * K, c).await;

    let out = svc(&s)
        .submit_withdrawal(&submit(user, 100 * K, "6222020000001"), now_ts())
        .await
        .unwrap();
    assert_eq!(out.order.status, PayoutStatus::Pending.code());
    assert_eq!(out.order.source, 1);
    assert_eq!(
        (out.order.tkmoney, out.order.sxfmoney, out.order.money),
        (100 * K, 2 * K, 98 * K)
    );
    assert_eq!(out.order.charge_type, 0);
    // The balance only lost the principal (§3.3 tk_charge_type=0).
    assert_eq!(out.balance_after, 200 * K);
    assert_eq!(balance(&s, user).await, 200 * K);

    // One lx=6 flow, signed delta, order_no on both id columns.
    let fs = flows(&s, &out.order.order_no).await;
    assert_eq!(fs.len(), 1);
    assert_eq!((fs[0].lx, fs[0].money), (6, -100 * K));
    assert_eq!((fs[0].y_money, fs[0].g_money), (300 * K, 200 * K));
    assert_eq!(fs[0].trans_id.as_deref(), Some(out.order.order_no.as_str()));
}

#[tokio::test]
async fn balance_charged_withdrawal_chains_lx6_then_lx16() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    let c = TestCfg {
        tk_type: 1, // fixed fee
        sxf_fixed: 5 * K,
        tk_charge_type: 1, // fee out of the balance
        ..TestCfg::default()
    };
    seed_merchant(&s, user, 300 * K, c).await;

    let out = svc(&s)
        .submit_withdrawal(&submit(user, 100 * K, "6222020000002"), now_ts())
        .await
        .unwrap();
    assert_eq!(
        (out.order.tkmoney, out.order.sxfmoney, out.order.money),
        (100 * K, 5 * K, 100 * K)
    );
    assert_eq!(out.order.charge_type, 1);
    assert_eq!(out.balance_after, 195 * K); // 300 - 100 - 5

    // 连续账: 300 →(lx6 −100)→ 200 →(lx16 −5)→ 195 — the legacy's gmoney
    // double-deduct (§3.3) never re-subtracts the fee.
    let fs = flows(&s, &out.order.order_no).await;
    assert_eq!(fs.len(), 2);
    assert_eq!(
        (fs[0].lx, fs[0].y_money, fs[0].money, fs[0].g_money),
        (6, 300 * K, -100 * K, 200 * K)
    );
    assert_eq!(
        (fs[1].lx, fs[1].y_money, fs[1].money, fs[1].g_money),
        (16, 200 * K, -5 * K, 195 * K)
    );
}

#[tokio::test]
async fn guards_reject_before_any_write() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    let c = TestCfg {
        tkzx_money: 50 * K,
        ..TestCfg::default()
    };
    seed_merchant(&s, user, 300 * K, c).await;

    // Below the single-minimum …
    let err = svc(&s)
        .submit_withdrawal(&submit(user, 20 * K, "6222020000003"), now_ts())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        payment_api::state::GatewayError::BadRequest(m) if m.contains("单笔提现最小金额")
    ));
    // … over the single-maximum …
    let err = svc(&s)
        .submit_withdrawal(&submit(user, 2_000_000 * K, "6222020000003"), now_ts())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        payment_api::state::GatewayError::BadRequest(m) if m.contains("单笔提现最大金额")
    ));
    // … and the insufficient-balance message the guard chain carries.
    let err = svc(&s)
        .submit_withdrawal(&submit(user, 400 * K, "6222020000003"), now_ts())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        payment_api::state::GatewayError::BadRequest(m) if m == "账户余额不足"
    ));
    // Nothing was written either way.
    assert_eq!(balance(&s, user).await, 300 * K);
    assert_eq!(count_user_orders(&s, user).await, 0);
}

#[tokio::test]
async fn rejected_orders_keep_burning_the_daily_quota() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    let c = TestCfg {
        dayzd_money: 150 * K,
        ..TestCfg::default()
    };
    seed_merchant(&s, user, 300 * K, c).await;

    let out = svc(&s)
        .submit_withdrawal(&submit(user, 100 * K, "6222020000004"), now_ts())
        .await
        .unwrap();
    // Reject it (the refund gives the balance back)…
    svc(&s)
        .reject(&out.order.order_no, Some("资料不全"))
        .await
        .unwrap();
    assert_eq!(balance(&s, user).await, 300 * K);
    // …but the legacy day roll-up never filters status: the rejected 100元
    // still counts, so a second 60元 breaches the 150元 cap (§3.3 step 9).
    let err = svc(&s)
        .submit_withdrawal(&submit(user, 60 * K, "6222020000004"), now_ts())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        payment_api::state::GatewayError::BadRequest(m) if m.contains("提现额度不足")
    ));
    assert_eq!(count_user_orders(&s, user).await, 1);
}

#[tokio::test]
async fn same_out_trade_no_resurfaces_the_first_order() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    seed_merchant(&s, user, 300 * K, TestCfg::default()).await;
    let service = svc(&s);

    let req = SubmitWithdrawal {
        user_id: user,
        amount: 100 * K,
        out_trade_no: Some("df-clone-001"),
        bank: bank("6222020000005"),
    };
    let first = service.submit_withdrawal(&req, now_ts()).await.unwrap();
    let second = service.submit_withdrawal(&req, now_ts()).await.unwrap();
    assert_eq!(first.order.order_no, second.order.order_no);
    // One debit, one order — the replay moved no money.
    assert_eq!(balance(&s, user).await, 200 * K);
    assert_eq!(count_user_orders(&s, user).await, 1);
}

#[tokio::test]
async fn own_withdrawals_never_share_the_out_trade_slot() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    seed_merchant(&s, user, 300 * K, TestCfg::default()).await;
    let service = svc(&s);

    // NULL out_trade_no rows must not collide under the partial unique index.
    let a = service
        .submit_withdrawal(&submit(user, 50 * K, "6222020000006"), now_ts())
        .await
        .unwrap();
    let b = service
        .submit_withdrawal(&submit(user, 50 * K, "6222020000006"), now_ts())
        .await
        .unwrap();
    assert_ne!(a.order.order_no, b.order.order_no);
    assert_eq!(balance(&s, user).await, 200 * K);
}

#[tokio::test]
async fn double_reject_refunds_once_and_cas_resurfaces() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    let c = TestCfg {
        tk_type: 1,
        sxf_fixed: 5 * K,
        tk_charge_type: 1, // fee balance-charged → fee is refundable
        ..TestCfg::default()
    };
    seed_merchant(&s, user, 300 * K, c).await;
    let service = svc(&s);

    let out = service
        .submit_withdrawal(&submit(user, 100 * K, "6222020000007"), now_ts())
        .await
        .unwrap();
    assert_eq!(out.balance_after, 195 * K);

    // First reject: principal + fee back, lx 11 then 17 (§4.2).
    let res = service
        .reject(&out.order.order_no, Some("卡号有误"))
        .await
        .unwrap();
    let RejectOutcome::Refunded {
        refund,
        balance_after,
        order,
    } = res
    else {
        panic!("expected a refund, got {res:?}");
    };
    assert_eq!((refund.principal, refund.fee), (100 * K, 5 * K));
    assert_eq!(balance_after, 300 * K);
    assert_eq!(order.status, PayoutStatus::Failed.code());
    assert_eq!(order.reject_reason.as_deref(), Some("卡号有误"));

    // Replay: already terminal-3, guard refuses (no second refund).
    let res = service.reject(&out.order.order_no, None).await.unwrap();
    assert!(matches!(res, RejectOutcome::NotRejectable(_)));
    assert_eq!(balance(&s, user).await, 300 * K);

    let fs = flows(&s, &out.order.order_no).await;
    assert_eq!(fs.len(), 4);
    assert_eq!(
        (fs[2].lx, fs[2].money, fs[2].g_money),
        (11, 100 * K, 295 * K)
    );
    assert_eq!((fs[3].lx, fs[3].money, fs[3].g_money), (17, 5 * K, 300 * K));
    // 连续账 across the whole order life: 300→195→300.
    assert_eq!(fs[3].g_money, fs[0].y_money);
}

#[tokio::test]
async fn mark_paid_cas_is_single_stamp() {
    let Some(s) = suite().await else { return };
    let user = uid(11_300_000_000_000);
    seed_merchant(&s, user, 300 * K, TestCfg::default()).await;
    let service = svc(&s);

    let out = service
        .submit_withdrawal(&submit(user, 100 * K, "6222020000008"), now_ts())
        .await
        .unwrap();
    let PaidOutcome::Transitioned(order) = service.mark_paid(&out.order.order_no).await.unwrap()
    else {
        panic!("pending order must transition");
    };
    assert_eq!(order.status, PayoutStatus::Success.code());
    assert!(order.settled_at.unwrap() > 0);

    // Replay keeps the first settle time (§4.2 case 2 never re-stamps).
    let PaidOutcome::AlreadyPaid(order2) = service.mark_paid(&out.order.order_no).await.unwrap()
    else {
        panic!("paid order must not re-transition");
    };
    assert_eq!(order2.settled_at, order.settled_at);

    // A rejected order is not payable.
    let other = service
        .submit_withdrawal(&submit(user, 100 * K, "6222020000008"), now_ts())
        .await
        .unwrap();
    service.reject(&other.order.order_no, None).await.unwrap();
    assert!(matches!(
        service.mark_paid(&other.order.order_no).await.unwrap(),
        PaidOutcome::Rejected(_)
    ));
    // Paying / rejecting never move the balance again: only the first (paid)
    // 100元 withdrawal stays debited, the rejected one refunded itself.
    assert_eq!(balance(&s, user).await, 200 * K);
}

async fn count_user_orders(s: &Suite, user: i64) -> i64 {
    payout_orders::Entity::find()
        .filter(payout_orders::Column::UserId.eq(user))
        .all(&*s.db)
        .await
        .unwrap()
        .len() as i64
}
