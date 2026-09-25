//! Merchant second-factor gating for the §10 writes — the port of the
//! `User/AccountController::saveProfile` conditional-auth matrix (L89-137,
//! `spec/05` §10). The legacy write gates on an SMS (`auth_type = 0`) or a
//! Google-authenticator (`auth_type = 4`) factor ONLY when the merchant has
//! that channel enabled: with neither on it writes straight through.
//!
//! [`required_factor`] is the pure matrix (offline-testable); [`verify_google`]
//! orchestrates the Google factor against the shared [`AuthLimiter`] (auth
//! kind [`AuthKind::GoogleTotp`], the legacy `check_auth_error(_, 4)` /
//! `log_auth_error` / `clear_auth_error` triple) and [`crate::totp`], mirroring
//! the [`crate::merchant::apikey`] lockout-gate-then-verify ordering.
//!
//! Scope (registered as a decision memory):
//! - `sms_status()` models the legacy `smsStatus()` config lookup, but the Rust
//!   side has NOT modeled `pay_sms` / `websiteconfig`, so it is a constant
//!   `false` seam: the SMS factor branch is currently unreachable and the
//!   [`Factor::Sms`] arm is a documented placeholder, never returned in this
//!   environment. This is faithful to "a channel that is off gates nothing" —
//!   not a new weakening.
//! - the Google factor is fully self-contained (a base32 secret on the member
//!   row + RFC 6238 TOTP), so it lands complete and offline-testable.

use crate::merchant::apikey::lockout_msg;
use crate::ratelimit::{AuthKind, AuthLimiter};
use crate::totp;

/// The legacy generic parameter-error reject (`saveProfile` L89-137 default).
pub const MSG_PARAM_ERR: &str = "参数错误！";
/// The empty Google-code reject (`GoogleAuthenticator` guard, §10).
pub const MSG_GOOGLE_CODE_EMPTY: &str = "谷歌安全码不能为空！";
/// The wrong Google-code reject (§10).
pub const MSG_GOOGLE_CODE_WRONG: &str = "谷歌安全码错误！";

/// The factor a §10 write must satisfy. `None` = no factor configured, so the
/// write proceeds unguarded (the legacy "neither channel on" branch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Factor {
    None,
    Google,
    Sms,
}

/// The factor-check gate outcome: `Passed` lets the write proceed, `Rejected`
/// carries the exact legacy `ajaxReturn({status:0, msg})` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactorGate {
    Passed,
    Rejected { msg: String },
}

/// The legacy `smsStatus()` — whether the merchant SMS factor is configured and
/// on. The Rust rewrite has not modeled `pay_sms` / `websiteconfig`, so this is
/// a deliberate lazy seam pinned to `false`; every caller treats the SMS branch
/// as currently unreachable.
pub fn sms_status() -> bool {
    false
}

/// The `saveProfile` auth matrix (`spec/05` §10 L89-137). Given whether the
/// merchant has a Google secret (`has_google`) and the SMS channel is open
/// (`sms_open`), decide which factor the POSTed `auth_type`
/// (`0` = SMS, `1` = Google) must satisfy:
/// - both on → `auth_type` ∈ {0, 1} selects the factor;
/// - Google only → `auth_type` MUST be 1 (Google);
/// - SMS only → `auth_type` MUST be 0 (SMS);
/// - neither → [`Factor::None`] (write straight through).
///
/// Any other combination is the legacy parameter error.
pub fn required_factor(
    has_google: bool,
    sms_open: bool,
    auth_type: i32,
) -> Result<Factor, &'static str> {
    match (has_google, sms_open) {
        (true, true) => match auth_type {
            0 => Ok(Factor::Sms),
            1 => Ok(Factor::Google),
            _ => Err(MSG_PARAM_ERR),
        },
        (true, false) => {
            if auth_type == 1 {
                Ok(Factor::Google)
            } else {
                Err(MSG_PARAM_ERR)
            }
        }
        (false, true) => {
            if auth_type == 0 {
                Ok(Factor::Sms)
            } else {
                Err(MSG_PARAM_ERR)
            }
        }
        (false, false) => Ok(Factor::None),
    }
}

/// Runs the Google-authenticator factor for `uid` (auth kind
/// [`AuthKind::GoogleTotp`]), in the legacy order: the lockout gate first (no
/// increment), then the code — an empty code rejects without recording a
/// failure, a wrong code records a failure and rejects, a correct code clears
/// the counter and passes. `secret` is the member's stored base32 key (already
/// loaded by the caller for the matrix); `now` is the current unix seconds so
/// the whole path is clock-injectable and testable.
pub async fn verify_google(
    limiter: &AuthLimiter,
    uid: i64,
    secret: &str,
    code: &str,
    now: i64,
) -> FactorGate {
    if limiter.is_locked(AuthKind::GoogleTotp, uid).await {
        let secs = limiter.retry_after(AuthKind::GoogleTotp, uid).await;
        return FactorGate::Rejected {
            msg: lockout_msg(secs),
        };
    }
    if code.is_empty() {
        return FactorGate::Rejected {
            msg: MSG_GOOGLE_CODE_EMPTY.to_string(),
        };
    }
    if !totp::verify_code(secret, code, totp::GOOGLE_DISCREPANCY, now) {
        limiter.record_fail(AuthKind::GoogleTotp, uid).await;
        return FactorGate::Rejected {
            msg: MSG_GOOGLE_CODE_WRONG.to_string(),
        };
    }
    limiter.clear(AuthKind::GoogleTotp, uid).await;
    FactorGate::Passed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_only_requires_auth_type_one() {
        assert_eq!(required_factor(true, false, 1), Ok(Factor::Google));
        assert_eq!(required_factor(true, false, 0), Err(MSG_PARAM_ERR));
        assert_eq!(required_factor(true, false, 9), Err(MSG_PARAM_ERR));
    }

    #[test]
    fn sms_only_requires_auth_type_zero() {
        assert_eq!(required_factor(false, true, 0), Ok(Factor::Sms));
        assert_eq!(required_factor(false, true, 1), Err(MSG_PARAM_ERR));
    }

    #[test]
    fn both_open_lets_auth_type_choose() {
        assert_eq!(required_factor(true, true, 0), Ok(Factor::Sms));
        assert_eq!(required_factor(true, true, 1), Ok(Factor::Google));
        assert_eq!(required_factor(true, true, 2), Err(MSG_PARAM_ERR));
    }

    #[test]
    fn neither_open_writes_straight_through() {
        // No factor configured: `auth_type` is ignored, the write is unguarded.
        assert_eq!(required_factor(false, false, 0), Ok(Factor::None));
        assert_eq!(required_factor(false, false, 1), Ok(Factor::None));
        assert_eq!(required_factor(false, false, 7), Ok(Factor::None));
    }

    #[test]
    fn sms_status_is_a_lazy_false_seam() {
        assert!(!sms_status());
    }
}
