//! API-key reveal — the `spec/05` §9 "查看 APIKEY" flow, the merchant-panel
//! port of `User/ChannelController::apikey` (L59-75). The signing secret is
//! guarded by the merchant's OWN payment password as a second factor
//! (`md5(code) == paypassword`, auth_type=6), with the shared auth-failure
//! limiter throttling repeated guesses. The pure message helpers
//! ([`sec2time`] / [`lockout_msg`]) and the whole reveal orchestration
//! ([`view_apikey`]) live here; the panel handler only adds the session gate.
//!
//! Scope notes (registered as a decision memory):
//! - legacy `ChannelController` has only the VIEW — there is NO merchant-side
//!   reset/rotate endpoint, so [`crate::merchant::MembersRepo::rotate_apikey`]
//!   stays a service-layer primitive for the future back-office, deliberately
//!   not exposed over HTTP (per the sensitive-credential timing rule);
//! - a wrong / absent payment password never reveals the key; the member is
//!   guaranteed to exist by the session gate, so a missing row folds to the
//!   same reject as a bad password rather than a distinct message.

use sea_orm::DatabaseConnection;

use crate::merchant::password::verify_pay_password;
use crate::merchant::MembersRepo;
use crate::ratelimit::{AuthKind, AuthLimiter};
use crate::state::GatewayResult;

/// The exact legacy reject message for a wrong payment password.
pub const MSG_PAY_PASSWORD_WRONG: &str = "支付密码错误";

/// The reveal outcome, shaped for the legacy `ajaxReturn` (`status` 0/1 plus
/// `msg` on a reject or `apikey` on success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApikeyOutcome {
    /// Too many failed attempts; `msg` carries the `sec2Time` retry window.
    Locked { msg: String },
    /// The payment password did not match — the key is not revealed.
    BadPassword { msg: &'static str },
    /// Verified — the merchant's API key (possibly `None` if never minted).
    Revealed { apikey: Option<String> },
}

/// The legacy `sec2Time` (`function.php:1233`): render a duration as the
/// concatenation of ONLY its non-zero `年/天/小时/分/秒` parts (0 seconds →
/// the empty string).
pub fn sec2time(secs: i64) -> String {
    let mut t = secs.max(0);
    let mut out = String::new();
    let years = t / 31_556_926;
    if years > 0 {
        out.push_str(&format!("{years}年"));
        t %= 31_556_926;
    }
    let days = t / 86_400;
    if days > 0 {
        out.push_str(&format!("{days}天"));
        t %= 86_400;
    }
    let hours = t / 3_600;
    if hours > 0 {
        out.push_str(&format!("{hours}小时"));
        t %= 3_600;
    }
    let minutes = t / 60;
    if minutes > 0 {
        out.push_str(&format!("{minutes}分"));
        t %= 60;
    }
    if t > 0 {
        out.push_str(&format!("{t}秒"));
    }
    out
}

/// The lockout message, the legacy `check_auth_error` format string (note the
/// single space before `再试!`).
pub fn lockout_msg(secs: i64) -> String {
    format!("输入错误次数过多，请于{}后 再试!", sec2time(secs))
}

/// Runs the §9 reveal against the database and limiter (auth_type=6), in the
/// legacy order: the lockout gate first (no increment), then the payment-
/// password compare — a miss records a failure and rejects, a hit clears the
/// counter and returns the key. Takes the primitives (not `AppState`) so it is
/// driven directly by the DB-gated tests.
pub async fn view_apikey(
    db: &DatabaseConnection,
    limiter: &AuthLimiter,
    uid: i64,
    code: &str,
) -> GatewayResult<ApikeyOutcome> {
    if limiter.is_locked(AuthKind::ApiKey, uid).await {
        let secs = limiter.retry_after(AuthKind::ApiKey, uid).await;
        return Ok(ApikeyOutcome::Locked {
            msg: lockout_msg(secs),
        });
    }
    let member = MembersRepo::new(db).by_id(uid).await?;
    let stored = member
        .as_ref()
        .and_then(|m| m.pay_password.clone())
        .unwrap_or_default();
    if !verify_pay_password(code, &stored) {
        limiter.record_fail(AuthKind::ApiKey, uid).await;
        return Ok(ApikeyOutcome::BadPassword {
            msg: MSG_PAY_PASSWORD_WRONG,
        });
    }
    limiter.clear(AuthKind::ApiKey, uid).await;
    Ok(ApikeyOutcome::Revealed {
        apikey: member.and_then(|m| m.apikey),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sec2time_renders_only_non_zero_units() {
        assert_eq!(sec2time(0), "");
        assert_eq!(sec2time(-5), "");
        assert_eq!(sec2time(50), "50秒");
        assert_eq!(sec2time(60), "1分");
        assert_eq!(sec2time(65), "1分5秒");
        assert_eq!(sec2time(3_600), "1小时");
        assert_eq!(sec2time(3_661), "1小时1分1秒");
        assert_eq!(sec2time(86_400), "1天");
        assert_eq!(sec2time(90_061), "1天1小时1分1秒");
    }

    #[test]
    fn lockout_message_uses_the_legacy_format() {
        assert_eq!(lockout_msg(65), "输入错误次数过多，请于1分5秒后 再试!");
        // a cleared / no-TTL window (0 secs) still yields the (empty-duration) message
        assert_eq!(lockout_msg(0), "输入错误次数过多，请于后 再试!");
    }
}
