//! DB-gated coverage for the §10 merchant password-change surface
//! ([`password::change_pay_password`] / [`password::change_login_password`]).
//! The handlers are thin接线 (session + portal gate, then these services), so
//! we drive the verify-then-write directly against a real member row: the
//! wrong-old / confirm-mismatch rejects, the successful persist (reloading the
//! stored hash), and the faithful new-equals-old branches — login gets the
//! `请勿使用旧密码` message, pay collapses to a bare `{status:0}` (no `msg`),
//! both modeled by explicit hash comparison rather than `rows_affected`.
//!
//! The pure evaluation is pinned offline in the module; the SMS factor is the
//! `sms_status() == false` lazy seam, so nothing gates the write here. Harness
//! mirrors `apikey_view.rs`; ids base 50_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::password::{self, PwdOutcome};
use payment_api::merchant::MembersRepo;

const BASE: i64 = 50_000_000_000_000;
const SALT: &str = "1234";
const OLD_LOGIN: &str = "oldlogin";
const OLD_PAY: &str = "123456";

/// Seeds a groupid-4 merchant with a known salted login password and an
/// unsalted payment password.
async fn seed(s: &Suite, user: i64) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("pw{user}")),
        password: Set(password::hash_password(OLD_LOGIN, SALT)),
        salt: Set(SALT.to_string()),
        groupid: Set(4),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        df_auto_check: Set(0),
        pay_password: Set(Some(password::hash_pay_password(OLD_PAY))),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn member(s: &Suite, user: i64) -> members::Model {
    MembersRepo::new(&s.db).by_id(user).await.unwrap().unwrap()
}

// --- payment password -------------------------------------------------------

#[tokio::test]
async fn pay_password_changes_with_the_correct_old() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    let out = password::change_pay_password(&s.db, user, OLD_PAY, "newpay", "newpay")
        .await
        .unwrap();
    assert_eq!(out, PwdOutcome::Success { msg: None });
    assert_eq!(
        member(&s, user).await.pay_password.as_deref(),
        Some(password::hash_pay_password("newpay").as_str())
    );
}

#[tokio::test]
async fn pay_password_rejects_a_wrong_old_without_writing() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    let out = password::change_pay_password(&s.db, user, "999999", "newpay", "newpay")
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Rejected {
            status: 0,
            msg: Some(password::MSG_INPUT_ERR.to_string())
        }
    );
    // unchanged
    assert_eq!(
        member(&s, user).await.pay_password.as_deref(),
        Some(password::hash_pay_password(OLD_PAY).as_str())
    );
}

#[tokio::test]
async fn pay_password_rejects_a_confirm_mismatch() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    let out = password::change_pay_password(&s.db, user, OLD_PAY, "newpay", "diff")
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Rejected {
            status: 0,
            msg: Some(password::MSG_INPUT_ERR.to_string())
        }
    );
}

#[tokio::test]
async fn pay_password_reuse_is_a_bare_status_zero() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    // new == old → {status:0} with NO message (legacy `{status: $res}`, 0 rows).
    let out = password::change_pay_password(&s.db, user, OLD_PAY, OLD_PAY, OLD_PAY)
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Rejected {
            status: 0,
            msg: None
        }
    );
}

// --- login password ---------------------------------------------------------

#[tokio::test]
async fn login_password_changes_and_hashes_with_the_salt() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    let out = password::change_login_password(&s.db, user, OLD_LOGIN, "newlogin", "newlogin")
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Success {
            msg: Some(password::MSG_LOGIN_OK.to_string())
        }
    );
    assert_eq!(
        member(&s, user).await.password,
        password::hash_password("newlogin", SALT)
    );
}

#[tokio::test]
async fn login_password_rejects_a_wrong_old() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    let out = password::change_login_password(&s.db, user, "nope", "newlogin", "newlogin")
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Rejected {
            status: 0,
            msg: Some(password::MSG_INPUT_ERR.to_string())
        }
    );
    assert_eq!(
        member(&s, user).await.password,
        password::hash_password(OLD_LOGIN, SALT)
    );
}

#[tokio::test]
async fn login_password_reuse_carries_the_dedicated_message() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user).await;

    let out = password::change_login_password(&s.db, user, OLD_LOGIN, OLD_LOGIN, OLD_LOGIN)
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Rejected {
            status: 0,
            msg: Some(password::MSG_REUSE_OLD.to_string())
        }
    );
}

#[tokio::test]
async fn a_missing_member_folds_to_the_input_error() {
    let Some(s) = suite().await else { return };
    let missing = uid(BASE);

    let out = password::change_login_password(&s.db, missing, OLD_LOGIN, "x", "x")
        .await
        .unwrap();
    assert_eq!(
        out,
        PwdOutcome::Rejected {
            status: 0,
            msg: Some(password::MSG_INPUT_ERR.to_string())
        }
    );
}

// --- per-merchant scoping ---------------------------------------------------

#[tokio::test]
async fn a_change_only_touches_the_targeted_member() {
    let Some(s) = suite().await else { return };
    let a = uid(BASE);
    let b = uid(BASE);
    seed(&s, a).await;
    seed(&s, b).await;

    password::change_pay_password(&s.db, a, OLD_PAY, "newpay", "newpay")
        .await
        .unwrap();

    // b is untouched.
    assert_eq!(
        member(&s, b).await.pay_password.as_deref(),
        Some(password::hash_pay_password(OLD_PAY).as_str())
    );
}
