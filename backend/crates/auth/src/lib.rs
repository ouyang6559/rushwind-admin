//! The auth gate for the protected subtree.
//!
//! Four-stage validation, in wire order:
//! `Authenticator.Authenticate(ACCESS)` → `AccessTokenChecker`
//! (Redis whitelist `at:{ct}:{uid}:{jti}` exact compare + blacklist
//! `bl:{jti}`) → `TenantAccessChecker` (tenant status / expiry
//! read-only / plan module whitelist — tenant members only, platform
//! admins skip) → `AuthorizationEvaluator` (per-role evaluation with
//! the audit trail). Failures render the four-field status envelope:
//! reason `UNAUTHORIZED` for every credential failure (message pinned
//! to `missing bearer token` / `access token expired`), reason
//! `FORBIDDEN` with the check's own message for the two later stages.
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

/// The tenant-level access checks — status, expiry read-only, and the
/// plan module whitelist — split out so the storage stays service-side.
/// Only tenant members (`tid > 0`) reach it; platform admins skip.
#[async_trait]
pub trait TenantAccessChecker: Send + Sync {
    /// `path` is the matched route template (the reference
    /// `PathTemplate()` form, `/admin/v1/users/{id}`), `method` the
    /// HTTP verb. `Err` carries the exact FORBIDDEN envelope text.
    async fn check_tenant_access(
        &self,
        tenant_id: u32,
        path: &str,
        method: &str,
    ) -> Result<(), String>;
}

/// The per-request inputs of the authorization evaluation.
pub struct AuthzEvaluation<'a> {
    pub user_id: u32,
    pub tenant_id: u32,
    /// The token's role codes — one evaluation per role, the first
    /// permitting role wins.
    pub roles: &'a [String],
    /// The HTTP verb.
    pub action: &'a str,
    /// The matched route template.
    pub resource: &'a str,
    /// Best-effort client IP (`X-Real-IP`, then the first
    /// `X-Forwarded-For` entry).
    pub ip: &'a str,
    /// `traceparent`'s trace id, else `X-Request-Id`.
    pub trace_id: &'a str,
}

/// The authorization point: evaluates the request against the policy
/// engine and records the evaluation trail. `Err` carries the exact
/// FORBIDDEN envelope text.
#[async_trait]
pub trait AuthorizationEvaluator: Send + Sync {
    async fn authorize(&self, evaluation: AuthzEvaluation<'_>) -> Result<(), String>;
}

/// The gate middleware body: verify the credential, run the tenant and
/// authorization stages, pass through with the claims injected, or
/// render the envelope.
pub async fn auth_gate(
    auth: Arc<dyn Authenticator>,
    checker: Arc<dyn AccessTokenChecker + 'static>,
    tenant_checker: Arc<dyn TenantAccessChecker + 'static>,
    authorizer: Arc<dyn AuthorizationEvaluator + 'static>,
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

            // The request facts the later stages evaluate against: the
            // matched route template (the reference `PathTemplate()`
            // form), the verb, and the best-effort client headers.
            let path = req
                .extensions()
                .get::<axum::extract::MatchedPath>()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_default();
            let method = req.method().to_string();
            let ip = client_ip(req.headers());
            let trace_id = trace_id(req.headers());

            // The tenant stage: tenant members only.
            let tenant_id = claims
                .0
                .get("tid")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .unwrap_or(0);
            if tenant_id > 0 {
                if let Err(message) = tenant_checker
                    .check_tenant_access(tenant_id, &path, &method)
                    .await
                {
                    return rushwind_http_binding::envelope::error_response(forbidden(&message));
                }
            }

            // The authorization stage: one evaluation per role, the
            // first permitting role wins.
            let roles: Vec<String> = claims
                .0
                .get("roc")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let evaluation = AuthzEvaluation {
                user_id: uid,
                tenant_id,
                roles: &roles,
                action: &method,
                resource: &path,
                ip: &ip,
                trace_id: &trace_id,
            };
            if let Err(message) = authorizer.authorize(evaluation).await {
                return rushwind_http_binding::envelope::error_response(forbidden(&message));
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

/// The tenant/authorization stage failure: `FORBIDDEN` (403) with the
/// check's own message text.
fn forbidden(message: &str) -> rushwind_http_binding::envelope::StatusError {
    let status = proto::tables::error_status("admin.service.v1", "FORBIDDEN").unwrap_or(403);
    rushwind_http_binding::envelope::StatusError::new(status, "FORBIDDEN", message)
}

/// Best-effort client IP: `X-Real-IP`, then the first `X-Forwarded-For`
/// entry. The socket peer stays unavailable at this layer.
fn client_ip(headers: &axum::http::HeaderMap) -> String {
    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return ip.to_owned();
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_owned();
        }
    }
    String::new()
}

/// The trace id: `traceparent`'s trace segment (the second field, 32
/// hex chars), else `X-Request-Id` — the same source the audit layer
/// logs, so one request correlates across trails.
fn trace_id(headers: &axum::http::HeaderMap) -> String {
    if let Some(tp) = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let segments: Vec<&str> = tp.split('-').collect();
        if segments.len() >= 2 && segments[1].len() == 32 {
            return segments[1].to_owned();
        }
    }
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn client_ip_prefers_real_ip_then_first_hop() {
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h), "");
        h.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&h), "203.0.113.7");
        h.insert("x-real-ip", "198.51.100.9".parse().unwrap());
        assert_eq!(client_ip(&h), "198.51.100.9");
    }

    #[test]
    fn trace_id_reads_traceparent_then_request_id() {
        let mut h = HeaderMap::new();
        assert_eq!(trace_id(&h), "");
        h.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );
        assert_eq!(trace_id(&h), "0af7651916cd43dd8448eb211c80319c");
        h.remove("traceparent");
        h.insert("x-request-id", "req-42".parse().unwrap());
        assert_eq!(trace_id(&h), "req-42");
    }
}
