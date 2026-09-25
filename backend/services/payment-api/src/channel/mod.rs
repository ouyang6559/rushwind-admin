//! The channel-adapter contract and its config-driven registry —
//! `spec/03-pay-gateway-channel.md` §13.1 (trait draft) / §13.2 (registry).
//!
//! Every upstream channel implements [`Channel`]. The registry binds a DB
//! `channels.code` to an adapter at startup ([`ChannelRegistry::assemble`]
//! is the pure core; [`ChannelRegistry::load`] drives it from the database),
//! so adding a channel is: implement the trait, register it in
//! [`known_adapter`], and add its `code` row — no gateway edit.
//!
//! Three sample adapters ship here ([`wxsm`], [`rzfkj`], [`aliscan`]) to
//! prove the contract carries real upstream shapes: a 易支付-style aggregate
//! (redirect), a bank quick-pay with a different signing variant (redirect),
//! and a QR-producing scan channel. `pay`/`query` are `async` so a live
//! upstream HTTP round-trip (and the order snapshot built in Phase 3) drops
//! straight in without reshaping the trait.

pub mod aliscan;
pub mod rzfkj;
pub mod sign;
pub mod wxsm;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::channels;

/// Credentials loaded from the selected sub-account / order snapshot
/// (`spec/03` §13.1 `ChannelCred`).
#[derive(Debug, Clone, Default)]
pub struct ChannelCred {
    pub mch_id: String,
    /// The upstream MD5 secret (order `key`).
    pub sign_key: String,
    pub app_id: String,
    pub app_secret: String,
    /// Upstream endpoint override (a channel's `gateway`).
    pub gateway: String,
    /// Anti-ban redirect domain override, when the account sets one.
    pub unlock_domain: Option<String>,
    /// Overrides the generated async-notify URL (`serverreturn`).
    pub server_return: Option<String>,
    /// Overrides the generated sync-callback URL (`pagereturn`).
    pub page_return: Option<String>,
}

/// The order context an adapter needs to build its upstream request. Money
/// stays internal (`amount_units`); each adapter formats 元/分 per its own
/// `exchange` (see [`crate::money`]).
#[derive(Debug, Clone, Default)]
pub struct OrderCtx {
    /// Platform order id (`pay_orderid` / `out_trade_no` / `spbillno`).
    pub order_id: String,
    /// The merchant's own order number (`out_trade_id`).
    pub merchant_order_id: String,
    /// Order amount in internal money units (1/10000 元).
    pub amount_units: i64,
    /// Product name / subject.
    pub subject: String,
    pub notify_url: String,
    pub callback_url: String,
}

/// The bundle handed to [`Channel::pay`] / [`Channel::query`].
pub struct PayCtx<'a> {
    pub order: &'a OrderCtx,
    pub cred: &'a ChannelCred,
}

/// A callback/notify request from upstream (raw form).
#[derive(Debug, Clone, Default)]
pub struct CallbackReq {
    pub form: BTreeMap<String, String>,
    pub raw_body: String,
}

/// What the caller must present to the payer's browser.
#[derive(Debug, Clone)]
pub enum PayOut {
    /// 302 / meta refresh to a URL (`header('Location')`).
    Redirect { url: String },
    /// A QR code image page pointing at this URL (`showQRcode`).
    QrCode { url: String },
    /// An auto-submitting HTML form posting `fields` to `url` (`createForm`).
    AutoForm {
        url: String,
        fields: Vec<(String, String)>,
    },
    /// Raw body emitted verbatim (e.g. a `<script>` location jump).
    Raw(String),
}

/// A rendered browser response.
pub struct Rendered {
    pub content_type: &'static str,
    pub body: String,
}

