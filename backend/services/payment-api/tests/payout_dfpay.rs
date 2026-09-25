//! DB-gated tests for the payout-API `Dfpay::add` wire core (`spec/04` §7.1 /
//! §7.2 / §7.4): the §7.1 business-guard chain (报备 domain / IP, holiday, the
//! effective-config window + bounds, the money / card / order-no validations,
//! the merchant-scoped duplicate rejection), the §7.2 no-debit filing, and the
//! §7.4 auto-review `df_pass` for an `df_auto_check` merchant (with the
//! compensating pending-row delete when the debit guard loses). Identity /
//! signature are handler-level and deliberately bypassed here — they ride the
//! `sign` unit tests. Same harness as `payout_review.rs`; ids base
//! 11_600_000_000_000.

mod common;

use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

use common::{suite, uid, Suite};
use payment_api::data::{members, payout_orders, tikuan_configs};
use payment_api::gateway::dfpay::{apply_payout, find_application, ref_code_for, DfApplyInput};
use payment_api::payout::state::{CheckStatus, PayoutStatus};
use payment_api::payout::PayoutService;

const K: i64 = 10_000; // 1 元 in money units
const BASE: i64 = 11_600_000_000_000;

fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}

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

/// Seeds an enabled payout-API merchant with a `systemxz = 1` personal rule
/// (window disabled, 1元 min, huge max, `fixed_fee` charged by `charge`), and
/// returns its persisted row so `apply_payout` can read the df_* gates off it.
async fn seed_merchant(
    s: &Suite,
    user: i64,
    balance: i64,
    fixed_fee: i64,
    charge: i32,
    auto_check: bool,
) -> members::Model {
    ensure_system_row(s).await;
    let created = members::ActiveModel {
        id: Set(user),
        username: Set(format!("d{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(balance),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(1),
        apikey: Set(Some("32charapikey000000000000000000aa".into())),
        df_auto_check: Set(i32::from(auto_check)),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    tikuan_configs::ActiveModel {
        id: Set(uid(BASE)),
        user_id: Set(user),
        t1zt: Set(0),
        tkzt: Set(1),
        systemxz: Set(1),
        issystem: Set(0),
        tkzx_money: Set(K),
        tkzd_money: Set(1_000_000 * K),
        dayzd_money: Set(0),
        dayzd_num: Set(0),
        allow_start: Set(0),
        allow_end: Set(0),
        daycardzd_money: Set(0),
        tk_type: Set(i32::from(fixed_fee > 0)),
        sx_rate: Set(0),
        sxf_fixed: Set(fixed_fee),
        tk_charge_type: Set(charge),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    created
}

fn input(money: &str, out_trade_no: &str) -> DfApplyInput {
    DfApplyInput {
        money_yuan: money.into(),
        out_trade_no: out_trade_no.into(),
        bankname: "工商银行".into(),
        subbranch: "测试支行".into(),
        accountname: "张三".into(),
        cardnumber: "622202000000DF".into(),
        province: "北京".into(),
        city: "北京".into(),
        extends_raw: String::new(),
        client_ip: String::new(),
        referer: String::new(),
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

async fn svc(s: &Suite) -> PayoutService {
    PayoutService::new((*s.db).clone())
}

async fn row(s: &Suite, user: i64, out_trade_no: &str) -> Option<payout_orders::Model> {
    payout_orders::Entity::find()
        .filter(payout_orders::Column::UserId.eq(user))
        .filter(payout_orders::Column::OutTradeNo.eq(out_trade_no))
        .one(&*s.db)
        .await
        .unwrap()
}

// --- §7.2 filing (no auto-review) -------------------------------------------

#[tokio::test]
async fn add_files_a_pending_application_with_balance_untouched() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let member = seed_merchant(&s, user, 300 * K, 0, 0, false).await;

    let res = apply_payout(
        &s.db,
        &svc(&s).await,
        &member,
        &input("100.00", "DFW-ADD-1"),
        now_ts(),
    )
    .await
    .unwrap();
    match res {
        payment_api::gateway::dfpay::ApplyResult::Success { transaction_id } => {
            let order = row(&s, user, "DFW-ADD-1").await.unwrap();
            assert_eq!(order.order_no, transaction_id);
            assert_eq!(order.source, 3);
            assert_eq!(order.check_status, Some(CheckStatus::Pending.code()));
            assert_eq!(order.status, PayoutStatus::Pending.code());
            assert_eq!(order.tkmoney, 100 * K);
            assert_eq!(balance(&s, user).await, 300 * K, "no debit on filing");
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[tokio::test]
async fn add_rejects_a_duplicate_out_trade_no() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let member = seed_merchant(&s, user, 300 * K, 0, 0, false).await;
    let db = &s.db;
    let payout = svc(&s).await;

    assert!(matches!(
        apply_payout(
            db,
            &payout,
            &member,
            &input("100.00", "DFW-DUP-1"),
            now_ts()
        )
        .await
        .unwrap(),
        payment_api::gateway::dfpay::ApplyResult::Success { .. }
    ));
    let res = apply_payout(
        db,
        &payout,
        &member,
        &input("100.00", "DFW-DUP-1"),
        now_ts(),
    )
    .await
    .unwrap();
    match res {
        payment_api::gateway::dfpay::ApplyResult::Rejected { msg } => {
            assert_eq!(msg, "存在重复订单号！");
        }
        other => panic!("expected dup rejection, got {other:?}"),
    }
    // Still exactly one row.
    let rows = payout_orders::Entity::find()
        .filter(payout_orders::Column::UserId.eq(user))
        .all(&*s.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn add_validates_amount_bounds_and_required_fields() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let member = seed_merchant(&s, user, 300 * K, 0, 0, false).await;
    let db = &s.db;
    let payout = svc(&s).await;

    // below the 1元 single-txn minimum
    let mut low = input("0.50", "DFW-LOW-1");
    low.bankname.clear();
    let res = apply_payout(db, &payout, &member, &low, now_ts())
        .await
        .unwrap();
    let payment_api::gateway::dfpay::ApplyResult::Rejected { msg } = res else {
        panic!("expected rejection, got {res:?}");
    };
    assert!(msg.starts_with("单笔最低提款额度："), "{msg}");

    // a missing payee field
    let mut missing = input("100.00", "DFW-MISS-1");
    missing.cardnumber.clear();
    let res = apply_payout(db, &payout, &member, &missing, now_ts())
        .await
        .unwrap();
    let payment_api::gateway::dfpay::ApplyResult::Rejected { msg } = res else {
        panic!("expected rejection, got {res:?}");
    };
    assert_eq!(msg, "银行卡号不能为空！");
}

// --- §7.4 auto-review -------------------------------------------------------

#[tokio::test]
async fn add_auto_reviews_and_debits_when_configured() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    // 5元 fixed fee charged off the BALANCE → the auto approve costs 105元.
    let member = seed_merchant(&s, user, 300 * K, 5 * K, 1, true).await;

    let res = apply_payout(
        &s.db,
        &svc(&s).await,
        &member,
        &input("100.00", "DFW-AUTO-1"),
        now_ts(),
    )
    .await
    .unwrap();
    assert!(matches!(
        res,
        payment_api::gateway::dfpay::ApplyResult::Success { .. }
    ));
    let order = row(&s, user, "DFW-AUTO-1").await.unwrap();
    assert_eq!(order.check_status, Some(CheckStatus::Approved.code()));
    assert_eq!(order.status, PayoutStatus::Pending.code(), "queue-ready");
    assert_eq!(order.sxfmoney, 5 * K);
    assert_eq!(balance(&s, user).await, 195 * K); // 300 - 100 - 5
}

#[tokio::test]
async fn add_auto_review_rolls_back_the_row_when_the_debit_breaches() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    // Balance covers the 100元 principal but not the 5元 balance-charged fee →
    // the auto df_pass loses the guarded debit and the pending row is deleted
    // (the legacy's file + dfPass shared one rollback).
    let member = seed_merchant(&s, user, 100 * K, 5 * K, 1, true).await;

    let res = apply_payout(
        &s.db,
        &svc(&s).await,
        &member,
        &input("100.00", "DFW-AUTO-2"),
        now_ts(),
    )
    .await
    .unwrap();
    let payment_api::gateway::dfpay::ApplyResult::Rejected { msg } = res else {
        panic!("expected rejection, got {res:?}");
    };
    assert!(
        msg.contains("余额") || msg.contains("金额"),
        "unexpected df_pass msg: {msg}"
    );
    assert!(
        row(&s, user, "DFW-AUTO-2").await.is_none(),
        "the just-filed pending row is compensated away"
    );
    assert_eq!(balance(&s, user).await, 100 * K, "no debit survived");
}

// --- the §7.1 domain / IP gate ----------------------------------------------

#[tokio::test]
async fn add_enforces_the_reported_ip_whitelist() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let mut member = seed_merchant(&s, user, 300 * K, 0, 0, false).await;
    member.df_ip = Some("10.0.0.1\r\n10.0.0.2".into());
    let db = &s.db;
    let payout = svc(&s).await;

    // a client IP off the whitelist is refused before any filing
    let mut bad = input("100.00", "DFW-IP-1");
    bad.client_ip = "10.9.9.9".into();
    let res = apply_payout(db, &payout, &member, &bad, now_ts())
        .await
        .unwrap();
    let payment_api::gateway::dfpay::ApplyResult::Rejected { msg } = res else {
        panic!("expected IP rejection, got {res:?}");
    };
    assert_eq!(msg, "IP地址与报备IP不一致！");
    assert!(row(&s, user, "DFW-IP-1").await.is_none());

    // an on-whitelist IP proceeds to a clean filing
    let mut good = input("100.00", "DFW-IP-2");
    good.client_ip = "10.0.0.2".into();
    assert!(matches!(
        apply_payout(db, &payout, &member, &good, now_ts())
            .await
            .unwrap(),
        payment_api::gateway::dfpay::ApplyResult::Success { .. }
    ));
}

// --- the §7.5 query lookup + ladder -----------------------------------------

#[tokio::test]
async fn query_lookup_then_reflects_the_review_ladder() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let member = seed_merchant(&s, user, 300 * K, 0, 0, false).await;
    let payout = svc(&s).await;

    apply_payout(
        &s.db,
        &payout,
        &member,
        &input("100.00", "DFW-Q-1"),
        now_ts(),
    )
    .await
    .unwrap();
    let pending = find_application(&s.db, user, "DFW-Q-1").await.unwrap();
    let pending = pending.expect("application is queryable");
    assert_eq!(
        ref_code_for(pending.check_status, pending.status),
        ("6", "待审核")
    );

    // Approving it moves the reported ladder to the execution status.
    payout.df_pass(&pending.order_no, now_ts()).await.unwrap();
    let approved = find_application(&s.db, user, "DFW-Q-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ref_code_for(approved.check_status, approved.status),
        ("4", "待处理")
    );
}
