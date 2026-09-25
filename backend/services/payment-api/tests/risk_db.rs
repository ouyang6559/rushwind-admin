//! DB+Redis gated integration for the post-settle risk observation
//! (`spec/06` §5.1): real ledger settles, real Redis buckets receive the
//! three legs, and the day-cap trip raises the offline marker.
//!
//! The day bucket keys on the UTC `Ymd` of the injected clock, so each run
//! picks a process-unique pseudo-day (offset far beyond today) and never
//! collifies with the real cron or re-runs of itself.

#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use payment_api::data::{channel_accounts, channels};
use payment_api::ledger::NewOrder;
use payment_api::rate::ResolvedRate;
use payment_api::risk::counters::{unit_bucket, ymd};
use payment_api::risk::{observe, RiskGate, Scope};
use redis::aio::ConnectionManager;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DatabaseConnection};

mod common;
use common::{uid, Suite};

const BASE: i64 = 11_100_000_000_000;
const AMOUNT: i64 = 1_000_000; // 100 元
                               // harness T+0 feilv 8_000/1e6 = 0.8% → poundage 8_000,
const ACTUAL: i64 = 992_000; // settled net the merchant leg feeds

/// A process-unique pseudo-day far past the real calendar, so the test's
/// `Ymd` buckets can never meet the production cron's (or its own reruns').
fn pseudo_now() -> i64 {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (36_500_000 + seed % 30_000_000 + (std::process::id() as i64) * 7) * 86_400 + 3_600
}

async fn redis() -> ConnectionManager {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://:*Abcd123456@127.0.0.1:6379/".to_string());
    redis::Client::open(url)
        .expect("redis url")
        .get_connection_manager()
        .await
        .expect("redis reachable")
}

async fn get_i64(redis: &mut ConnectionManager, key: &str) -> Option<i64> {
    redis::cmd("GET")
        .arg(key)
        .query_async(redis)
        .await
        .expect("redis get")
}

