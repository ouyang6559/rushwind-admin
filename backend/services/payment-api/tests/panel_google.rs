//! DB + Redis coverage for the §10 Google-authenticator bind / unbind surface.
//! The handlers are thin接线, so (per the established service-layer-direct
//! discipline) we drive [`google::bind_initiate`] / [`google::bind_confirm`] /
//! [`google::unbind`] against a real [`PendingSecrets`] holder and
//! [`AuthLimiter`]:
//!
//! - initiate mints a pending secret (DB untouched) and REUSES it on a second
//!   call, while an already-bound member reports `AlreadyBound`;
//! - confirm needs a live pending secret AND a valid TOTP code, persists the
//!   secret only while still unbound, and consumes the pending secret on success;
//! - the `auth_type = 4` lockout trips after `MAX_AUTH_ERROR_TIMES` misses and
//!   then refuses even a correct code (the consistent orchestration that repairs
//!   the legacy clear-on-wrong bug);
//! - unbind clears the column, and a persist attempt against an already-bound
//!   member affects 0 rows yet still reports `Bound`.
//!
//! Harness mirrors `panel_mobile.rs`; ids base 70_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use redis::aio::ConnectionManager;
use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::google::{self, BindResult, Initiate, PendingSecrets, UnbindResult};
use payment_api::merchant::MembersRepo;
use payment_api::ratelimit::{AuthKind, AuthLimiter, MAX_AUTH_ERROR_TIMES};
use payment_api::totp;

const BASE: i64 = 70_000_000_000_000;
/// A fixed valid base32 secret for the "already bound" seeds.
const BOUND: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
/// A pinned unix second the whole TOTP path is evaluated at.
const NOW: i64 = 1_700_000_000;
/// Never a valid code for any secret's current window but a well-formed 6-digit
/// guess — a dependable always-wrong value (all digits identical).
const WRONG: &str = "000000";

async fn redis() -> Option<ConnectionManager> {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL").ok()?;
    redis::Client::open(url)
        .expect("redis client")
        .get_connection_manager()
        .await
        .ok()
}

/// Seeds a groupid-4 merchant with an optional bound Google secret.
async fn seed(s: &Suite, user: i64, google: Option<&str>) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("gg{user}")),
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

async fn secret_of(s: &Suite, user: i64) -> Option<String> {
    MembersRepo::new(&s.db)
        .by_id(user)
        .await
        .unwrap()
        .unwrap()
        .google_secret_key
}

/// Mints + returns the pending secret for a fresh merchant.
async fn initiate_secret(pending: &PendingSecrets, s: &Suite, user: i64) -> String {
    match google::bind_initiate(&s.db, pending, user).await.unwrap() {
        Initiate::Secret { secret, .. } => secret,
        Initiate::AlreadyBound => panic!("expected a pending secret"),
    }
}

// --- initiate ---------------------------------------------------------------

#[tokio::test]
async fn initiate_issues_and_reuses_a_pending_secret() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let pending = PendingSecrets::new(cm);
    pending.clear(user).await;

    let first = initiate_secret(&pending, &s, user).await;
    // the DB is untouched, the secret lives only in the pending holder
    assert!(secret_of(&s, user).await.is_none());
    assert_eq!(pending.get(user).await.as_deref(), Some(first.as_str()));
    // a second initiate reuses the SAME pending secret (legacy session reuse)
    assert_eq!(initiate_secret(&pending, &s, user).await, first);
}

#[tokio::test]
async fn initiate_reports_already_bound() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some(BOUND)).await;
    let pending = PendingSecrets::new(cm);

    let out = google::bind_initiate(&s.db, &pending, user).await.unwrap();
    assert_eq!(out, Initiate::AlreadyBound);
}

// --- confirm ----------------------------------------------------------------

