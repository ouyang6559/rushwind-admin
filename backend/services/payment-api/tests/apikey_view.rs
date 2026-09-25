//! DB- + Redis-gated tests for the §9 API-key reveal
//! (`merchant::apikey::view_apikey` — the port of `User/ChannelController::apikey`).
//! The pure message helpers are pinned offline in the module; here we drive the
//! reveal directly: the payment-password second factor (a hit reveals, a miss
//! rejects and records a failure), the counter clear on success, and the
//! auth_type=6 lockout after `MAX_AUTH_ERROR_TIMES` misses. Identity / session /
//! portal are the handler's job. Harness mirrors `invite_code.rs`; ids base
//! 11_900_000_000_000.

mod common;

use redis::aio::ConnectionManager;
use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::apikey::{self, ApikeyOutcome, MSG_PAY_PASSWORD_WRONG};
use payment_api::merchant::password;
use payment_api::ratelimit::{AuthKind, AuthLimiter, MAX_AUTH_ERROR_TIMES};

const BASE: i64 = 11_900_000_000_000;

async fn redis() -> Option<ConnectionManager> {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL").ok()?;
    redis::Client::open(url)
        .expect("redis client")
        .get_connection_manager()
        .await
        .ok()
}

/// Seeds a merchant with a known payment password and API key.
async fn seed_merchant(s: &Suite, user: i64, apikey: &str) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("ak{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        apikey: Set(Some(apikey.to_string())),
        pay_password: Set(Some(password::hash_pay_password("123456"))),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

fn limiter(cm: ConnectionManager) -> AuthLimiter {
    AuthLimiter::new(cm)
}

#[tokio::test]
async fn reveals_the_key_with_the_correct_pay_password() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let uid_v = uid(BASE);
    seed_merchant(&s, uid_v, "secret-key-abc").await;

    let out = apikey::view_apikey(&s.db, &limiter(cm), uid_v, "123456")
        .await
        .unwrap();
    assert_eq!(
        out,
        ApikeyOutcome::Revealed {
            apikey: Some("secret-key-abc".to_string())
        }
    );
}

#[tokio::test]
async fn rejects_a_wrong_pay_password_without_revealing() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let uid_v = uid(BASE);
    seed_merchant(&s, uid_v, "secret-key-xyz").await;

    let out = apikey::view_apikey(&s.db, &limiter(cm), uid_v, "wrong")
        .await
        .unwrap();
    assert!(matches!(
        out,
        ApikeyOutcome::BadPassword {
            msg: MSG_PAY_PASSWORD_WRONG
        }
    ));
}

#[tokio::test]
async fn success_clears_the_failure_counter() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let uid_v = uid(BASE);
    seed_merchant(&s, uid_v, "k").await;
    let lim = limiter(cm);

    // Two misses, then a hit must clear the window.
    apikey::view_apikey(&s.db, &lim, uid_v, "bad")
        .await
        .unwrap();
    apikey::view_apikey(&s.db, &lim, uid_v, "bad")
        .await
        .unwrap();
    assert_eq!(lim.count(AuthKind::ApiKey, uid_v).await, 2);
    let out = apikey::view_apikey(&s.db, &lim, uid_v, "123456")
        .await
        .unwrap();
    assert!(matches!(out, ApikeyOutcome::Revealed { .. }));
    assert_eq!(
        lim.count(AuthKind::ApiKey, uid_v).await,
        0,
        "clear on success"
    );
}

#[tokio::test]
async fn locks_out_after_the_threshold_of_misses() {
    let Some(s) = suite().await else { return };
    let Some(cm) = redis().await else { return };
    let uid_v = uid(BASE);
    seed_merchant(&s, uid_v, "k").await;
    let lim = limiter(cm);

    for _ in 0..MAX_AUTH_ERROR_TIMES {
        let out = apikey::view_apikey(&s.db, &lim, uid_v, "bad")
            .await
            .unwrap();
        assert!(matches!(out, ApikeyOutcome::BadPassword { .. }));
    }
    // The next call is gated by the lockout (no password check, no increment).
    let out = apikey::view_apikey(&s.db, &lim, uid_v, "123456")
        .await
        .unwrap();
    assert!(
        matches!(out, ApikeyOutcome::Locked { .. }),
        "expected lockout even with the correct password, got {out:?}"
    );
}
