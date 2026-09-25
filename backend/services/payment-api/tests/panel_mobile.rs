//! DB + Redis coverage for the §10 mobile bind / change surface. The handlers
//! are thin接线, so (per the established service-layer-direct discipline) we
//! drive the pieces they orchestrate — [`mobile::bind_confirm`] /
//! [`mobile::edit_send`] / [`mobile::edit_confirm`] — against a real
//! [`SmsCodes`] kernel and [`AuthLimiter`]:
//!
//! - binding writes ONLY on a correct, unexpired code (a wrong code leaves the
//!   member untouched) and consumes the code on success;
//! - `editMobile` is a strict two-step old→new machine: step one advances the
//!   phase flag with no write, step two writes the new number and clears it;
//! - the send step's empty-target branches (`EmptyOld` / `EmptyNew`) and the
//!   `MerchantSms` lockout (`MAX_AUTH_ERROR_TIMES` misses, then a then-correct
//!   code refused) both hold;
//! - a successful step-one verify clears the failure counter.
//!
//! Harness mirrors `panel_sec_factor.rs`; ids base 60_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use redis::aio::ConnectionManager;
use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::mobile::{self, BindOutcome, EditOutcome, SendOutcome};
use payment_api::merchant::MembersRepo;
use payment_api::ratelimit::{AuthKind, AuthLimiter, MAX_AUTH_ERROR_TIMES};
use payment_api::sms::SmsCodes;

const BASE: i64 = 60_000_000_000_000;
const CALL_BIND: &str = "bindMobile";
const CALL_EDIT: &str = "editMobile";
/// Never producible by [`SmsCodes`] (its codes are 6 DISTINCT digits), so it is
/// a dependable always-wrong guess for the reject / lockout tests.
const WRONG: &str = "000000";

async fn redis() -> Option<ConnectionManager> {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL").ok()?;
    redis::Client::open(url)
        .expect("redis client")
        .get_connection_manager()
        .await
        .ok()
}

/// Seeds a groupid-4 merchant with an optional bound mobile.
async fn seed(s: &Suite, user: i64, mobile: Option<&str>) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("mb{user}")),
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
        mobile: Set(mobile.map(str::to_string)),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn mobile_of(s: &Suite, user: i64) -> Option<String> {
    MembersRepo::new(&s.db)
        .by_id(user)
        .await
        .unwrap()
        .unwrap()
        .mobile
}

// --- binding ----------------------------------------------------------------

#[tokio::test]
async fn bind_send_issues_without_exposing_a_code() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let sms = SmsCodes::new(cm);

    assert_eq!(
        mobile::bind_send(&sms, user, "13800000000").await.unwrap(),
        SendOutcome::Sent
    );
}

#[tokio::test]
async fn bind_confirm_writes_on_a_correct_code() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let sms = SmsCodes::new(cm);

    let code = sms.issue(CALL_BIND, user).await.unwrap();
    let out = mobile::bind_confirm(&s.db, &sms, user, &code, "13800000000")
        .await
        .unwrap();
    assert!(matches!(out, BindOutcome::Saved { status: 1 }), "{out:?}");
    assert_eq!(mobile_of(&s, user).await.as_deref(), Some("13800000000"));
    // the code was consumed — a replay of the same value can no longer verify
    assert!(!sms.verify(CALL_BIND, user, &code).await);
}

#[tokio::test]
async fn bind_confirm_rejects_a_wrong_code_without_writing() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some("111")).await;
    let sms = SmsCodes::new(cm);

    let out = mobile::bind_confirm(&s.db, &sms, user, WRONG, "13800000000")
        .await
        .unwrap();
    assert_eq!(out, BindOutcome::BadCode);
    assert_eq!(mobile_of(&s, user).await.as_deref(), Some("111"));
}

// --- changing (two-step old -> new) -----------------------------------------