impl PayOut {
    /// Renders the output into a browser-facing document. The gateway maps
    /// [`Redirect`] to a real 302 when it prefers; the HTML fallback keeps
    /// the surface transport-agnostic (and unit-testable) here.
    pub fn render(&self) -> Rendered {
        match self {
            PayOut::Redirect { url } => Rendered {
                content_type: "text/html; charset=utf-8",
                body: format!(
                    "<!doctype html><meta charset=\"utf8\"><script>location.replace({})</script>",
                    html_script_string(url)
                ),
            },
            PayOut::QrCode { url } => Rendered {
                content_type: "text/html; charset=utf-8",
                body: format!(
                    "<!doctype html><meta charset=\"utf8\"><title>扫码支付</title>\
                     <img id=\"qr\" alt=\"QR\" src=\"{}\">",
                    html_attr(&format!("/qr?data={}", url_encode(url)))
                ),
            },
            PayOut::AutoForm { url, fields } => {
                let mut form = String::from(
                    "<!doctype html><html><head><meta charset=\"utf8\">\
                     <title>正在跳转付款页</title></head>\
                     <body onload=\"document.pay.submit()\">\
                     <form method=\"post\" name=\"pay\" action=\"",
                );
                form.push_str(&html_attr(url));
                form.push_str("\">");
                for (k, v) in fields {
                    form.push_str(&format!(
                        "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                        html_attr(k),
                        html_attr(v)
                    ));
                }
                form.push_str("</form></body></html>");
                Rendered {
                    content_type: "text/html; charset=utf-8",
                    body: form,
                }
            }
            PayOut::Raw(body) => Rendered {
                content_type: "text/html; charset=utf-8",
                body: body.clone(),
            },
        }
    }
}

/// The parsed, signature-verified async-notify result. The gateway performs
/// the idempotent ledger settle from this in Phase 3; the adapter only
/// authenticates the upstream message and reads out the settlement intent.
#[derive(Debug, Clone)]
pub struct NotifyOk {
    /// The platform order id the notify refers to.
    pub platform_order_id: String,
    /// The upstream transaction number (idempotency / dedup key).
    pub upstream_txn: String,
    /// Whether the upstream reports the trade as successful.
    pub success: bool,
    /// The exact body to echo back to upstream (`"success"` / `"SUCCESS"` …).
    pub ack: String,
}

/// The upstream trade state (query).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeState {
    NotPay,
    Success,
    Closed,
    Unknown,
}

