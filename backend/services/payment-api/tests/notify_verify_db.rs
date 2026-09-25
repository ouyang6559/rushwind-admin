//! DB-gated upstream-notify verification tests (`spec/03` §5, the Phase-3b
//! gate): the adapter-extracted order id, the frozen-key signature check, the
//! channel-code match and the success/failure verdict split. An unsigned (or
//! badly signed, or foreign-coded) POST must never reach the settle — the
//! pre-wiring behavior credited a merchant off a bare `pay_orderid` POST.
//! Runs only with `PAYMENT_TEST_DATABASE_URL` set; base 9.6e12 keeps ids
//! disjoint from the other binaries.

mod common;

use std::collections::BTreeMap;

use sea_orm::EntityTrait;

use common::{suite, uid};
use payment_api::channel::sign::easy_pay_sign;
use payment_api::channel::ChannelRegistry;
use payment_api::gateway::verify::{check_notify, NotifyCheck};
use payment_api::ledger::SettleOutcome;

const BASE: i64 = 9_600_000_000_000;
/// The frozen upstream signing key stored on the order (the `orderadd` `key`).
const KEY: &str = "notifyverifykey0000000000000";

/// A WxSm member + live channel + an UNPAID order carrying the frozen key.
async fn seed_unpaid(s: &common::Suite, order_id: &str) -> (i64, i64) {
    use payment_api::data::{channels, members};
    use sea_orm::{ActiveModelTrait, Set};

    let (user, channel) = (uid(BASE), uid(BASE));
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("nv{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        apikey: Set(Some("32charapikey000000000000000000aa".into())),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    channels::ActiveModel {
        id: Set(channel),
        code: Set("WxSm".into()),
        title: Set("t".into()),
        default_rate: Set(6_000),
        fengding: Set(0),
        t0_default_rate: Set(8_000),
        t0_fengding: Set(0),
        status: Set(1),
        paytype: Set(1),
        control_status: Set(0),
        offline_status: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();

    let new = payment_api::ledger::NewOrder {
        user_id: user,
        order_id: order_id.into(),
        amount_units: 1_000_000, // 100元
        rate: payment_api::rate::ResolvedRate {
            feilv: 8_000,
            fengding: 0,
        },
        cost_rate: 8_000,
        t: 0,
        bank_code: "903".into(),
        channel_code: "WxSm".into(),
        notify_url: "http://merchant/notify".into(),
        callback_url: "http://merchant/back".into(),
        channel_id: channel,
        account_id: 1,
        sign_key: Some(KEY.into()),
        app_id: None,
        attach: None,
        product_name: None,
    };
    s.ledger.create_order(&new).await.unwrap();
    (user, channel)
}

/// The upstream's well-formed WxSm notify form, signed with `key` over the
/// sorted non-empty subset (skip sign/sign_type).
fn signed_form(order_id: &str, trade_status: &str, key: &str) -> BTreeMap<String, String> {
    let mut form = BTreeMap::new();
    form.insert("pid".to_string(), "858580".to_string());
    form.insert("trade_no".to_string(), "UP123".to_string());
    form.insert("out_trade_no".to_string(), order_id.to_string());
    form.insert("type".to_string(), "wxpay".to_string());
    form.insert("money".to_string(), "100.00".to_string());
    form.insert("trade_status".to_string(), trade_status.to_string());
    form.insert("sign_type".to_string(), "MD5".to_string());
    let sign = easy_pay_sign(&form, key);
    form.insert("sign".to_string(), sign);
    form
}

#[tokio::test]
async fn verified_success_proceeds_and_settles() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let registry = ChannelRegistry::assemble(["WxSm"]);
    let oid = format!("NV{}", uid(BASE));
    let (user, _channel) = seed_unpaid(&s, &oid).await;

    let verdict = check_notify(
        &s.db,
        &registry,
        "WxSm",
        &signed_form(&oid, "TRADE_SUCCESS", KEY),
    )
    .await;
    let ack = match verdict {
        NotifyCheck::Proceed { order_id, ack } => {
            assert_eq!(order_id, oid);
            ack
        }
        other => panic!("expected Proceed, got {other:?}"),
    };
    assert_eq!(ack, "success"); // the adapter's ack rides to the upstream

    let outcome = s.ledger.settle_order(&oid).await.unwrap();
    // The order closed and the merchant holds exactly the planned net
    // arrival, credited available (t = 0). The absolute figure tolerates
    // whatever complaints-deposit rule the shared test DB currently carries
    // (the deposit_db binary seeds global rules): net + deposit == arrival.
    let (net, deposit) = match &outcome {
        SettleOutcome::Settled(w) => (
            w.merchant_flow.money,
            w.deposit.as_ref().map(|d| d.amount).unwrap_or(0),
        ),
        other => panic!("expected Settled, got {other:?}"),
    };
    assert_eq!(net + deposit, 992_000);
    let order = common::order_row(&s.db, &oid).await;
    assert_eq!(order.status, 1);
    let member = payment_api::data::members::Entity::find_by_id(user)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(member.balance, net);
}

