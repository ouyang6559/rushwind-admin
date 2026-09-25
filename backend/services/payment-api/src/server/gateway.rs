//! The merchant/channel gateway route pack — the wire-compatible HTTP
//! surface, built as hand-written axum routes (the legacy form +
//! `pay_md5sign` + `Pay_<code>_notifyurl.html` URL shapes are not a
//! protobuf fit; see the plan's dual-API decision). The legacy PATHINFO
//! URLs (`spec/00` §3) map onto clean, byte-stable paths:
//!
//! - `POST /Pay_Index_index`   → unified order
//! - `GET|POST /Pay_Trade_query` → merchant order query
//! - `POST /Payment_Dfpay_add` → downstream payout application (`spec/04` §7)
//! - `GET|POST /Payment_Dfpay_query` → downstream payout poll (§7.5)
//! - `POST /notify/{code}`     → async upstream notify (`Pay_<code>_notifyurl.html`)
//! - `GET|POST /callback/{code}` → sync cashier return (`Pay_<code>_callbackurl.html`)
//! - `GET|POST /Pay_Repost_postUrl` → cron reissue sweep (§7.1, answers `ok`)
//! - `GET /Pay_Pay_bufa`         → manual single-order repost (§9.2)
//! - `GET /health`               → liveness probe (not part of the legacy contract)
//!
//! The whole surface is stateful (`Arc<AppState>`); authentication is the
//! MD5 signature carried in the form, so no auth-gate layer rides here.

use std::sync::Arc;

use axum::routing::{get, post};
use rushwind_bootstrap::{BootstrapError, RouteInput, RouteSurface};

use crate::gateway::dfpay;
use crate::gateway::handlers;
use crate::state::AppState;

/// The gateway route pack registered under
/// `servers[].route_packs[].name: payment-gateway`.
pub fn pack(
    state: Arc<AppState>,
) -> impl Fn(serde_json::Value, RouteInput) -> Result<RouteSurface, BootstrapError> + Send + Sync + 'static
{
    move |_settings, _input| {
        let router = axum::Router::new()
            .route("/health", get(handlers::health))
            .route("/Pay_Index_index", post(handlers::unified_order))
            .route(
                "/Pay_Trade_query",
                get(handlers::trade_query).post(handlers::trade_query),
            )
            .route("/Payment_Dfpay_add", post(dfpay::add))
            .route(
                "/Payment_Dfpay_query",
                get(dfpay::query_get).post(dfpay::query_post),
            )
            .route("/notify/{code}", post(handlers::notify))
            .route(
                "/callback/{code}",
                get(handlers::callback_get).post(handlers::callback_post),
            )
            .route(
                "/Pay_Repost_postUrl",
                get(handlers::repost_scan).post(handlers::repost_scan),
            )
            .route("/Pay_Pay_bufa", get(handlers::bufa))
            .with_state(Arc::clone(&state));
        Ok(RouteSurface::new(router))
    }
}