/// Adapter failure.
#[derive(Debug, Clone)]
pub enum ChannelError {
    /// The upstream signature did not verify.
    Signature(String),
    /// The message was malformed / missing required fields.
    BadMessage(String),
    /// An upstream call failed or returned an error.
    Upstream(String),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Signature(m) => write!(f, "signature error: {m}"),
            ChannelError::BadMessage(m) => write!(f, "bad message: {m}"),
            ChannelError::Upstream(m) => write!(f, "upstream error: {m}"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// The uniform adapter contract (`spec/03` §13.1).
#[async_trait]
pub trait Channel: Send + Sync {
    /// The channel code, matching `channels.code` (e.g. "WxSm").
    fn code(&self) -> &'static str;

    /// Initiate payment, returning a user-facing [`PayOut`].
    async fn pay(&self, ctx: &PayCtx<'_>) -> Result<PayOut, ChannelError>;

    /// The platform order id an async notify refers to, read out of the raw
    /// upstream form (each channel names its own field — WxSm `out_trade_no`,
    /// Aliscan `merReqNo`, Rzfkj `spbillno`). The gateway loads the referenced
    /// order (its frozen signing snapshot) BEFORE [`Self::verify_notify`] can
    /// run, so this extraction must not need the key. The default reads the
    /// rewrite's own wire convention (`pay_orderid`).
    fn notify_order_id(&self, req: &CallbackReq) -> Option<String> {
        req.form
            .get("pay_orderid")
            .filter(|s| !s.is_empty())
            .cloned()
    }

    /// Parse + verify an async notify (pure — no I/O, no settle), yielding a
    /// [`NotifyOk`]; the gateway settles idempotently in Phase 3.
    fn verify_notify(
        &self,
        cred: &ChannelCred,
        req: &CallbackReq,
    ) -> Result<NotifyOk, ChannelError>;

    /// Optional: query the upstream trade state (a live round-trip lands in
    /// Phase 3; the default reports `Unknown`).
    async fn query(&self, _ctx: &PayCtx<'_>) -> Result<TradeState, ChannelError> {
        Ok(TradeState::Unknown)
    }
}

/// Constructs the built-in adapter for a code (case-insensitive), or `None`
/// when no adapter is implemented for it yet.
fn known_adapter(code: &str) -> Option<Arc<dyn Channel>> {
    match code.to_ascii_lowercase().as_str() {
        "wxsm" => Some(Arc::new(wxsm::WxSm)),
        "rzfkj" => Some(Arc::new(rzfkj::Rzfkj)),
        "aliscan" => Some(Arc::new(aliscan::Aliscan)),
        _ => None,
    }
}

/// The startup-built registry mapping configured channel codes to adapters.
pub struct ChannelRegistry {
    adapters: BTreeMap<String, Arc<dyn Channel>>,
    /// Configured codes that have no adapter implemented yet (surfaced for
    /// ops visibility, not an error).
    missing: Vec<String>,
}

impl ChannelRegistry {
    /// Pure core: for each configured code, instantiate a known adapter.
    pub fn assemble<I, S>(configured: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut adapters = BTreeMap::new();
        let mut missing = Vec::new();
        for code in configured {
            let code = code.as_ref().to_string();
            match known_adapter(&code) {
                Some(a) => {
                    adapters.insert(code.to_ascii_lowercase(), a);
                }
                None => missing.push(code),
            }
        }
        Self { adapters, missing }
    }

    /// Loads the enabled channel codes from the DB and assembles the
    /// registry over them.
    pub async fn load(db: &DatabaseConnection) -> Result<Self, String> {
        let rows = channels::Entity::find()
            .filter(channels::Column::Status.eq(1))
            .all(db)
            .await
            .map_err(|e| format!("channel registry load: {e}"))?;
        Ok(Self::assemble(rows.into_iter().map(|c| c.code)))
    }

    /// The adapter for a channel code (case-insensitive), if configured.
    pub fn get(&self, code: &str) -> Option<Arc<dyn Channel>> {
        self.adapters.get(&code.to_ascii_lowercase()).cloned()
    }

    /// Whether a channel code is configured with a live adapter.
    pub fn contains(&self, code: &str) -> bool {
        self.get(code).is_some()
    }

    /// The registered codes (diagnostics / docs).
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys().map(String::as_str)
    }

    /// Configured codes awaiting an adapter implementation.
    pub fn missing(&self) -> &[String] {
        &self.missing
    }
}

// --- tiny HTML helpers (only what the renderers need) -----------------

fn html_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A JS string literal (double-quoted) safe against `</script>` breakout.
fn html_script_string(s: &str) -> String {
    let esc = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('<', "\\u003c")
        .replace('\n', "\\n");
    format!("\"{esc}\"")
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_assembles_known_and_flags_unknown() {
        let reg = ChannelRegistry::assemble(["WxSm", "Aliscan", "NotYetBuilt"]);
        assert!(reg.contains("wxsm"));
        assert!(reg.contains("AliScan")); // case-insensitive lookup
        assert_eq!(reg.missing(), &["NotYetBuilt".to_string()]);
        assert_eq!(reg.codes().count(), 2);
    }

    #[test]
    fn redirect_render_escapes_script_breakout() {
        let out = PayOut::Redirect {
            url: "https://up/pay?x=1\"></script>".into(),
        }
        .render();
        assert_eq!(out.content_type, "text/html; charset=utf-8");
        // the `<` inside the JS string is neutralised
        assert!(!out.body.contains("\"></script>"));
        assert!(out.body.contains("\\u003c"));
    }

    #[test]
    fn autoform_render_carries_action_and_fields() {
        let out = PayOut::AutoForm {
            url: "https://up/submit".into(),
            fields: vec![("a".into(), "1".into()), ("b".into(), "<x>".into())],
        }
        .render();
        assert!(out.body.contains("action=\"https://up/submit\""));
        assert!(out.body.contains("name=\"a\" value=\"1\""));
        assert!(out.body.contains("value=\"&lt;x&gt;\""));
    }
}
