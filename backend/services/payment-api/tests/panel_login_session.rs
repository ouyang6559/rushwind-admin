//! DB- + Redis-gated tests for the §4.2 / §4.6 login seams. Two things were
//! previously stubbed and are wired here:
//!
//! - the `login_ip` whitelist (`User/LoginController::check` L95-101): the
//!   client IP is checked against the member's `\r\n`-separated list before the
//!   password, an empty / absent list admitting every IP. Driven through
//!   [`login::login_with`] (the primitive the `POST /panel/login` handler calls).
//! - the `session_random` single-sign-on kick (§4.6, modelled as
//!   `session_version`): a login bumps the member version and issues a session
//!   carrying it, and [`require_live_session`] rejects any session whose held
//!   version no longer matches — i.e. after a login elsewhere.
//!
//! Harness mirrors `apikey_view.rs`; ids base 20_000_000_000_000 (clear of the
//! ≤ 11.9e12 + clock-mix reach of every other suite's ids).

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{HeaderMap, HeaderValue};
use redis::aio::ConnectionManager;
use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::login::{self, LoginOutcome};
use payment_api::merchant::password;
use payment_api::merchant::MembersRepo;
use payment_api::panel::handlers::require_live_session;
use payment_api::panel::session::{PanelSession, SessionStore};
use payment_api::ratelimit::AuthLimiter;

const BASE: i64 = 20_000_000_000_000;

async fn redis() -> Option<ConnectionManager> {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL").ok()?;
    redis::Client::open(url)
        .expect("redis client")
        .get_connection_manager()
        .await
        .ok()
}

/// Seeds a member with a known password and an optional login-IP whitelist.
async fn seed_member(
    s: &Suite,
    user: i64,
    username: &str,
    salt: &str,
    plain: &str,
    login_ip: Option<&str>,
) {
    members::ActiveModel {
        id: Set(user),
        username: Set(username.to_string()),
        password: Set(password::hash_password(plain, salt)),
        groupid: Set(4),
        salt: Set(salt.to_string()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        apikey: Set(Some("32charapikey000000000000000000aa".into())),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        login_ip: Set(login_ip.map(str::to_string)),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

fn limiter(cm: ConnectionManager) -> AuthLimiter {
    AuthLimiter::new(cm)
}

fn bearer(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        axum::http::HeaderName::from_static("x-panel-token"),
        HeaderValue::from_str(token).unwrap(),
    );
    h
}

// --- §4.2 login_ip whitelist ------------------------------------------------

#[tokio::test]
async fn listed_client_ip_passes_and_others_are_rejected() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(
        &s,
        user,
        &format!("ip{user}"),
        "sadsalt",
        "pw",
        Some("1.1.1.1\r\n2.2.2.2"),
    )
    .await;

    // An IP on the whitelist clears the gate and reaches the password check.
    let ok = login::login_with(
        &s.db,
        &limiter(cm.clone()),
        &format!("ip{user}"),
        "pw",
        "2.2.2.2",
    )
    .await
    .unwrap();
    assert_eq!(
        ok,
        LoginOutcome::Success {
            user_id: user,
            role: payment_api::merchant::Role::Merchant
        }
    );

    // An IP off the list is rejected BEFORE the password (right pw still fails).
    let off = login::login_with(
        &s.db,
        &limiter(cm.clone()),
        &format!("ip{user}"),
        "pw",
        "9.9.9.9",
    )
    .await
    .unwrap();
    assert_eq!(off, LoginOutcome::IpNotAllowed);
}

#[tokio::test]
async fn an_empty_whitelist_admits_every_client_ip() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, &format!("nw{user}"), "sadsalt", "pw", None).await;

    let out = login::login_with(&s.db, &limiter(cm), &format!("nw{user}"), "pw", "8.8.8.8")
        .await
        .unwrap();
    assert_eq!(
        out,
        LoginOutcome::Success {
            user_id: user,
            role: payment_api::merchant::Role::Merchant
        }
    );
}

// --- §4.6 session_version single-sign-on kick -------------------------------

#[tokio::test]
async fn bump_persists_the_current_version_on_the_member() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, &format!("bv{user}"), "sadsalt", "pw", None).await;

    let repo = MembersRepo::new(&s.db);
    repo.bump_session_version(user, "V1").await.unwrap();
    let member = repo.by_id(user).await.unwrap().unwrap();
    assert_eq!(member.session_version.as_deref(), Some("V1"));

    // A re-login overwrites it; older values are gone.
    repo.bump_session_version(user, "V2").await.unwrap();
    let member = repo.by_id(user).await.unwrap().unwrap();
    assert_eq!(member.session_version.as_deref(), Some("V2"));
}

#[tokio::test]
async fn a_session_is_live_until_a_login_elsewhere_bumps_the_version() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, &format!("sk{user}"), "sadsalt", "pw", None).await;

    let repo = MembersRepo::new(&s.db);
    let store = SessionStore::new(cm);

    // Login #1: bump + issue a session carrying the new version.
    repo.bump_session_version(user, "V1").await.unwrap();
    let token = store
        .issue(PanelSession {
            user_id: user,
            groupid: 4,
            version: "V1".to_string(),
        })
        .await
        .unwrap();
    // The live session passes the kick gate.
    let live = require_live_session(&store, &s.db, &bearer(&token)).await;
    assert_eq!(live.unwrap().user_id, user);

    // Login #2 elsewhere bumps the member version; the old token is now stale.
    repo.bump_session_version(user, "V2").await.unwrap();
    let kicked = require_live_session(&store, &s.db, &bearer(&token)).await;
    assert!(
        kicked.is_err(),
        "a bumped version must revoke the older session"
    );
}
