//! Gateway request handlers. The unified order runs the full Phase-3
//! pipeline: checks → dispatch → 302/QR render (`crate::gateway::dispatch`).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::gateway::callback::{self, Callback};
use crate::gateway::dispatch::{self, DispatchReq};
use crate::gateway::reissue::{self, SweepPolicy};
use crate::gateway::sign::{sign_from_form, verify_sign, ORDER_SIGN_FIELDS};
use crate::gateway::verify;
use crate::ledger::SettleOutcome;
use crate::merchant::{self, MembersRepo};
use crate::money::parse_yuan_to_units;
use crate::risk::{self, Decision};
use crate::state::{AppState, GatewayError};

/// Liveness probe (not part of the legacy contract).
pub async fn health() -> &'static str {
    "ok"
}

/// `Pay/Index/index` — the unified order entry. Runs the signature check
/// (real) and the merchant/risk pre-checks, then hands off to the channel
/// dispatcher (Phase 3).
pub async fn unified_order(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let get = |k: &str| form.get(k).map(|s| s.as_str()).unwrap_or("");

    // Step 1: required params (firstCheckParams, `spec/03` §2.2).
    for field in [
        "pay_memberid",
        "pay_orderid",
        "pay_amount",
        "pay_bankcode",
        "pay_notifyurl",
        "pay_callbackurl",
        "pay_md5sign",
    ] {
        if get(field).is_empty() {
            return legacy_error(&format!("参数 {field} 不能为空"));
        }
    }
    let memberid = get("pay_memberid");
    let order_id = get("pay_orderid");
    let bank_code = get("pay_bankcode");

    // Resolve the merchant (mch id = member.id + 10000).
    let user_id = match memberid
        .parse::<i64>()
        .ok()
        .and_then(merchant::user_id_of_mch)
    {
        Some(u) => u,
        None => return legacy_error("商户号错误"),
    };
    let member = match MembersRepo::new(&state.db).by_id(user_id).await {
        Ok(Some(m)) => m,
        Ok(None) => return legacy_error("商户不存在"),
        Err(e) => return internal_error(format!("db: {e}")),
    };
    if member.status != 1 {
        return legacy_error("商户已被冻结");
    }
    let apikey = match member.apikey.as_deref() {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => return legacy_error("商户未配置密钥"),
    };

    // Step 2: verify the MD5 signature over the seven signed fields.
    let computed = sign_from_form(&form, &ORDER_SIGN_FIELDS, &apikey);
    if !verify_sign(get("pay_md5sign"), &computed) {
        return legacy_error("签名验证失败");
    }

    // Step 3: amount sanity + the merchant screen (URC, `spec/06` §4.3):
    // the served rule row (own `systemxz = 1` or the platform fallback)
    // gates trading window → bounds → domain → day total → unit throttle.
    // No served row = the whole gate is a no-op, exactly like the legacy's
    // `findConfigInfo() === false`.
    let amount_units = match parse_yuan_to_units(get("pay_amount")) {
        Some(u) => u,
        None => return legacy_error("金额错误"),
    };
    let now_ts = chrono::Utc::now().timestamp();
    let merchant_cfg = match risk::config::load_merchant_config(&state.db, user_id).await {
        Ok(c) => c,
        Err(e) => return internal_error(format!("db: {e:?}")),
    };
    if let Some(cfg) = &merchant_cfg {
        let referer = headers.get(header::REFERER).and_then(|v| v.to_str().ok());
        if let Decision::Reject { message, .. } =
            risk::config::merchant_decision(&state.risk, cfg, referer, amount_units, now_ts).await
        {
            return legacy_error(&message);
        }
    }

    // Step 4: channel dispatch — product/routing/account pick, order store,
    // upstream pay, 302/QR render (`spec/03` §2.3–2.4, §4.2).
    let req = DispatchReq {
        user_id,
        order_id: order_id.to_string(),
        amount_units,
        bank_code: bank_code.to_string(),
        notify_url: get("pay_notifyurl").to_string(),
        callback_url: get("pay_callbackurl").to_string(),
        product_name: opt(get("pay_productname")),
        attach: opt(get("pay_attach")),
    };
    match dispatch::dispatch_core(
        &state.db,
        &state.ledger,
        &state.channels,
        &state.cfg.site_url,
        Some(&state.risk),
        req,
    )
    .await
    {
        Ok((_order, payout)) => dispatch::payout_response(payout),
        Err(GatewayError::BadRequest(m)) => legacy_error(&m),
        Err(e) => internal_error(dispatch::dispatch_error(e)),
    }
}