#[tokio::test]
async fn bind_confirm_requires_a_valid_code() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let pending = PendingSecrets::new(cm.clone());
    let lim = AuthLimiter::new(cm);
    pending.clear(user).await;

    let secret = initiate_secret(&pending, &s, user).await;
    let valid = totp::totp_code(&secret, NOW).unwrap();

    // empty code → 请输入验证码 (before any verify, no counter change)
    let out = google::bind_confirm(&s.db, &pending, &lim, user, "", NOW)
        .await
        .unwrap();
    assert_eq!(out, BindResult::EmptyCode);
    assert_eq!(lim.count(AuthKind::GoogleTotp, user).await, 0);

    // wrong code → BadCode, one failure recorded
    let out = google::bind_confirm(&s.db, &pending, &lim, user, WRONG, NOW)
        .await
        .unwrap();
    assert_eq!(out, BindResult::BadCode);
    assert_eq!(lim.count(AuthKind::GoogleTotp, user).await, 1);

    // valid code → Bound; persisted, pending consumed, counter cleared
    let out = google::bind_confirm(&s.db, &pending, &lim, user, &valid, NOW)
        .await
        .unwrap();
    assert!(matches!(out, BindResult::Bound { status: 1 }), "{out:?}");
    assert_eq!(secret_of(&s, user).await.as_deref(), Some(secret.as_str()));
    assert!(pending.get(user).await.is_none());
    assert_eq!(lim.count(AuthKind::GoogleTotp, user).await, 0);
}

#[tokio::test]
async fn bind_confirm_without_a_pending_secret_is_no_pending() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let pending = PendingSecrets::new(cm.clone());
    let lim = AuthLimiter::new(cm);
    pending.clear(user).await;

    let out = google::bind_confirm(&s.db, &pending, &lim, user, "123456", NOW)
        .await
        .unwrap();
    assert_eq!(out, BindResult::NoPending);
}

#[tokio::test]
async fn bind_confirm_locks_after_max_misses() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, None).await;
    let pending = PendingSecrets::new(cm.clone());
    let lim = AuthLimiter::new(cm);
    pending.clear(user).await;

    let secret = initiate_secret(&pending, &s, user).await;
    let valid = totp::totp_code(&secret, NOW).unwrap();

    for _ in 0..MAX_AUTH_ERROR_TIMES {
        let out = google::bind_confirm(&s.db, &pending, &lim, user, WRONG, NOW)
            .await
            .unwrap();
        assert_eq!(out, BindResult::BadCode);
    }
    assert!(lim.is_locked(AuthKind::GoogleTotp, user).await);

    // Once locked, even the correct code is refused (no verify, no write).
    let out = google::bind_confirm(&s.db, &pending, &lim, user, &valid, NOW)
        .await
        .unwrap();
    match out {
        BindResult::Locked { msg } => assert!(msg.contains("输入错误次数过多"), "got {msg}"),
        other => panic!("expected lockout, got {other:?}"),
    }
    assert!(secret_of(&s, user).await.is_none());
}

// --- unbind -----------------------------------------------------------------

#[tokio::test]
async fn unbind_clears_the_secret() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some(BOUND)).await;

    let out = google::unbind(&s.db, user).await.unwrap();
    assert_eq!(out, UnbindResult::Unbound);
    assert!(secret_of(&s, user)
        .await
        .unwrap_or_default()
        .trim()
        .is_empty());
}

#[tokio::test]
async fn persist_only_when_currently_unbound() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let user = uid(BASE);
    seed(&s, user, Some(BOUND)).await;
    let pending = PendingSecrets::new(cm.clone());
    let lim = AuthLimiter::new(cm);

    // A stale pending secret + a valid code for an ALREADY-BOUND member: the
    // gated update matches nothing (0 rows) yet still reports Bound (legacy).
    let new_secret = totp::create_secret();
    pending.put(user, &new_secret).await;
    let valid = totp::totp_code(&new_secret, NOW).unwrap();
    let out = google::bind_confirm(&s.db, &pending, &lim, user, &valid, NOW)
        .await
        .unwrap();
    assert!(matches!(out, BindResult::Bound { status: 0 }), "{out:?}");
    assert_eq!(secret_of(&s, user).await.as_deref(), Some(BOUND));
}
