//! DB coverage for the §5找回密码 email-code flow. The code generator / body are
//! offline in `forgetpwd.rs`; here we drive the two DB legs against a real
//! Postgres (`pay_user_code` added by `m20260924_000008_user_codes`), using the
//! [`NoopEmailProvider`] seam (never fails, so delivery is a logged trace):
//!
//! - [`send_user_code`] on a known account files a live (`status = 0`, unexpired)
//!   code; on an unknown username/email it files nothing;
//! - [`reset_password`] with the stored code rewrites `md5(new . salt)` and
//!   consumes the row (`status = 1` + `uptime`);
//! - a wrong code and an expired code both fail without touching the password.
//!
//! Harness mirrors `panel_console.rs`; ids base 130_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
};

use common::{suite, uid, Suite};
use payment_api::data::{members, user_codes};
use payment_api::merchant::{forgetpwd, password};

const BASE: i64 = 130_000_000_000_000;

async fn seed_member(s: &Suite, user: i64, username: &str, email: &str, salt: &str) {
    members::ActiveModel {
        id: Set(user),
        username: Set(username.into()),
        password: Set(password::hash_password("oldpass", salt)),
        salt: Set(salt.into()),
        email: Set(Some(email.into())),
        groupid: Set(4),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        df_auto_check: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// Newest live code row for an account (the one `send_user_code` just filed).
async fn latest_code(s: &Suite, username: &str) -> user_codes::Model {
    user_codes::Entity::find()
        .filter(user_codes::Column::Username.eq(username))
        .order_by_desc(user_codes::Column::Id)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn send_code_then_reset_with_stored_code() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let uname = format!("fp{user}");
    let email = format!("{user}@example.com");
    let salt = "1234";
    seed_member(&s, user, &uname, &email, salt).await;

    let out = forgetpwd::send_user_code(
        &s.db,
        &forgetpwd::NoopEmailProvider,
        &uname,
        &email,
        payment_api::data::now_ts(),
    )
    .await
    .unwrap();
    assert_eq!(out, forgetpwd::SendOutcome::Sent);
    let filed = latest_code(&s, &uname).await;
    assert_eq!(filed.status, 0);
    assert!(filed.endtime.unwrap() > payment_api::data::now_ts());

    // reset with the stored code
    let code = filed.code.clone().unwrap();
    let out = forgetpwd::reset_password(
        &s.db,
        &uname,
        &email,
        &code,
        "newpass",
        payment_api::data::now_ts(),
    )
    .await
    .unwrap();
    assert_eq!(out, forgetpwd::ResetOutcome::Success);

    let m = members::Entity::find_by_id(user)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.password, password::hash_password("newpass", salt));
    // code consumed
    let after = user_codes::Entity::find_by_id(filed.id)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, 1);
    assert!(after.uptime.is_some());
}

#[tokio::test]
async fn unknown_account_files_nothing() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let uname = format!("nope{user}");
    let out = forgetpwd::send_user_code(
        &s.db,
        &forgetpwd::NoopEmailProvider,
        &uname,
        "ghost@example.com",
        payment_api::data::now_ts(),
    )
    .await
    .unwrap();
    assert_eq!(out, forgetpwd::SendOutcome::UserNotFound);
    let n = user_codes::Entity::find()
        .filter(user_codes::Column::Username.eq(&uname))
        .count(&*s.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn wrong_and_expired_codes_are_rejected() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    let uname = format!("bad{user}");
    let email = format!("{user}@example.com");
    let salt = "7777";
    let original = password::hash_password("oldpass", salt);
    seed_member(&s, user, &uname, &email, salt).await;
    forgetpwd::send_user_code(
        &s.db,
        &forgetpwd::NoopEmailProvider,
        &uname,
        &email,
        payment_api::data::now_ts(),
    )
    .await
    .unwrap();
    let live = latest_code(&s, &uname).await;

    // a wrong code fails
    let out = forgetpwd::reset_password(
        &s.db,
        &uname,
        &email,
        "00000",
        "attacker",
        payment_api::data::now_ts(),
    )
    .await
    .unwrap();
    assert_eq!(out, forgetpwd::ResetOutcome::CodeInvalid);
    // an expired code fails even when it matches
    let now = payment_api::data::now_ts();
    user_codes::ActiveModel {
        r#type: Set(0),
        code: Set(Some("11111".into())),
        username: Set(Some(uname.clone())),
        email: Set(Some(email.clone())),
        mobile: Set(None),
        status: Set(0),
        ctime: Set(Some(now - 1200)),
        uptime: Set(None),
        endtime: Set(Some(now - 600)), // already expired
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
    let out = forgetpwd::reset_password(&s.db, &uname, &email, "11111", "attacker", now)
        .await
        .unwrap();
    assert_eq!(out, forgetpwd::ResetOutcome::CodeInvalid);

    // password untouched through both rejects
    let m = members::Entity::find_by_id(user)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.password, original);
    // the live code is still unconsumed
    assert_eq!(live.status, 0);
}
