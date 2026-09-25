//! DB-gated downstream payout-API review tests (`spec/04` §6.3 / §6.4 /
//! §7.2): the `check_status` application → approve / reject machine on the
//! unified `payout_orders` row, the debit-at-approval money trail (lx 6 +
//! balance-charged lx 14), the principal-only refund on a df reject (lx 12,
//! the fee forfeited), and the execution queue's `check_status` gate. Same
//! harness as `payout_exec.rs`; ids ride base 11_500_000_000_000.

mod common;

use sea_orm::{
    sea_query, ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, Set,
};

use common::{suite, uid, Suite};
use payment_api::data::{members, money_changes, payout_orders, tikuan_configs};
use payment_api::payout::state::{CheckStatus, PayoutStatus};
use payment_api::payout::{ApplyPayoutApi, BankSnapshot, PayoutService, ReviewOutcome, SubmitGate};

const K: i64 = 10_000; // 1 元 in money units
const BASE: i64 = 11_500_000_000_000;

fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}

fn svc(s: &Suite) -> PayoutService {
    PayoutService::new((*s.db).clone())
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

/// A merchant with a `systemxz = 1` personal config whose fee knobs are
/// passed in, so approve-time math is exactly what the test asserts.
async fn seed_merchant(
    s: &Suite,
    user: i64,
    balance: i64,
    sx_rate: i64,
    sxf_fixed: i64,
    charge: i32,
) {
    ensure_system_row(s).await;
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("r{user}")),
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
        tk_type: Set(i32::from(sxf_fixed > 0)),
        sx_rate: Set(sx_rate),
        sxf_fixed: Set(sxf_fixed),
        tk_charge_type: Set(charge),
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

fn apply<'a>(user: i64, amount: i64, out_trade_no: &'a str, card: &str) -> ApplyPayoutApi<'a> {
    ApplyPayoutApi {
        user_id: user,
        amount,
        out_trade_no,
        bank: bank(card),
        extends: Some("{\"channel_extra\":\"x\"}".into()),
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

async fn reload(s: &Suite, order_no: &str) -> payout_orders::Model {
    payout_orders::Entity::find()
        .filter(payout_orders::Column::OrderNo.eq(order_no))
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
}

async fn flows(s: &Suite, order_no: &str) -> Vec<money_changes::Model> {
    money_changes::Entity::find()
        .filter(money_changes::Column::OrderId.eq(order_no.to_string()))
        .order_by_asc(money_changes::Column::Id)
        .all(&*s.db)
        .await
        .unwrap()
}

/// Force an approved order off `status = 0` (the queue took it), simulating
/// an in-flight payout without a live channel.
async fn push_off_pending(s: &Suite, order_no: &str, status: i16) {
    payout_orders::Entity::update_many()
        .col_expr(
            payout_orders::Column::Status,
            sea_query::Expr::value(status),
        )
        .filter(payout_orders::Column::OrderNo.eq(order_no))
        .exec(&*s.db)
        .await
        .unwrap();
}

// --- the application (§7.2 add) ---------------------------------------------

#[tokio::test]
async fn application_lands_pending_with_balance_untouched() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 0, 0).await;

    let order = svc(&s)
        .apply_payout_api(&apply(user, 100 * K, "DFR-APP-1", "622202000000R1"))
        .await
        .unwrap();
    // A request, not a booking: source=3, awaiting review, NO debit yet —
    // the fee is only resolved at df_pass (§6.3 / §7.2).
    assert_eq!(order.source, 3);
    assert_eq!(order.check_status, Some(CheckStatus::Pending.code()));
    assert_eq!(order.status, PayoutStatus::Pending.code());
    assert_eq!(
        (order.tkmoney, order.sxfmoney, order.money),
        (100 * K, 0, 100 * K)
    );
    assert_eq!(order.charge_type, 0);
    assert_eq!(order.out_trade_no.as_deref(), Some("DFR-APP-1"));
    assert_eq!(
        order.additional.as_deref(),
        Some("{\"channel_extra\":\"x\"}")
    );
    assert_eq!(balance(&s, user).await, 300 * K);
    assert!(flows(&s, &order.order_no).await.is_empty());
}

