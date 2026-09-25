//! Auth-failure rate limiting — the Redis port of the legacy
//! `check_auth_error` / `log_auth_error` / `clear_auth_error` triple
//! (`Common/Common/function.php` L1286-1333, `spec/05` §4.4; target design
//! §13.2). A fixed-window counter keyed by `(auth kind, uid)` replaces the
//! `pay_auth_error_log` table: threshold `max_auth_error_times` (default 5)
//! within a window of `auth_error_ban_time` minutes (default 10).
//!
//! [`is_banned`] is the pure decision (offline-testable); the counter
//! operations go through Redis and fail *closed* only on an explicit lock —
//! a Redis error is treated as "not locked" so an outage degrades to the
//! legacy fail-open rather than a login lockout (mirrors `admin-api`).

use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// `max_auth_error_times` default (`pay_websiteconfig`, `spec/05` §2.3).
pub const MAX_AUTH_ERROR_TIMES: i64 = 5;
/// `auth_error_ban_time` default, in minutes.
pub const AUTH_ERROR_BAN_MINS: i64 = 10;
/// The lock window in seconds (ban minutes × 60).
pub const AUTH_ERROR_BAN_SECS: i64 = AUTH_ERROR_BAN_MINS * 60;

/// The protected operation a failure is attributed to — the legacy
/// `auth_type` (`spec/05` §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    MerchantLogin,
    PlatformLogin,
    MerchantSms,
    PlatformSms,
    GoogleTotp,
    PayPassword,
    ApiKey,
}

impl AuthKind {
    /// The legacy `auth_type` integer (kept for parity / diagnostics).
    pub fn code(self) -> u8 {
        match self {
            AuthKind::MerchantLogin => 0,
            AuthKind::PlatformLogin => 1,
            AuthKind::MerchantSms => 2,
            AuthKind::PlatformSms => 3,
            AuthKind::GoogleTotp => 4,
            AuthKind::PayPassword => 5,
            AuthKind::ApiKey => 6,
        }
    }
}

fn fail_key(kind: AuthKind, uid: i64) -> String {
    format!("payment:auth:fail:{}:{uid}", kind.code())
}

/// The pure lockout decision.
pub fn is_banned(count: i64) -> bool {
    count >= MAX_AUTH_ERROR_TIMES
}

/// A Redis-backed auth-failure limiter (cheap to clone; shares the pool).
#[derive(Clone)]
pub struct AuthLimiter {
    redis: ConnectionManager,
}

impl AuthLimiter {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }

    /// The current window count (0 when absent / on error).
    pub async fn count(&self, kind: AuthKind, uid: i64) -> i64 {
        let mut conn = self.redis.clone();
        conn.get::<_, Option<i64>>(fail_key(kind, uid))
            .await
            .unwrap_or(None)
            .unwrap_or(0)
    }

    /// The pre-operation lockout gate (no increment).
    pub async fn is_locked(&self, kind: AuthKind, uid: i64) -> bool {
        is_banned(self.count(kind, uid).await)
    }

    /// Seconds until the current window clears (`-2`/`-1` TTL → 0).
    pub async fn retry_after(&self, kind: AuthKind, uid: i64) -> i64 {
        let mut conn = self.redis.clone();
        match conn.ttl::<_, i64>(fail_key(kind, uid)).await {
            Ok(t) if t > 0 => t,
            _ => 0,
        }
    }

    /// Records one failure; sets the window TTL on the first hit.
    pub async fn record_fail(&self, kind: AuthKind, uid: i64) {
        let mut conn = self.redis.clone();
        let key = fail_key(kind, uid);
        let count: Option<i64> = conn.incr(&key, 1).await.ok();
        if count == Some(1) {
            let _: redis::RedisResult<i64> = conn.expire(&key, AUTH_ERROR_BAN_SECS).await;
        }
    }

    /// Clears the counter on a success (`clear_auth_error`).
    pub async fn clear(&self, kind: AuthKind, uid: i64) {
        let mut conn = self.redis.clone();
        let _: Result<i64, _> = conn.del(fail_key(kind, uid)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_kind_codes_match_legacy() {
        assert_eq!(AuthKind::MerchantLogin.code(), 0);
        assert_eq!(AuthKind::PayPassword.code(), 5);
        assert_eq!(AuthKind::ApiKey.code(), 6);
    }

    #[test]
    fn ban_trips_at_threshold() {
        assert!(!is_banned(MAX_AUTH_ERROR_TIMES - 1));
        assert!(is_banned(MAX_AUTH_ERROR_TIMES));
        assert!(is_banned(MAX_AUTH_ERROR_TIMES + 1));
    }

    #[test]
    fn keys_are_scoped_by_kind_and_uid() {
        assert_eq!(
            fail_key(AuthKind::MerchantLogin, 42),
            "payment:auth:fail:0:42"
        );
        assert_ne!(
            fail_key(AuthKind::ApiKey, 42),
            fail_key(AuthKind::MerchantLogin, 42)
        );
    }
}
