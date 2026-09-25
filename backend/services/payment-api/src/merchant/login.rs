//! Merchant / agent login — the sequential check of
//! `User/LoginController::check` (`spec/05` §4.2), split into a pure
//! [`evaluate_login`] that fixes the check *order* and outcome (so it is
//! offline-unit-testable) and a thin [`login`] orchestrator that reads the
//! member row and drives the auth-failure limiter.
//!
//! The `login_ip` whitelist (§4.2) is evaluated from the member row here; the
//! session it establishes upstream (`session('user_auth')` + the
//! `session_random` single-sign-on kick, §4.3/§4.6) becomes a Redis panel
//! session whose `session_version` the login handler bumps on success. The
//! gateway itself authenticates by MD5 signature and does not use this path.

use crate::merchant::password;
use crate::merchant::MembersRepo;
use crate::merchant::Role;
use crate::ratelimit::{AuthKind, AuthLimiter};
use crate::state::{AppState, GatewayResult};

/// A minimal projection of a member row for the login decision, decoupled
/// from the ORM model so the ordering is testable without a database.
#[derive(Debug, Clone, Copy)]
pub struct MemberView<'a> {
    pub user_id: i64,
    pub groupid: i32,
    pub status: i32,
    pub salt: &'a str,
    pub password: &'a str,
    /// Precomputed from the (optional) `login_ip` whitelist + client IP; see
    /// [`ip_allowed`]. `true` when unrestricted.
    pub login_ip_allowed: bool,
}

/// The login decision. The rate-limiter lockout (`Banned`) is produced by the
/// orchestrator before [`evaluate_login`] runs, mirroring the legacy order
/// (`check_auth_error` precedes the password compare).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    Success {
        user_id: i64,
        role: Role,
    },
    /// Locked out by the auth-failure limiter; carries the retry hint.
    Banned {
        retry_after_secs: i64,
    },
    UserNotFound,
    IpNotAllowed,
    Disabled,
    BadPassword,
}

/// The check order (`spec/05` §4.2): existence → IP whitelist → status →
/// password. Password is verified last so a disabled/unknown account never
/// burns a comparison, and a wrong password is distinguished from a banned
/// account (the limiter decides `Banned` independently).
pub fn evaluate_login(view: Option<&MemberView<'_>>, password: &str) -> LoginOutcome {
    let Some(v) = view else {
        return LoginOutcome::UserNotFound;
    };
    if !v.login_ip_allowed {
        return LoginOutcome::IpNotAllowed;
    }
    if v.status != 1 {
        return LoginOutcome::Disabled;
    }
    if !password::verify_password(password, v.salt, v.password) {
        return LoginOutcome::BadPassword;
    }
    LoginOutcome::Success {
        user_id: v.user_id,
        role: Role::from_groupid(v.groupid),
    }
}

/// Whether a client IP passes a `\r\n`-separated whitelist
/// (`User/LoginController::check` L95-101): an empty/whitespace-only list
/// admits every IP; otherwise the client IP must appear verbatim on a line.
pub fn ip_allowed(whitelist: &str, client_ip: &str) -> bool {
    if whitelist.trim().is_empty() {
        return true;
    }
    whitelist
        .lines()
        .map(str::trim)
        .any(|line| !line.is_empty() && line == client_ip)
}

/// Loads the member, enforces the auth-failure lockout, runs
/// [`evaluate_login`], and records/clears the failure counter accordingly. It
/// takes the ORM handle and limiter directly (not the whole [`AppState`]) so
/// the whole check is drivable from a DB-gated test. The success path's
/// `session_version` bump (single-sign-on kick, §4.6) is the panel handler's
/// job, not this pure-ish check.
pub async fn login_with(
    db: &sea_orm::DatabaseConnection,
    limiter: &AuthLimiter,
    username: &str,
    password: &str,
    client_ip: &str,
) -> GatewayResult<LoginOutcome> {
    let member = MembersRepo::new(db)
        .by_username(username)
        .await
        .map_err(crate::state::db_err)?;

    let Some(member) = member else {
        // No uid to attribute a failure to; the legacy path logs against a null
        // id here (`spec/05` §12.3), which we drop.
        return Ok(LoginOutcome::UserNotFound);
    };

    if limiter.is_locked(AuthKind::MerchantLogin, member.id).await {
        let retry = limiter
            .retry_after(AuthKind::MerchantLogin, member.id)
            .await;
        return Ok(LoginOutcome::Banned {
            retry_after_secs: retry,
        });
    }

    let view = MemberView {
        user_id: member.id,
        groupid: member.groupid,
        status: member.status,
        salt: &member.salt,
        password: &member.password,
        // §4.2: a blank / absent whitelist admits every IP; otherwise the
        // client IP must be on one of the `\r\n`-separated lines.
        login_ip_allowed: ip_allowed(member.login_ip.as_deref().unwrap_or(""), client_ip),
    };
    let outcome = evaluate_login(Some(&view), password);

    match outcome {
        LoginOutcome::BadPassword => {
            limiter
                .record_fail(AuthKind::MerchantLogin, member.id)
                .await;
        }
        LoginOutcome::Success { user_id, .. } => {
            limiter.clear(AuthKind::MerchantLogin, user_id).await;
        }
        _ => {}
    }
    Ok(outcome)
}

/// Thin [`AppState`] wrapper over [`login_with`].
pub async fn login(
    state: &AppState,
    username: &str,
    password: &str,
    client_ip: &str,
) -> GatewayResult<LoginOutcome> {
    let limiter = AuthLimiter::new(state.redis.clone());
    login_with(&state.db, &limiter, username, password, client_ip).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(status: i32, salt: &'a str, hash: &'a str, ip_ok: bool) -> MemberView<'a> {
        MemberView {
            user_id: 7,
            groupid: 4,
            status,
            salt,
            password: hash,
            login_ip_allowed: ip_ok,
        }
    }

    #[test]
    fn success_and_role_mapping() {
        let salt = "1234";
        let hash = password::hash_password("pw", salt);
        let v = view(1, salt, &hash, true);
        assert_eq!(
            evaluate_login(Some(&v), "pw"),
            LoginOutcome::Success {
                user_id: 7,
                role: Role::Merchant
            }
        );
    }

    #[test]
    fn check_order_precedes_password() {
        let hash = password::hash_password("pw", "1234");
        // missing account short-circuits before anything else
        assert_eq!(evaluate_login(None, "pw"), LoginOutcome::UserNotFound);
        // IP rejection is evaluated before status and password
        let v = view(1, "1234", &hash, false);
        assert_eq!(evaluate_login(Some(&v), "pw"), LoginOutcome::IpNotAllowed);
        // disabled account is reported before a (correct) password is checked
        let v = view(0, "1234", &hash, true);
        assert_eq!(evaluate_login(Some(&v), "pw"), LoginOutcome::Disabled);
        // enabled + right IP + wrong password
        let v = view(1, "1234", &hash, true);
        assert_eq!(evaluate_login(Some(&v), "nope"), LoginOutcome::BadPassword);
    }

    #[test]
    fn ip_whitelist_semantics() {
        assert!(ip_allowed("  ", "1.2.3.4"), "empty whitelist admits all");
        assert!(ip_allowed("1.2.3.4\r\n5.6.7.8", "5.6.7.8"));
        assert!(!ip_allowed("1.2.3.4", "9.9.9.9"));
        assert!(!ip_allowed("1.2.3.4\r\n", "  "));
    }
}