#[tokio::test]
async fn application_is_idempotent_on_out_trade_no() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 0, 0).await;
    let svc_ = svc(&s);

    let first = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFR-IDEM-1", "622202000000R2"))
        .await
        .unwrap();
    let again = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFR-IDEM-1", "622202000000R2"))
        .await
        .unwrap();
    // The raced twin resurfaces the first row (§12.6), never a second order.
    assert_eq!(first.order_no, again.order_no);
    let rows = payout_orders::Entity::find()
        .filter(payout_orders::Column::UserId.eq(user))
        .all(&*s.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// --- df_pass (§6.4 审核通过) -------------------------------------------------

#[tokio::test]
async fn df_pass_debits_at_approval_time_and_chains_lx6_lx14() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    // Fixed 5元 fee charged off the BALANCE — the debit only happens on
    // approve, and the fee flow must ride lx=14 (not the settlement's 16).
    seed_merchant(&s, user, 300 * K, 0, 5 * K, 1).await;

    let order = svc(&s)
        .apply_payout_api(&apply(user, 100 * K, "DFR-PASS-1", "622202000000R3"))
        .await
        .unwrap();
    assert_eq!(balance(&s, user).await, 300 * K);

    let res = svc(&s).df_pass(&order.order_no, now_ts()).await.unwrap();
    let ReviewOutcome::Approved(after) = res else {
        panic!("expected approval, got {res:?}");
    };
    assert_eq!(after.check_status, Some(CheckStatus::Approved.code()));
    assert_eq!(after.status, PayoutStatus::Pending.code(), "queue-ready");
    // The fee math re-ran at approval: 5元 fee, balance-charged.
    assert_eq!(
        (after.sxfmoney, after.money, after.charge_type),
        (5 * K, 100 * K, 1)
    );
    assert!(after.review_time.is_some());
    assert_eq!(balance(&s, user).await, 195 * K); // 300 - 100 - 5

    // 连续账: 300 →(lx6 −100)→ 200 →(lx14 −5)→ 195.
    let fs = flows(&s, &order.order_no).await;
    assert_eq!(fs.len(), 2);
    assert_eq!(
        (fs[0].lx, fs[0].y_money, fs[0].money, fs[0].g_money),
        (6, 300 * K, -100 * K, 200 * K)
    );
    assert_eq!(
        (fs[1].lx, fs[1].y_money, fs[1].money, fs[1].g_money),
        (14, 200 * K, -5 * K, 195 * K)
    );
}

#[tokio::test]
async fn df_pass_is_idempotent_on_replay() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 5 * K, 1).await;
    let svc_ = svc(&s);

    let order = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFR-PASS-2", "622202000000R4"))
        .await
        .unwrap();
    assert!(matches!(
        svc_.df_pass(&order.order_no, now_ts()).await.unwrap(),
        ReviewOutcome::Approved(_)
    ));

    // Replaying the approve folds nothing — no second debit, no second flow.
    let res = svc_.df_pass(&order.order_no, now_ts()).await.unwrap();
    assert!(matches!(res, ReviewOutcome::AlreadyApproved(_)));
    assert_eq!(balance(&s, user).await, 195 * K);
    assert_eq!(flows(&s, &order.order_no).await.len(), 2);
}

#[tokio::test]
async fn df_pass_insufficient_balance_rolls_the_approve_back() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    // Balance covers the 100元 principal but not the 5元 balance-charged fee.
    seed_merchant(&s, user, 100 * K, 0, 5 * K, 1).await;

    let order = svc(&s)
        .apply_payout_api(&apply(user, 100 * K, "DFR-PASS-3", "622202000000R5"))
        .await
        .unwrap();
    let err = svc(&s)
        .df_pass(&order.order_no, now_ts())
        .await
        .unwrap_err();
    match err {
        payment_api::state::GatewayError::BadRequest(m) => assert_eq!(m, "余额不足！"),
        e => panic!("expected 余额不足, got {e:?}"),
    }
    // The check_status CAS rode the same tx — the rollback un-flips it.
    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.check_status, Some(CheckStatus::Pending.code()));
    assert_eq!(balance(&s, user).await, 100 * K);
    assert!(flows(&s, &order.order_no).await.is_empty());
}

// --- df_reject (§6.4 审核驳回) -----------------------------------------------