#[tokio::test]
async fn edit_runs_the_two_step_machine() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some("111")).await;
    let sms = SmsCodes::new(cm.clone());
    let lim = AuthLimiter::new(cm);
    sms.clear_edit_phase(user).await;

    // Step one: verify the OLD-phone code → phase advances, nothing written.
    let c1 = sms.issue(CALL_EDIT, user).await.unwrap();
    let out1 = mobile::edit_confirm(&s.db, &sms, &lim, user, &c1, "")
        .await
        .unwrap();
    assert_eq!(out1, EditOutcome::OldVerified);
    assert_eq!(mobile_of(&s, user).await.as_deref(), Some("111"));
    assert!(sms.edit_phase(user).await);

    // Step two: verify the NEW-phone code → the number is written, phase cleared.
    let c2 = sms.issue(CALL_EDIT, user).await.unwrap();
    let out2 = mobile::edit_confirm(&s.db, &sms, &lim, user, &c2, "222")
        .await
        .unwrap();
    assert!(matches!(out2, EditOutcome::Saved { status: 1 }), "{out2:?}");
    assert_eq!(mobile_of(&s, user).await.as_deref(), Some("222"));
    assert!(!sms.edit_phase(user).await);
}

#[tokio::test]
async fn edit_send_rejects_a_missing_bound_old_mobile() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let sms = SmsCodes::new(cm);
    sms.clear_edit_phase(user).await;

    assert_eq!(
        mobile::edit_send(&s.db, &sms, user, "").await.unwrap(),
        SendOutcome::EmptyOld
    );
}

#[tokio::test]
async fn edit_send_rejects_a_blank_new_number_in_phase_two() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some("111")).await;
    let sms = SmsCodes::new(cm);
    sms.set_edit_phase(user).await;

    assert_eq!(
        mobile::edit_send(&s.db, &sms, user, "").await.unwrap(),
        SendOutcome::EmptyNew
    );
}

// --- the MerchantSms failure counter ----------------------------------------

#[tokio::test]
async fn misses_trip_the_lockout_gate_even_for_a_valid_code() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some("111")).await;
    let sms = SmsCodes::new(cm.clone());
    let lim = AuthLimiter::new(cm);
    sms.clear_edit_phase(user).await;

    for _ in 0..MAX_AUTH_ERROR_TIMES {
        let out = mobile::edit_confirm(&s.db, &sms, &lim, user, WRONG, "")
            .await
            .unwrap();
        assert_eq!(out, EditOutcome::BadCode);
    }
    assert!(lim.is_locked(AuthKind::MerchantSms, user).await);

    // Once locked, even a freshly issued correct code is refused (no verify,
    // no write, no further increment).
    let good = sms.issue(CALL_EDIT, user).await.unwrap();
    let out = mobile::edit_confirm(&s.db, &sms, &lim, user, &good, "")
        .await
        .unwrap();
    match out {
        EditOutcome::Locked { msg } => assert!(msg.contains("输入错误次数过多"), "got {msg}"),
        other => panic!("expected lockout, got {other:?}"),
    }
    assert_eq!(
        lim.count(AuthKind::MerchantSms, user).await,
        MAX_AUTH_ERROR_TIMES
    );
}

#[tokio::test]
async fn a_correct_code_clears_the_counter() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some("111")).await;
    let sms = SmsCodes::new(cm.clone());
    let lim = AuthLimiter::new(cm);
    sms.clear_edit_phase(user).await;

    // One failure, then a correct step-one verify wipes the counter.
    mobile::edit_confirm(&s.db, &sms, &lim, user, WRONG, "")
        .await
        .unwrap();
    assert_eq!(lim.count(AuthKind::MerchantSms, user).await, 1);

    let code = sms.issue(CALL_EDIT, user).await.unwrap();
    let out = mobile::edit_confirm(&s.db, &sms, &lim, user, &code, "")
        .await
        .unwrap();
    assert_eq!(out, EditOutcome::OldVerified);
    assert_eq!(lim.count(AuthKind::MerchantSms, user).await, 0);
}
