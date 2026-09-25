//! The panel's server-side session: a bearer token → [`PanelSession`] record
//! held in Redis with a TTL (the faithful port of PHP's `session('user_auth')`
//! the legacy `isLogin()` reads). The token is a fresh CSPRNG hex (the same
//! generator the merchant api-key uses), so it is unguessable and revocable —
//! a logout deletes the key and a re-login can rotate it.

use std::sync::Arc;
use std::time::Duration;

use redis::aio::ConnectionManager;
use redis::AsyncCommands;

use crate::merchant::generate_apikey;

/// The session lifetime. Legacy PHP `user_auth` sessions ride the framework
/// session GC; two hours is a conservative web-console idle window and keeps
/// a leaked token short-lived.
pub const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);

/// An authenticated panel identity. `version` is the single-sign-on token
/// copied off the member's `session_version` at login; it is replayed on every
/// request and compared against the member's CURRENT version so a later login
/// elsewhere invalidates this session (§4.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSession {
    pub user_id: i64,
    pub groupid: i32,
    pub version: String,
}

/// Whether a session still carries the member's CURRENT single-sign-on version.
/// A `false` result means the account has signed in elsewhere (the version was
/// bumped) and this token must be rejected — the faithful, non-typo port of
/// the `User/UserController` `session_random` kick (§4.6).
pub fn session_is_live(session: &PanelSession, member_version: &str) -> bool {
    session.version == member_version
}

/// The Redis key a token resolves to. Pure + unit-tested so a key-shape change
/// never silently orphans live sessions.
pub fn session_key(token: &str) -> String {
    format!("payment:panel:session:{token}")
}

/// The stored value — `user_id|groupid|version`. `|` never appears in any
/// field (the version is hex), so the split is unambiguous without pulling a
/// serde format into Redis.
fn encode_session(s: &PanelSession) -> String {
    format!("{}|{}|{}", s.user_id, s.groupid, s.version)
}

/// The inverse of [`encode_session`]; `None` on any malformed record (a hand-
/// edited key or a pre-`version` 2-field session is treated as no session, not
/// a panic — forcing a clean re-login rather than guessing a version).
fn decode_session(raw: &str) -> Option<PanelSession> {
    let mut it = raw.split('|');
    let user_id = it.next()?.parse().ok()?;
    let groupid = it.next()?.parse().ok()?;
    let version = it.next()?;
    if it.next().is_some() {
        return None; // stray extra field
    }
    Some(PanelSession {
        user_id,
        groupid,
        version: version.to_string(),
    })
}

/// A Redis-backed session store. Cheap to clone (shares the connection pool).
#[derive(Clone)]
pub struct SessionStore {
    redis: ConnectionManager,
}

impl SessionStore {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }

    /// Mints a token, records `user_id|groupid|version` under it with the TTL,
    /// and returns the token. The `version` is the caller's freshly bumped
    /// member `session_version`, so an older session carrying the previous
    /// value is rejected on its next request (`session_is_live`, §4.6).
    pub async fn issue(&self, session: PanelSession) -> redis::RedisResult<String> {
        let token = generate_apikey();
        let mut conn = self.redis.clone();
        conn.set_ex::<_, _, ()>(
            session_key(&token),
            encode_session(&session),
            SESSION_TTL.as_secs(),
        )
        .await?;
        Ok(token)
    }

    /// Resolves a token to its session, or `None` when absent / expired /
    /// malformed. A Redis error degrades to `None` (fail-closed: an outage
    /// must not authenticate anyone).
    pub async fn lookup(&self, token: &str) -> Option<PanelSession> {
        let mut conn = self.redis.clone();
        let raw: String = conn.get(session_key(token)).await.ok()?;
        decode_session(&raw)
    }

    /// Drops a session (logout). Idempotent — a missing key is not an error.
    pub async fn revoke(&self, token: &str) {
        let mut conn = self.redis.clone();
        let _: Result<i64, _> = conn.del(session_key(token)).await;
    }
}

/// A store shared across handlers, behind the same `Arc` the state carries.
pub type SharedSessionStore = Arc<SessionStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_namespaced_by_token() {
        assert_eq!(session_key("abc123"), "payment:panel:session:abc123");
    }

    #[test]
    fn encode_roundtrips_and_rejects_junk() {
        let s = PanelSession {
            user_id: 42,
            groupid: 6,
            version: "deadbeef".to_string(),
        };
        assert_eq!(decode_session(&encode_session(&s)), Some(s));
        assert_eq!(decode_session(""), None);
        assert_eq!(decode_session("42"), None);
        assert_eq!(decode_session("x|y|z"), None);
        assert_eq!(decode_session("42|"), None);
        // a pre-version 2-field record (or an over-long one) is rejected
        assert_eq!(decode_session("42|6"), None);
        assert_eq!(decode_session("42|6|abc|extra"), None);
    }

    #[test]
    fn live_only_while_the_version_matches() {
        let s = PanelSession {
            user_id: 1,
            groupid: 4,
            version: "v1".to_string(),
        };
        assert!(session_is_live(&s, "v1"));
        // a login elsewhere bumped the member version → this session is stale
        assert!(!session_is_live(&s, "v2"));
        assert!(!session_is_live(&s, ""));
    }
}