#[tokio::test]
async fn df_reject_pending_application_only_flips() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 5 * K, 1).await;

    let order = svc(&s)
        .apply_payout_api(&apply(user, 100 * K, "DFR-REJ-1", "622202000000R6"))
        .await
        .unwrap();
    let res = svc(&s)
        .df_reject(&order.order_no, "资料有误", now_ts())
        .await
        .unwrap();
    let ReviewOutcome::Rejected {
        order,
        refund,
        balance_after,
    } = res
    else {
        panic!("expected a rejection, got {res:?}");
    };
    // Never debited → nothing to refund; the row just terminalises.
    assert_eq!((refund.principal, refund.fee), (0, 0));
    assert_eq!(balance_after, 300 * K);
    assert_eq!(order.check_status, Some(CheckStatus::Rejected.code()));
    assert_eq!(order.status, PayoutStatus::Failed.code());
    assert_eq!(order.reject_reason.as_deref(), Some("资料有误"));
    assert_eq!(balance(&s, user).await, 300 * K);
    assert!(flows(&s, &order.order_no).await.is_empty());

    // A replay is an idempotent no-op.
    let res = svc(&s)
        .df_reject(&order.order_no, "资料有误", now_ts())
        .await
        .unwrap();
    assert!(matches!(res, ReviewOutcome::AlreadyRejected(_)));
}

#[tokio::test]
async fn df_reject_approved_refunds_principal_only() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 5 * K, 1).await;
    let svc_ = svc(&s);

    let order = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFR-REJ-2", "622202000000R7"))
        .await
        .unwrap();
    svc_.df_pass(&order.order_no, now_ts()).await.unwrap();
    assert_eq!(balance(&s, user).await, 195 * K);

    let res = svc_
        .df_reject(&order.order_no, "卡号有误", now_ts())
        .await
        .unwrap();
    let ReviewOutcome::Rejected {
        order,
        refund,
        balance_after,
    } = res
    else {
        panic!("expected a rejection, got {res:?}");
    };
    // §6.4 faithful quirk: ONLY the principal returns, the fee is forfeited.
    assert_eq!((refund.principal, refund.fee), (100 * K, 0));
    assert_eq!(balance_after, 295 * K);
    assert_eq!(balance(&s, user).await, 295 * K);
    assert_eq!(order.check_status, Some(CheckStatus::Rejected.code()));
    assert_eq!(order.status, PayoutStatus::Failed.code());
    assert_eq!(order.reject_reason.as_deref(), Some("卡号有误"));

    // The refund rides exactly ONE lx=12 flow (+100), no fee refund row.
    let fs = flows(&s, &order.order_no).await;
    assert_eq!(fs.len(), 3);
    let r = &fs[2];
    assert_eq!(
        (r.lx, r.y_money, r.money, r.g_money),
        (12, 195 * K, 100 * K, 295 * K)
    );
    assert!(!fs.iter().any(|f| f.lx == 15), "the fee is never refunded");
}

#[tokio::test]
async fn df_reject_refused_once_the_platform_took_it() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 0, 0).await;
    let svc_ = svc(&s);

    let order = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFR-REJ-3", "622202000000R8"))
        .await
        .unwrap();
    svc_.df_pass(&order.order_no, now_ts()).await.unwrap();
    // The queue moved it off status=0 → 「后台已处理代付，不能驳回」 (§6.4).
    push_off_pending(&s, &order.order_no, PayoutStatus::Processing.code()).await;

    let res = svc_
        .df_reject(&order.order_no, "来晚了", now_ts())
        .await
        .unwrap();
    assert!(matches!(res, ReviewOutcome::NotRejectable(_)));
    let after = reload(&s, &order.order_no).await;
    assert_eq!(after.check_status, Some(CheckStatus::Approved.code()));
    assert_eq!(after.status, PayoutStatus::Processing.code());
    assert_eq!(balance(&s, user).await, 200 * K, "no refund rode along");
}

// --- the queue gate (§6.3 × §8.1) -------------------------------------------