/// `Pay/Trade/query` — the merchant order query (`spec/00` §4, legacy
/// `TradeController::query`). The full legacy guard chain: member id parse →
/// merchant row (its apikey signs the exchange) → the two-field signature →
/// the order lookup keyed on the merchant's own number. The reply echoes the
/// legacy field set — `memberid` / `orderid` / `amount` (4-decimal 元) /
/// `time_end` (`Y-m-d H:i:s`, epoch 0 for an unpaid order) / `transaction_id`
/// / `returncode:"00"` / `trade_state` — signed with the merchant's key
/// (`createSign` over the sorted reply pairs). Every guard failure answers
/// the legacy `showmessage` envelope verbatim.
pub async fn trade_query(
    State(state): State<Arc<AppState>>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let get = |k: &str| form.get(k).map(|s| s.as_str()).unwrap_or("");
    let memberid = get("pay_memberid");
    let order_id = get("pay_orderid");
    if order_id.is_empty() {
        return legacy_error("不存在的交易订单号.");
    }
    let Some(user_id) = memberid
        .parse::<i64>()
        .ok()
        .and_then(merchant::user_id_of_mch)
    else {
        return legacy_error("不存在的商户编号!");
    };
    let member = match MembersRepo::new(&state.db).by_id(user_id).await {
        Ok(Some(m)) => m,
        Ok(None) => return legacy_error("商户不存在"),
        Err(e) => return internal_error(format!("db: {e}")),
    };
    let apikey = match member.apikey.as_deref() {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => return legacy_error("商户未配置密钥"),
    };

    let computed = sign_from_form(&form, &["pay_memberid", "pay_orderid"], &apikey);
    if !verify_sign(get("pay_md5sign"), &computed) {
        return legacy_error("验签失败!");
    }

    // The legacy looked the order up on (pay_memberid, out_trade_id) — the
    // merchant's own number; the rewrite's unique order id IS that number.
    let order = match state.ledger.find_order(order_id).await {
        Ok(Some(o)) if o.user_id == user_id => o,
        Ok(_) => return legacy_error("不存在的交易订单."),
        Err(e) => return internal_error(format!("db: {e:?}")),
    };
    let trade_state = match order.status {
        0 => "NOTPAY",
        1 | 2 => "SUCCESS",
        other => {
            return internal_error(format!("bad order status {other}"));
        }
    };
    // PHP `date('Y-m-d H:i:s', successdate)` — the epoch renders as the
    // local-time epoch string when the order was never paid.
    let time_end = chrono::DateTime::from_timestamp(order.success_date.unwrap_or(0), 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    let mut reply = BTreeMap::new();
    // The legacy echoed the STORED wire member id column, not the request
    // string (a zero-padded request still replies canonical).
    reply.insert("memberid".to_string(), order.mch_id.clone());
    reply.insert("orderid".to_string(), order.order_id.clone());
    reply.insert(
        "amount".to_string(),
        crate::money::units_to_yuan(order.amount),
    );
    reply.insert("time_end".to_string(), time_end);
    reply.insert("transaction_id".to_string(), order.order_id.clone());
    reply.insert("returncode".to_string(), "00".to_string());
    reply.insert("trade_state".to_string(), trade_state.to_string());
    let sign = crate::gateway::sign::sign_form_all(&apikey, &reply);
    reply.insert("sign".to_string(), sign);
    let body: serde_json::Map<String, serde_json::Value> = reply
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    legacy_json(StatusCode::OK, &serde_json::Value::Object(body))
}

/// `Pay_<code>_notifyurl.html` — async upstream notify. The legacy let each
/// channel controller verify its own callback signature before `EditMoney`;
/// the rewrite keeps that gate generic in [`crate::gateway::verify::
/// check_notify`] (adapter-driven order-id extraction, the order's frozen
/// signing snapshot, per-adapter `verify_notify`). A verified SUCCESS settles
/// through the idempotent [`crate::ledger::LedgerService::settle_order`] (the
/// 0->1 CAS dedupes repeat callbacks); every handled callback also spawns the
/// merchant outbound notify (`spec/02` §4.6) — settled or already-settled,
/// matching the legacy `EditMoney`, whose notify section sat outside the
/// settle `if` and re-POSTed on duplicate callbacks.
///
/// Reply discipline (`spec/03` §5): a verified success answers the adapter's
/// ack (`success`); a verified non-success answers its failure ack
/// (`trade fail`); anything unverifiable answers `FAIL` and settles nothing
/// (the Phase-3b security gate: an unsigned POST can never credit a
/// merchant).
pub async fn notify(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let (order_id, ack) = match verify::check_notify(&state.db, &state.channels, &code, &form).await
    {
        verify::NotifyCheck::Reject => {
            return (StatusCode::OK, "FAIL").into_response();
        }
        verify::NotifyCheck::AckOnly(ack) => {
            return (StatusCode::OK, ack).into_response();
        }
        verify::NotifyCheck::Proceed { order_id, ack } => (order_id, ack),
    };

    match state.ledger.settle_order(&order_id).await {
        Ok(outcome) => {
            tracing::info!(%code, order_id, ?outcome, "notify settled");
            // Post-settle risk observation (`spec/06` §5.1, legacy
            // saveOfflineStatus): day/unit buckets + the day-cap offline
            // trip, run only on a FRESH settle — the legacy section sat
            // inside the `pay_status == 0` gate, so an AlreadySettled
            // duplicate never re-counts. Between settle and notify, like
            // the legacy's commit → risk → notify order; failures are
            // logged, never surfaced — a Redis/DB hiccup must not make
            // the upstream keep retrying a callback we already settled.
            if matches!(outcome, SettleOutcome::Settled(_)) {
                observe_settled(&state, &order_id).await;
            }
            state.notifier.spawn(order_id);
            (StatusCode::OK, ack).into_response()
        }
        Err(e) => {
            tracing::warn!(%code, order_id, error = ?e, "notify settle failed");
            (StatusCode::OK, "FAIL").into_response()
        }
    }
}

/// Feed one freshly-settled order into the risk counters. Read-only on the
/// ledger — re-reads the frozen row the settle just CAS-locked to 1.
async fn observe_settled(state: &AppState, order_id: &str) {
    let order = match state.ledger.find_order(order_id).await {
        Ok(Some(order)) => order,
        Ok(None) => {
            tracing::warn!(order_id, "risk observe: order vanished after settle");
            return;
        }
        Err(e) => {
            tracing::warn!(order_id, error = ?e, "risk observe: order read failed");
            return;
        }
    };
    let now_ts = chrono::Utc::now().timestamp();
    if let Err(e) = risk::observe::observe_settlement(&state.db, &state.risk, &order, now_ts).await
    {
        tracing::warn!(order_id, error = ?e, "risk observe failed");
    }
}

// --- response shapers -------------------------------------------------

/// `Pay_<code>_callbackurl.html` — the synchronous cashier return, GET
/// flavor (upstream 302s the browser back with the order in the query).
pub async fn callback_get(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    callback_reply(&state, &code, &query).await
}

/// The same route, POST flavor (form-gated upstream returns ride a
/// urlencoded body; the legacy read `$_REQUEST`, both together).
pub async fn callback_post(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    callback_reply(&state, &code, &form).await
}

async fn callback_reply(
    state: &AppState,
    code: &str,
    params: &BTreeMap<String, String>,
) -> Response {
    let Some(order_id) = callback::order_id_of(params) else {
        return axum::response::Html("error").into_response(); // no id to look up
    };
    match callback::callback_core(&state.db, order_id).await {
        Ok(Callback::Form { html }) => {
            tracing::info!(%code, order_id, "sync callback rendered");
            axum::response::Html(html).into_response()
        }
        Ok(Callback::SuccessText) => {
            tracing::info!(%code, order_id, "sync callback: settled, no callback_url");
            axum::response::Html("交易成功！").into_response()
        }
        Ok(Callback::Error) => {
            tracing::info!(%code, order_id, "sync callback: unknown or unpaid order");
            axum::response::Html("error").into_response()
        }
        Err(e) => {
            tracing::warn!(%code, order_id, error = ?e, "sync callback fault");
            axum::response::Html("error").into_response()
        }
    }
}

/// `Pay/Repost/postUrl` — the cron-hit reissue sweep (`spec/02` §7.1).
/// The legacy answers `ok` first and keeps looping; the spawn reproduces
/// that fire-and-forget shape (the cron only ever reads the `ok`).
pub async fn repost_scan(State(state): State<Arc<AppState>>) -> &'static str {
    let (ledger, notifier) = (state.ledger.clone(), state.notifier.clone());
    tokio::spawn(async move {
        let now = chrono::Local::now().timestamp();
        match reissue::run_sweep(&ledger, &notifier, &SweepPolicy::default(), now).await {
            Ok(report) => tracing::info!(?report, "reissue sweep done"),
            Err(e) => tracing::warn!(error = ?e, "reissue sweep failed"),
        }
    });
    "ok"
}

