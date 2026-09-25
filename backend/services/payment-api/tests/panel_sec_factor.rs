//! DB + Redis coverage for the §10 second-factor write surface. The handlers
//! are thin接线, so (per the established service-layer-direct discipline) we
//! drive the pieces they orchestrate:
//!
//! - [`twofactor::verify_google`] against a real [`AuthLimiter`]: a valid
//!   RFC 6238 code passes and clears the counter, a wrong code records a
//!   failure, an empty code rejects without recording, and `MAX_AUTH_ERROR_TIMES`
//!   misses trip the lockout gate (even a then-valid code is refused).
//! - the [`twofactor::required_factor`] matrix gating a profile write: a
//!   Google secret forces the factor (wrong code → no DB write, right code →
//!   [`profile::apply_profile`] lands), and a merchant with neither factor
//!   writes straight through.
//! - the bank-card writes are strictly owner-scoped `(id, userid)`, so one
//!   merchant can never default / delete / list another's cards.
//!
//! Harness mirrors `apikey_view.rs`; ids base 40_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use redis::aio::ConnectionManager;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::{bankcards, members};
use payment_api::merchant::bankcard::{self, BankcardForm};
use payment_api::merchant::twofactor::{self, Factor, FactorGate};
use payment_api::merchant::{profile, MembersRepo};
use payment_api::ratelimit::{AuthKind, AuthLimiter, MAX_AUTH_ERROR_TIMES};
use payment_api::totp;

const BASE: i64 = 40_000_000_000_000;
/// A fixed base32 secret (the RFC 6238 SHA1 test key) reused as a merchant's
/// `google_secret_key`, so codes are reproducible against [`NOW`].
const SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
/// A pinned unix second the whole factor path is evaluated at.
const NOW: i64 = 1_700_000_000;

async fn redis() -> Option<ConnectionManager> {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL").ok()?;
    redis::Client::open(url)
        .expect("redis client")
        .get_connection_manager()
        .await
        .ok()
}

/// Seeds a groupid-4 merchant with an optional Google secret.
async fn seed_member(s: &Suite, user: i64, google: Option<&str>) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("sf{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        df_auto_check: Set(0),
        google_secret_key: Set(google.map(str::to_string)),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn member(s: &Suite, user: i64) -> members::Model {
    MembersRepo::new(&s.db).by_id(user).await.unwrap().unwrap()
}

fn post<'a>(pairs: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, String)> {
    pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
}

/// A valid current code (inside the ±2 window).
fn valid_code() -> String {
    totp::totp_code(SECRET, NOW).unwrap()
}

/// A well-formed 6-digit code guaranteed to be OUTSIDE the accept window (a
/// thousand steps in the past), so it must be rejected as "谷歌安全码错误！".
fn stale_code() -> String {
    totp::totp_code(SECRET, NOW - 1_000 * totp::TOTP_STEP).unwrap()
}

fn form<'a>(bank: &'a str, number: &'a str) -> BankcardForm<'a> {
    BankcardForm {
        bankname: bank,
        subbranch: "某支行",
        accountname: "持卡人",
        cardnumber: number,
        province: "广东",
        city: "深圳",
        alias: "",
    }
}

// --- the Google factor against a real limiter -------------------------------

#[tokio::test]
async fn a_valid_code_passes_and_clears_the_counter() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, Some(SECRET)).await;
    let lim = AuthLimiter::new(cm);
    // A prior failure must be wiped by the successful verify.
    lim.record_fail(AuthKind::GoogleTotp, user).await;

    let gate = twofactor::verify_google(&lim, user, SECRET, &valid_code(), NOW).await;
    assert_eq!(gate, FactorGate::Passed);
    assert_eq!(lim.count(AuthKind::GoogleTotp, user).await, 0);
}

#[tokio::test]
async fn a_wrong_code_records_a_failure() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, Some(SECRET)).await;
    let lim = AuthLimiter::new(cm);

    let gate = twofactor::verify_google(&lim, user, SECRET, &stale_code(), NOW).await;
    assert_eq!(
        gate,
        FactorGate::Rejected {
            msg: twofactor::MSG_GOOGLE_CODE_WRONG.to_string()
        }
    );
    assert_eq!(lim.count(AuthKind::GoogleTotp, user).await, 1);
}

#[tokio::test]
async fn an_empty_code_rejects_without_recording() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, Some(SECRET)).await;
    let lim = AuthLimiter::new(cm);

    let gate = twofactor::verify_google(&lim, user, SECRET, "", NOW).await;
    assert_eq!(
        gate,
        FactorGate::Rejected {
            msg: twofactor::MSG_GOOGLE_CODE_EMPTY.to_string()
        }
    );
    assert_eq!(lim.count(AuthKind::GoogleTotp, user).await, 0);
}