#[tokio::test]
async fn unsigned_and_wrong_key_posts_are_rejected() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let registry = ChannelRegistry::assemble(["WxSm"]);
    let oid = format!("NV{}", uid(BASE));
    seed_unpaid(&s, &oid).await;

    // The pre-wiring hole itself: a bare pay_orderid-style POST with no
    // signature can no longer settle — reject before any money moves.
    let mut unsigned = signed_form(&oid, "TRADE_SUCCESS", KEY);
    unsigned.remove("sign");
    assert_eq!(
        check_notify(&s.db, &registry, "WxSm", &unsigned).await,
        NotifyCheck::Reject
    );

    // Signed with the wrong secret: reject.
    let forged = signed_form(&oid, "TRADE_SUCCESS", "another-key");
    assert_eq!(
        check_notify(&s.db, &registry, "WxSm", &forged).await,
        NotifyCheck::Reject
    );

    // Signed with the right key but naming a non-existent order: reject.
    let ghost = signed_form("NV-no-such-order", "TRADE_SUCCESS", KEY);
    assert_eq!(
        check_notify(&s.db, &registry, "WxSm", &ghost).await,
        NotifyCheck::Reject
    );

    let order = common::order_row(&s.db, &oid).await;
    assert_eq!(order.status, 0, "nothing settled");
}

#[tokio::test]
async fn foreign_code_and_failed_trade_never_reach_the_settle() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let registry = ChannelRegistry::assemble(["WxSm", "Rzfkj"]);
    let oid = format!("NV{}", uid(BASE));
    seed_unpaid(&s, &oid).await;

    // A correctly-signed WxSm message replayed under ANOTHER channel code:
    // the code must name the order's own channel.
    let form = signed_form(&oid, "TRADE_SUCCESS", KEY);
    assert_eq!(
        check_notify(&s.db, &registry, "Rzfkj", &form).await,
        NotifyCheck::Reject
    );

    // A verified message reporting a FAILED trade: acknowledged, no settle.
    let failed = signed_form(&oid, "WAIT_BUYER_PAY", KEY);
    match check_notify(&s.db, &registry, "WxSm", &failed).await {
        NotifyCheck::AckOnly(ack) => assert_eq!(ack, "fail"),
        other => panic!("expected AckOnly, got {other:?}"),
    }

    // An unconfigured code rejects even a well-signed message.
    let unconfigured = signed_form(&oid, "TRADE_SUCCESS", KEY);
    let empty: Vec<&str> = Vec::new();
    let empty_registry = ChannelRegistry::assemble(empty);
    assert_eq!(
        check_notify(&s.db, &empty_registry, "WxSm", &unconfigured).await,
        NotifyCheck::Reject
    );

    let order = common::order_row(&s.db, &oid).await;
    assert_eq!(order.status, 0, "nothing settled");
}