/// `Pay/Pay/bufa` — the manual single-order repost (`spec/03` §9.2),
/// the admin order page's「补发通知」link. The legacy echoes the 已补发
/// line BEFORE `EditMoney(...,0)` runs; the spawn keeps that order: the
/// operator sees the verdict, the POST completes behind it. No attempt
/// is spent (bufa never touched `num` — the sweep still owns the cap).
pub async fn bufa(
    State(state): State<Arc<AppState>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let get = |k: &str| query.get(k).cloned().unwrap_or_default();
    let (trans_id, tongdao) = (get("TransID"), get("tongdao"));
    match reissue::bufa_admit(&state.ledger, &trans_id).await {
        Ok(true) => {
            let notifier = state.notifier.clone();
            let order_id = trans_id.clone();
            tokio::spawn(async move {
                match notifier.notify_order(&order_id).await {
                    Ok(outcome) => tracing::info!(%order_id, ?outcome, "bufa notify done"),
                    Err(e) => tracing::warn!(%order_id, error = ?e, "bufa notify failed"),
                }
            });
            axum::response::Html(reissue::bufa_text(&trans_id, &tongdao)).into_response()
        }
        Ok(false) => axum::response::Html("补发失败").into_response(),
        Err(e) => {
            tracing::warn!(%trans_id, error = ?e, "bufa gate fault");
            axum::response::Html("补发失败").into_response()
        }
    }
}

/// An optional wire field: empty string means absent (`spec/03` §2.5).
fn opt(v: &str) -> Option<String> {
    (!v.is_empty()).then(|| v.to_string())
}

pub(crate) fn legacy_error(msg: &str) -> Response {
    legacy_json(
        StatusCode::OK,
        &json!({ "status": "error", "msg": msg, "data": {} }),
    )
}

pub(crate) fn internal_error(msg: impl Into<String>) -> Response {
    legacy_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        &json!({ "status": "error", "msg": msg.into(), "data": {} }),
    )
}

pub(crate) fn legacy_json(status: StatusCode, body: &serde_json::Value) -> Response {
    (status, axum::Json(body.clone())).into_response()
}
