//! The auth gate for the protected subtree.
//!
//! Two-stage validation:
//! `TokenChecker.IsValidAccessToken` → `Authenticator.Authenticate(ACCESS)`:
//! signature/validity through the framework engine, then the server-side
//! Redis whitelist (`at:{ct}:{uid}:{jti}` exact compare) and blacklist
//! (`bl:{jti}`) checks through [`AccessTokenChecker`]. Failures render
//! the four-field status envelope: reason `UNAUTHORIZED`, and the exact
//! message the wire contract pins — `missing bearer
//! token` when no bearer credential is presented, `access token expired`
//! for EVERY validation failure (all causes collapse onto
//! that text).
//!
//! Success inserts the claim bag into the request extensions — the
//! request-context injection — consumed by the glue into
//! the per-request [`rushwind_http_binding::ctx::RequestContext`].

use std::sync::Arc;

use async_trait::async_trait;
use rushwind_authn::{Authenticator, AuthnError};

/// The server-side session checks (Redis whitelist/blacklist), split out
/// of the engine so the storage stays service-side.
#[async_trait]
pub trait AccessTokenChecker: Send + Sync {
    /// `at:{ct}:{uid}:{jti}` exact-match — false means revoked/expired.
    async fn is_valid_access_token(&self, uid: u32, jti: &str, token: &str) -> bool;
    /// `bl:{jti}` existence.
    async fn is_blocked_access_token(&self, jti: &str) -> bool;
}

/// The gate middleware body: verify the credential, pass through with the
/// claims injected, or render the 401 envelope.
pub async fn auth_gate(
    auth: Arc<dyn Authenticator>,
    checker: Arc<dyn AccessTokenChecker + 'static>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|t| t.to_owned());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    match auth.authenticate(&headers) {
        Ok(claims) => {
            // The second stage: whitelist + blacklist.
            let uid = claims
                .0
                .get("uid")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .unwrap_or(0);
            let jti = claims.get_jwt_id().unwrap_or_default();
            let token = bearer.unwrap_or_default();
            let valid = if jti.is_empty() {
                false
            } else {
                checker.is_valid_access_token(uid, &jti, &token).await
                    && !checker.is_blocked_access_token(&jti).await
            };
            if !valid {
                return rushwind_http_binding::envelope::error_response(unauthorized(
                    AuthnError::TokenExpired,
                ));
            }
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(err) => rushwind_http_binding::envelope::error_response(unauthorized(err)),
    }
}

/// The middleware failure: the status the admin error tables anchor to
/// `UNAUTHORIZED` (401), and the branch's fixed message text.
fn unauthorized(err: AuthnError) -> rushwind_http_binding::envelope::StatusError {
    let message = match err {
        AuthnError::MissingBearerToken => "missing bearer token",
        _ => "access token expired",
    };
    let status = proto::tables::error_status("admin.service.v1", "UNAUTHORIZED").unwrap_or(401);
    rushwind_http_binding::envelope::StatusError::new(status, "UNAUTHORIZED", message)
}