#[tokio::test]
async fn misses_trip_the_lockout_gate_even_for_a_valid_code() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, Some(SECRET)).await;
    let lim = AuthLimiter::new(cm);

    for _ in 0..MAX_AUTH_ERROR_TIMES {
        let gate = twofactor::verify_google(&lim, user, SECRET, &stale_code(), NOW).await;
        assert!(matches!(gate, FactorGate::Rejected { .. }));
    }
    assert!(lim.is_locked(AuthKind::GoogleTotp, user).await);

    // Once locked, even the correct code is refused with the lockout message
    // (no verify, no clear, no further increment).
    let gate = twofactor::verify_google(&lim, user, SECRET, &valid_code(), NOW).await;
    match gate {
        FactorGate::Rejected { msg } => assert!(msg.contains("输入错误次数过多"), "got {msg}"),
        FactorGate::Passed => panic!("expected lockout even with a valid code"),
    }
    assert_eq!(
        lim.count(AuthKind::GoogleTotp, user).await,
        MAX_AUTH_ERROR_TIMES
    );
}

// --- the factor matrix gating a profile write -------------------------------

#[tokio::test]
async fn a_google_secret_gates_the_profile_write() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, Some(SECRET)).await;
    let lim = AuthLimiter::new(cm);

    let has_google = !member(&s, user)
        .await
        .google_secret_key
        .unwrap_or_default()
        .is_empty();
    let factor = twofactor::required_factor(has_google, twofactor::sms_status(), 1).unwrap();
    assert_eq!(factor, Factor::Google);

    let update = || profile::plan_profile(&post(&[("realname", "张三")]), |_| true).unwrap();

    // A wrong code rejects BEFORE any write — the member stays untouched.
    match twofactor::verify_google(&lim, user, SECRET, &stale_code(), NOW).await {
        FactorGate::Rejected { .. } => {}
        FactorGate::Passed => panic!("stale code must not pass"),
    }
    assert!(member(&s, user).await.realname.is_none());

    // The correct code passes, so the planned profile write lands.
    match twofactor::verify_google(&lim, user, SECRET, &valid_code(), NOW).await {
        FactorGate::Passed => {
            profile::apply_profile(&s.db, user, &update())
                .await
                .unwrap();
        }
        FactorGate::Rejected { msg } => panic!("valid code rejected: {msg}"),
    }
    assert_eq!(member(&s, user).await.realname.as_deref(), Some("张三"));
}

#[tokio::test]
async fn a_merchant_with_no_factor_writes_straight_through() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, None).await;

    // No Google secret, `sms_status()` false → `Factor::None`, no verify.
    let factor = twofactor::required_factor(false, twofactor::sms_status(), 0).unwrap();
    assert_eq!(factor, Factor::None);

    let u = profile::plan_profile(&post(&[("mobile", "13800000000")]), |_| true).unwrap();
    profile::apply_profile(&s.db, user, &u).await.unwrap();
    assert_eq!(
        member(&s, user).await.mobile.as_deref(),
        Some("13800000000")
    );
}

// --- bank-card write ownership ----------------------------------------------

#[tokio::test]
async fn bank_card_writes_are_owner_scoped() {
    let Some(s) = suite().await else { return };
    let owner = uid(BASE);
    let other = uid(BASE);
    seed_member(&s, owner, None).await;
    seed_member(&s, other, None).await;

    // The owner saves a card (sms off → no factor gates the write).
    assert_eq!(
        bankcard::upsert_card(&s.db, None, owner, &form("ICBC", "6222"), 100)
            .await
            .unwrap(),
        1
    );
    let card = bankcard::list_for_user(&s.db, owner)
        .await
        .unwrap()
        .first()
        .unwrap()
        .id;

    // Another merchant can neither default, delete, nor even see it.
    assert_eq!(
        bankcard::set_default(&s.db, card, other, 1, 200)
            .await
            .unwrap(),
        0
    );
    assert!(bankcard::list_for_user(&s.db, other)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(bankcard::delete_card(&s.db, card, other).await.unwrap(), 0);

    // The owner still holds the card, and the default flag is intact.
    assert_eq!(
        bankcards::Entity::find_by_id(card)
            .one(&*s.db)
            .await
            .unwrap()
            .unwrap()
            .isdefault,
        0
    );
    assert_eq!(bankcard::delete_card(&s.db, card, owner).await.unwrap(), 1);
    assert!(bankcard::list_for_user(&s.db, owner)
        .await
        .unwrap()
        .is_empty());
}