async fn seed_channel(
    db: &DatabaseConnection,
    id: i64,
    control: i32,
    offline: i32,
    all_money: i64,
) {
    channels::ActiveModel {
        id: Set(id),
        code: Set(format!("RK{id}")),
        title: Set("risk-test".into()),
        default_rate: Set(6_000),
        fengding: Set(0),
        t0_default_rate: Set(8_000),
        t0_fengding: Set(0),
        status: Set(1),
        paytype: Set(1),
        control_status: Set(control),
        offline_status: Set(offline),
        all_money: Set(all_money),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn seed_account(
    db: &DatabaseConnection,
    id: i64,
    is_defined: i32,
    control: i32,
    offline: i32,
    all_money: i64,
    unit_interval: i32,
    time_unit: &str,
) {
    channel_accounts::ActiveModel {
        id: Set(id),
        channel_id: Set(id),
        weight: Set(1),
        status: Set(1),
        default_rate: Set(6_000),
        fengding: Set(0),
        t0_default_rate: Set(8_000),
        t0_fengding: Set(0),
        custom_rate: Set(0),
        control_status: Set(control),
        offline_status: Set(offline),
        is_defined: Set(is_defined),
        all_money: Set(all_money),
        unit_interval: Set(unit_interval),
        time_unit: Set(time_unit.into()),
        unit_number: Set(0),
        unit_all_money: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

/// One full create → settle → observe round on the given world; returns the
/// observation report for the settled order.
async fn settle_and_observe(
    s: &Suite,
    gate: &RiskGate,
    user: i64,
    channel: i64,
    account: i64,
    order_id: &str,
    now_ts: i64,
) -> observe::Report {
    let new = NewOrder {
        user_id: user,
        order_id: order_id.into(),
        amount_units: AMOUNT,
        rate: ResolvedRate {
            feilv: 8_000,
            fengding: 0,
        },
        cost_rate: 8_000,
        t: 0,
        bank_code: "903".into(),
        channel_code: "TEST".into(),
        notify_url: String::new(),
        callback_url: String::new(),
        channel_id: channel,
        account_id: account,
        sign_key: None,
        app_id: None,
        attach: None,
        product_name: None,
    };
    s.ledger.create_order(&new).await.unwrap();
    let outcome = s.ledger.settle_order(order_id).await.unwrap();
    assert!(matches!(
        outcome,
        payment_api::ledger::SettleOutcome::Settled(_)
    ));
    let order = s.ledger.find_order(order_id).await.unwrap().unwrap();
    observe::observe_settlement(&s.db, gate, &order, now_ts)
        .await
        .unwrap()
}

#[tokio::test]
async fn three_legs_accumulate_the_trade() {
    let Some(s) = common::suite().await else {
        return;
    };
    let mut redis = redis().await;
    let now = pseudo_now();
    let ymd = ymd(now);

    let user = common::uid(BASE);
    let ch = common::uid(BASE);
    let ac = common::uid(BASE);
    // Not the seed_world channel — this test needs risk config on it.
    members_seed(&s, user).await;
    seed_channel(&s.db, ch, 1, 1, 100 * AMOUNT).await;
    seed_account(&s.db, ac, 1, 1, 1, 100 * AMOUNT, 5, "i").await;

    let gate = RiskGate::new(redis.clone());
    let r = settle_and_observe(&s, &gate, user, ch, ac, &format!("RKA{}", uid(BASE)), now).await;
    assert_eq!(
        r,
        observe::Report {
            channel_counted: true,
            channel_tripped: false,
            account_counted: true,
            account_tripped: false,
            account_unit_counted: true,
            merchant_counted: true,
            merchant_unit_counted: false,
        }
    );

    // Day buckets: channel + account feed the FACE amount, the merchant the
    // settled net (the legacy's exact money bases).
    assert_eq!(
        get_i64(&mut redis, &format!("risk:daily:channel:{ch}:{ymd}")).await,
        Some(AMOUNT)
    );
    assert_eq!(
        get_i64(&mut redis, &format!("risk:daily:account:{ac}:{ymd}")).await,
        Some(AMOUNT)
    );
    assert_eq!(
        get_i64(&mut redis, &format!("risk:daily:member:{user}:{ymd}")).await,
        Some(ACTUAL)
    );
    // The account's 5-minute unit window took count +1 and the NET amount.
    let bucket = unit_bucket(now, 300);
    assert_eq!(
        get_i64(&mut redis, &format!("risk:unit:account:{ac}:c:{bucket}")).await,
        Some(1)
    );
    assert_eq!(
        get_i64(&mut redis, &format!("risk:unit:account:{ac}:a:{bucket}")).await,
        Some(ACTUAL)
    );
    // Nothing near a cap → nobody offline.
    assert!(!gate.is_offline(Scope::Channel, ch).await);
}

#[tokio::test]
async fn day_cap_trip_puts_the_channel_offline() {
    let Some(s) = common::suite().await else {
        return;
    };
    let mut redis = redis().await;
    let now = pseudo_now();

    let user = common::uid(BASE);
    let ch = common::uid(BASE);
    let ac = common::uid(BASE);
    members_seed(&s, user).await;
    // Channel cap = exactly one order: the first settle already reaches >=.
    seed_channel(&s.db, ch, 1, 1, AMOUNT).await;
    seed_account(&s.db, ac, 1, 1, 1, 100 * AMOUNT, 0, "s").await;

    let gate = RiskGate::new(redis.clone());
    let r = settle_and_observe(&s, &gate, user, ch, ac, &format!("RK{}", uid(BASE)), now).await;
    assert!(r.channel_tripped, "total == cap trips (>=, P:479)");
    assert!(!r.account_tripped);
    assert!(!r.account_unit_counted, "interval 0 = unit leg off");
    assert!(gate.is_offline(Scope::Channel, ch).await);
    // A second trade on the same day double-accumulates but stays marked;
    // the screening side reads the marker, not this counter.
    let r2 = settle_and_observe(&s, &gate, user, ch, ac, &format!("RK2-{}", uid(BASE)), now).await;
    assert!(r2.channel_tripped);
    assert_eq!(
        get_i64(&mut redis, &format!("risk:daily:channel:{ch}:{}", ymd(now))).await,
        Some(2 * AMOUNT)
    );
}

#[tokio::test]
async fn unconfigured_subjects_only_feed_the_merchant_leg() {
    let Some(s) = common::suite().await else {
        return;
    };
    let mut redis = redis().await;
    let now = pseudo_now();

    let user = common::uid(BASE);
    let ch = common::uid(BASE);
    let ac = common::uid(BASE);
    members_seed(&s, user).await;
    // control off + cap 0: saveOfflineStatus admits nothing, not even the
    // accumulation; the account inherits the (dead) channel config.
    seed_channel(&s.db, ch, 0, 1, 0).await;
    seed_account(&s.db, ac, 0, 1, 1, 99 * AMOUNT, 5, "i").await;

    let gate = RiskGate::new(redis.clone());
    let r = settle_and_observe(&s, &gate, user, ch, ac, &format!("RK3-{}", uid(BASE)), now).await;
    assert!(!r.channel_counted);
    assert!(
        !r.account_counted,
        "is_defined=0 inherits the channel's cap-0 config"
    );
    assert!(
        !r.account_unit_counted,
        "inheriting accounts never carry a unit"
    );
    assert!(r.merchant_counted, "the merchant leg is unconditional");
    assert_eq!(
        get_i64(
            &mut redis,
            &format!("risk:daily:member:{user}:{}", ymd(now))
        )
        .await,
        Some(ACTUAL)
    );
    assert_eq!(
        get_i64(&mut redis, &format!("risk:daily:channel:{ch}:{}", ymd(now))).await,
        None,
        "a skipped leg must not even accumulate"
    );
}

/// The member row only (seed_world's channel is not the risk world here).
async fn members_seed(s: &Suite, user: i64) {
    use payment_api::data::members;
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("n{user}")),
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
}

#[allow(clippy::too_many_arguments)]
async fn seed_urc(
    db: &DatabaseConnection,
    id: i64,
    user_id: i64,
    all_money: i64,
    unit_interval: i32,
    time_unit: &str,
    domain: &str,
    is_system: i32,
    status: i32,
    system_xz: i32,
) {
    use payment_api::data::user_riskcontrol_configs;
    user_riskcontrol_configs::ActiveModel {
        id: Set(id),
        user_id: Set(user_id),
        min_money: Set(0),
        max_money: Set(0),
        all_money: Set(all_money),
        start_time: Set(0),
        end_time: Set(0),
        unit_interval: Set(unit_interval),
        time_unit: Set(time_unit.into()),
        unit_number: Set(0),
        unit_all_money: Set(0),
        is_system: Set(is_system),
        status: Set(status),
        domain: Set(domain.into()),
        system_xz: Set(system_xz),
    }
    .insert(db)
    .await
    .unwrap();
}

/// The served URC row feeds the merchant unit leg at settle time and then
/// screens the NEXT order off the accumulated day total; the domain list
/// answers before the total (`spec/06` §4.3 chain).
#[tokio::test]
async fn merchant_rule_row_feeds_the_unit_leg_and_screens() {
    let Some(s) = common::suite().await else {
        return;
    };
    let mut redis = redis().await;
    let now = pseudo_now();

    let user = common::uid(BASE);
    let ch = common::uid(BASE);
    let ac = common::uid(BASE);
    members_seed(&s, user).await;
    seed_channel(&s.db, ch, 1, 1, 100 * AMOUNT).await;
    seed_account(&s.db, ac, 1, 0, 1, 0, 0, "s").await;
    // Day cap = exactly one settled net; a 1-hour unit window; domain list.
    let cfg_id = common::uid(BASE);
    seed_urc(
        &s.db,
        cfg_id,
        user,
        ACTUAL,
        1,
        "h",
        "ok.example\r\nb.example",
        0,
        1,
        1,
    )
    .await;

    let gate = RiskGate::new(redis.clone());
    let r = settle_and_observe(&s, &gate, user, ch, ac, &format!("RK4-{}", uid(BASE)), now).await;
    assert!(r.merchant_unit_counted, "the rule row lights the unit leg");
    let bucket = unit_bucket(now, 3600);
    assert_eq!(
        get_i64(&mut redis, &format!("risk:unit:member:{user}:c:{bucket}")).await,
        Some(1)
    );
    assert_eq!(
        get_i64(&mut redis, &format!("risk:unit:member:{user}:a:{bucket}")).await,
        Some(ACTUAL)
    );

    // The day bucket now holds exactly the cap → the next attempt is out.
    let cfg = payment_api::risk::config::load_merchant_config(&s.db, user)
        .await
        .unwrap()
        .expect("own systemxz=1 row is served");
    let d = payment_api::risk::config::merchant_decision(
        &gate,
        &cfg,
        Some("https://ok.example/checkout"),
        ACTUAL,
        now,
    )
    .await;
    assert_eq!(
        d.rule_kind(),
        Some(payment_api::risk::RuleKind::TheTotalVolume),
        "accum {ACTUAL} + attempt > cap {ACTUAL}"
    );
    // A foreign (or missing) referer trips the domain leg BEFORE the total.
    for referer in [Some("https://evil.example/"), None] {
        let d = payment_api::risk::config::merchant_decision(&gate, &cfg, referer, 1, now).await;
        assert_eq!(d.message().as_deref(), Some("请求域名错误！"));
    }
}

/// findConfigInfo: the own row only counts while enabled AND `systemxz = 1`,
/// otherwise the enabled platform row serves, and no row at all = no gate.
#[tokio::test]
async fn merchant_config_row_selection() {
    let Some(s) = common::suite().await else {
        return;
    };
    use payment_api::risk::config::load_merchant_config;

    // Re-runs must not stack platform rows (`.one()` would pick at random).
    s.db.execute_unprepared("DELETE FROM user_riskcontrol_configs WHERE is_system = 1")
        .await
        .unwrap();

    let plat = common::uid(BASE);
    seed_urc(&s.db, plat, 0, 5_000, 0, "i", "", 1, 1, 0).await;

    let ua = common::uid(BASE); // own row not user-defined → platform
    seed_urc(&s.db, common::uid(BASE), ua, 9_999, 0, "i", "", 0, 1, 0).await;
    let ub = common::uid(BASE); // own user-defined row wins
    let own_b = common::uid(BASE);
    seed_urc(&s.db, own_b, ub, 7_777, 0, "i", "", 0, 1, 1).await;
    let uc = common::uid(BASE); // disabled own row → platform
    seed_urc(&s.db, common::uid(BASE), uc, 6_666, 0, "i", "", 0, 0, 1).await;
    let ud = common::uid(BASE); // no row at all → platform

    assert_eq!(
        load_merchant_config(&s.db, ua).await.unwrap().map(|c| c.id),
        Some(plat)
    );
    assert_eq!(
        load_merchant_config(&s.db, ub).await.unwrap().map(|c| c.id),
        Some(own_b)
    );
    assert_eq!(
        load_merchant_config(&s.db, uc).await.unwrap().map(|c| c.id),
        Some(plat)
    );
    assert_eq!(
        load_merchant_config(&s.db, ud).await.unwrap().map(|c| c.id),
        Some(plat)
    );

    // No enabled platform row → the merchant runs without any gate.
    s.db.execute_unprepared("DELETE FROM user_riskcontrol_configs WHERE is_system = 1")
        .await
        .unwrap();
    assert!(load_merchant_config(&s.db, ud).await.unwrap().is_none());
}
