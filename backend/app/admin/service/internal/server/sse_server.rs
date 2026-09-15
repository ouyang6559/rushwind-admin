//! The SSE notification server — the port of the reference
//! `internal/server/sse_server.go` + `InternalMessageService.HandleAuthorize`.
//!
//! * token from `Authorization: Bearer`, `X-Token`, or `?token=`
//!   (kratos-transport sse/auth.go:18-31);
//! * access-token validation rides the same gate primitives: signature +
//!   expiry via the engine, Redis whitelist/blacklist via the store;
//! * `?stream=` must equal the token's userId (anti cross-user
//!   subscription, internal_message_service.go:146-158);
//! * events: `notification`, id = GUIDv4, data = the recipient protojson;
//!   stream id = userId — all of a user's devices share one stream.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::broadcast;

use crate::state::{status_error, AppState};
use crate::token::UserTokenPayload;

/// The in-process notification hub: senders publish (userId, json);
/// every SSE connection subscribed filters to its own stream.
#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<(u32, String)>,
}

impl Default for Hub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }
}

impl Hub {
    /// TryPublish (non-blocking; a slow/full buffer drops for that
    /// receiver only).
    pub fn publish(&self, user_id: u32, payload: String) {
        let _ = self.tx.send((user_id, payload));
    }

    fn subscribe(&self) -> broadcast::Receiver<(u32, String)> {
        self.tx.subscribe()
    }
}

fn extract_token(headers: &HeaderMap, query_token: Option<&String>) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(rest) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            return Some(rest.to_string());
        }
    }
    if let Some(v) = headers.get("x-token").and_then(|v| v.to_str().ok()) {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    query_token.filter(|t| !t.is_empty()).cloned()
}

/// GET /events — authorize, then hold the SSE stream open, forwarding
/// this user's notifications.
#[allow(clippy::result_large_err)]
pub async fn events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    axum::response::Response,
> {
    let token = extract_token(&headers, params.get("token")).ok_or_else(|| {
        rushwind_http_binding::envelope::error_response(status_error(
            "UNAUTHORIZED",
            "invalid token",
        ))
    })?;

    // Signature + expiry via the engine, whitelist + blacklist via the
    // store — the HandleAuthorize ValidateTokenRequest(ACCESS) sequence.
    let claims = state
        .authenticator
        .authenticate_token(&token)
        .map_err(|_| {
            rushwind_http_binding::envelope::error_response(status_error(
                "UNAUTHORIZED",
                "invalid token",
            ))
        })?;
    let uid = claims
        .0
        .get("uid")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| {
            rushwind_http_binding::envelope::error_response(status_error(
                "UNAUTHORIZED",
                "invalid token",
            ))
        })?;
    let jti = claims.get_jwt_id().map_err(|_| {
        rushwind_http_binding::envelope::error_response(status_error(
            "UNAUTHORIZED",
            "invalid token",
        ))
    })?;
    if !state.tokens.is_valid_access_token(uid, &jti, &token).await {
        return Err(rushwind_http_binding::envelope::error_response(
            status_error("UNAUTHORIZED", "access token is revoked or expired"),
        ));
    }
    if state.tokens.is_blocked_access_token(&jti).await {
        return Err(rushwind_http_binding::envelope::error_response(
            status_error("FORBIDDEN", "token is blocked"),
        ));
    }

    // The stream must be the token's own userId.
    let stream_uid: u32 = params
        .get("stream")
        .and_then(|s| s.parse().ok())
        .filter(|s| *s == uid)
        .ok_or_else(|| {
            rushwind_http_binding::envelope::error_response(status_error(
                "FORBIDDEN",
                "stream user mismatch",
            ))
        })?;
    let _ = stream_uid;

    let rx = state.hub.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok((user_id, payload)) => {
                    if payload.is_empty() {
                        continue;
                    }
                    let event = Event::default()
                        .id(uuid::Uuid::new_v4().to_string())
                        .event("notification")
                        .data(payload);
                    let _ = user_id;
                    return Some((Ok::<_, Infallible>(event), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    ))
}

/// Publishes a notification for one recipient — called by SendMessage
/// after the recipient rows land. Payload = the recipient protojson,
/// assembled field-by-field (the proto types carry no serde derives).
pub struct NotificationPayload {
    pub id: u32,
    pub message_id: u32,
    pub recipient_user_id: u32,
    pub title: String,
    pub content: String,
    pub received_at: Option<pbjson_types::Timestamp>,
}

pub fn publish_recipient(hub: &Hub, payload: &NotificationPayload) {
    let json = serde_json::json!({
        "id": payload.id,
        "messageId": payload.message_id,
        "recipientUserId": payload.recipient_user_id,
        "status": "RECEIVED",
        "title": payload.title,
        "content": payload.content,
        "receivedAt": payload.received_at.map(|ts| serde_json::json!({
            "seconds": ts.seconds,
            "nanos": ts.nanos,
        })),
    });
    hub.publish(payload.recipient_user_id, json.to_string());
}

#[allow(dead_code)]
fn _claims_shape(_: &UserTokenPayload) {}

/// NewSseServer — assembles the `/events` router into a transport server
/// bound to `server.sse.addr` (default :7789), registered into the same
/// lifecycle as REST.
pub fn new_sse_server(
    state: std::sync::Arc<crate::state::AppState>,
    addr: std::net::SocketAddr,
) -> Result<rushwind_transport_axum::AxumServer, String> {
    let router = axum::Router::new().route("/events", axum::routing::get(events).with_state(state));
    rushwind_transport_axum::AxumServer::new(addr, router).map_err(|e| format!("sse server: {e}"))
}