#[tokio::test]
async fn due_submits_skips_unreviewed_and_rejected_orders() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 0, 0).await;
    let svc_ = svc(&s);
    let gate = SubmitGate::manual(100_000);

    let order = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFR-GATE-1", "622202000000R9"))
        .await
        .unwrap();
    // status=0 but check_status=0 — a pending review is NOT queue material.
    let due = svc_.due_submits(&gate).await.unwrap();
    assert!(!due.iter().any(|o| o.order_no == order.order_no));

    svc_.df_pass(&order.order_no, now_ts()).await.unwrap();
    let due = svc_.due_submits(&gate).await.unwrap();
    assert!(due.iter().any(|o| o.order_no == order.order_no));

    // Rejected (check_status=2, status=3) drops out on both counts.
    push_off_pending(&s, &order.order_no, PayoutStatus::Pending.code()).await;
    let res = svc_
        .df_reject(&order.order_no, "撤回", now_ts())
        .await
        .unwrap();
    assert!(matches!(res, ReviewOutcome::Rejected { .. }));
    let due = svc_.due_submits(&gate).await.unwrap();
    assert!(!due.iter().any(|o| o.order_no == order.order_no));
}

// --- batch review (§6.5 dfPassBatch / dfRejectBatch) -------------------------

#[tokio::test]
async fn df_pass_batch_commits_per_row_and_tallies_success_fail() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    // 250元, fixed 5元 balance-charged fee → each approve costs 105元, so only
    // two of the three 100元 applications fit; the third rolls back ALONE.
    seed_merchant(&s, user, 250 * K, 0, 5 * K, 1).await;
    let svc_ = svc(&s);

    let mut ids = Vec::new();
    for (i, ot) in ["DFB-P-1", "DFB-P-2", "DFB-P-3"].into_iter().enumerate() {
        let o = svc_
            .apply_payout_api(&apply(user, 100 * K, ot, &format!("622202000000B{i}")))
            .await
            .unwrap();
        ids.push(o.order_no);
    }

    let rep = svc_.df_pass_batch(&ids, now_ts()).await.unwrap();
    assert_eq!(rep.succeeded_count(), 2, "two fit the balance");
    assert_eq!(
        rep.failed_count(),
        1,
        "the third breaches the balance guard"
    );
    assert_eq!(rep.summary(), "成功 2 失败 1");
    assert!(rep
        .succeeded
        .iter()
        .all(|(_, o)| matches!(o, ReviewOutcome::Approved(_))));
    assert_eq!(rep.failures[0].1, "账户余额不足");
    // 250 - 105 - 105 = 40; the failed row debited nothing.
    assert_eq!(balance(&s, user).await, 40 * K);
    // The two committed rows each wrote the lx=6 + lx=14 pair; the loser none.
    assert_eq!(flows(&s, &ids[0]).await.len(), 2);
    assert_eq!(flows(&s, &ids[1]).await.len(), 2);
    assert!(flows(&s, &ids[2]).await.is_empty());
    assert_eq!(
        reload(&s, &ids[2]).await.check_status,
        Some(CheckStatus::Pending.code()),
        "the rolled-back row stays awaiting review"
    );
}

#[tokio::test]
async fn df_reject_batch_is_per_row_independent() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_merchant(&s, user, 300 * K, 0, 5 * K, 1).await;
    let svc_ = svc(&s);

    let a = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFB-R-1", "622202000000B4"))
        .await
        .unwrap();
    let b = svc_
        .apply_payout_api(&apply(user, 100 * K, "DFB-R-2", "622202000000B5"))
        .await
        .unwrap();
    // One bogus id alongside the two real pending applications.
    let ids = vec![
        a.order_no.clone(),
        "DFB-NOPE".to_string(),
        b.order_no.clone(),
    ];

    let rep = svc_.df_reject_batch(&ids, "", now_ts()).await.unwrap();
    // The two real rows flip (never debited → no refund); the phantom id fails.
    assert_eq!(rep.succeeded_count(), 2);
    assert_eq!(rep.failed_count(), 1);
    assert_eq!(rep.failures[0].0, "DFB-NOPE");
    assert_eq!(rep.failures[0].1, "代付申请不存在");
    assert_eq!(
        balance(&s, user).await,
        300 * K,
        "nothing was debited to refund"
    );
    assert_eq!(
        reload(&s, &a.order_no).await.check_status,
        Some(CheckStatus::Rejected.code())
    );
    assert_eq!(
        reload(&s, &b.order_no).await.reject_reason.as_deref(),
        Some(""),
        "the batch reason defaults to empty (§6.5)"
    );
}
