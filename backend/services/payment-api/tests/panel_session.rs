//! Redis-gated round-trip for the panel session store: `issue` → `lookup` →
//! `revoke` against a live Redis, plus the fail-closed behaviour on an unknown
//! token (an outage or a bogus key must never authenticate a caller). Skips
//! cleanly when no Redis is reachable, mirroring `risk_db.rs`'s gate but
//! degrading to a no-op rather than a panic.

#![allow(clippy::unwrap_used)]

use redis::aio::ConnectionManager;

use payment_api::panel::session::{PanelSession, SessionStore};

async fn redis() -> Option<ConnectionManager> {
    let url = std::env::var("PAYMENT_TEST_REDIS_URL")
        .unwrap_or_else(|_| "redis://:*Abcd123456@127.0.0.1:6379/".to_string());
    redis::Client::open(url)
        .ok()?
        .get_connection_manager()
        .await
        .ok()
}

#[tokio::test]
async fn session_round_trips_and_revokes() {
    let Some(r) = redis().await else {
        eprintln!("skip: no redis reachable");
        return;
    };
    let store = SessionStore::new(r);
    let session = PanelSession {
        user_id: 9,
        groupid: 4,
        version: "v9".to_string(),
    };

    let token = store.issue(session.clone()).await.unwrap();
    assert!(!token.is_empty(), "a token is minted");
    // A fresh token resolves to exactly the identity that was issued.
    assert_eq!(store.lookup(&token).await, Some(session));

    // Logout revokes it; a second lookup is now unauthenticated.
    store.revoke(&token).await;
    assert_eq!(store.lookup(&token).await, None);

    // An unknown token is fail-closed (never a session).
    assert_eq!(store.lookup("definitely-not-a-session").await, None);
}

#[tokio::test]
async fn each_issue_mints_a_distinct_token() {
    let Some(r) = redis().await else {
        return;
    };
    let store = SessionStore::new(r);
    let session = PanelSession {
        user_id: 11,
        groupid: 6,
        version: "v11".to_string(),
    };
    let a = store.issue(session.clone()).await.unwrap();
    let b = store.issue(session.clone()).await.unwrap();
    assert_ne!(
        a, b,
        "tokens are per-issue unguessable, not derived from the id"
    );
    // Both are independently valid until revoked.
    assert_eq!(store.lookup(&a).await, Some(session.clone()));
    assert_eq!(store.lookup(&b).await, Some(session));
    store.revoke(&a).await;
    store.revoke(&b).await;
}
