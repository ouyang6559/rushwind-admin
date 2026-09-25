//! The synchronous cashier return — `Pay_<code>_callbackurl` + the
//! `EditMoney(returntype = 1)` branch (`spec/02` §4.2 step 4 / §4.6,
//! `spec/03` §4.1 contract row 2). The upstream bounces the payer's browser
//! back here after payment; the platform does NOT settle or notify from this
//! route — it reads the order and, when the async callback has already
//! settled it (`pay_status <> 0`), renders the `setHtml` auto-submit
//! form POSTing the signed reply to the merchant's `pay_callbackurl`.
//! Status never advances to `2` here (that stays the async ok-reply's
//! verdict, §4.2 step 3), and an unpaid order gets the legacy `error` text
//! — the async notify (plus the reissue sweep) is the crediting path.
//!
//! The wire message is the outbound notify's own: the same six signed
//! fields + `sign` + unsigned `attach` ([`crate::gateway::notify::notify_pairs`])
//! — legacy built ONE `$return_array` and both branches rendered from it
//! (PayController:411-423), so this module reuses the kernel rather than
//! re-deriving pairs, keeping sync and async byte-identical by
//! construction.
//!
//! Two deliberate departures from the PHP, both on the 语义差异清单:
//! * `setHtml` echoed values unescaped (an `attach` containing `"` broke
//!   the merchant page — an XSS surface); the form here attribute-escapes.
//! * `WxSm::callbackurl` passed an uninitialized `$data['out_trade_no']`
//!   into `EditMoney` (variable-scope bug, spec/03 §5 row 2) and `KRY`
//!   `sleep(5)`-polled unpaid orders; the kernel simply reads the request
//!   params once and answers `error` while status is 0.

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::{members, orders};
use crate::gateway::notify::notify_pairs;
use crate::state::GatewayResult;

/// The minimal HTML-attribute escape (the improvement over the legacy raw
/// concatenation): `&` first so the entity `&quot;` is not double-encoded.
pub(crate) fn attr_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `PayController::setHtml` (P:517-529) re-stated: a hidden-input form and
/// the inline `document.Form1.submit()`. Field order is the pairs' order
/// (PHP `foreach` followed insertion), values attribute-escaped.
pub fn auto_form(action_url: &str, pairs: &[(String, String)]) -> String {
    let mut html = format!(
        "<form id=\"Form1\" name=\"Form1\" method=\"post\" action=\"{}\">",
        attr_escape(action_url)
    );
    for (k, v) in pairs {
        html.push_str(&format!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            attr_escape(k),
            attr_escape(v)
        ));
    }
    html.push_str("</form><script>document.Form1.submit();</script>");
    html
}

/// Pick the order id out of an upstream return. Channels name it
/// `out_trade_no` (what the adapter order sent as `merchant_order_id`,
/// the value the dispatch froze); `pay_orderid`/`orderid` are accepted
/// like the legacy `$_REQUEST` reads that varied per channel.
pub fn order_id_of(params: &std::collections::BTreeMap<String, String>) -> Option<&str> {
    ["out_trade_no", "pay_orderid", "orderid"]
        .iter()
        .find_map(|k| params.get(*k).filter(|s| !s.is_empty()))
        .map(|s| s.as_str())
}

/// What the callback route answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// No (or unpaid) order — the legacy `exit("error")` body.
    Error,
    /// Settled: the auto-submit form back to the merchant's page url.
    Form { html: String },
    /// Settled but the order stored no `pay_callbackurl`: nothing to post
    /// to, the WxSm-path plain success text instead of a broken form.
    SuccessText,
}

/// One synchronous return: load the order, gate on `pay_status <> 0`
/// (§4.2 step 4's precondition), render the signed form from the stored
/// merchant apikey. Read-only on money and status — no settle, no notify,
/// no 1→2.
pub async fn callback_core(db: &DatabaseConnection, order_id: &str) -> GatewayResult<Callback> {
    let order = orders::Entity::find()
        .filter(orders::Column::OrderId.eq(order_id))
        .one(db)
        .await?;
    callback_from(db, order).await
}

/// The shared body: [`Option<orders::Model>`] separates "unknown order"
/// from "unpaid order" for the caller's log line while both answer
/// `error` on the wire, exactly like the legacy `getField` lookup.
async fn callback_from(
    db: &DatabaseConnection,
    order: Option<orders::Model>,
) -> GatewayResult<Callback> {
    let Some(order) = order else {
        return Ok(Callback::Error);
    };
    if order.status == crate::ledger::PayStatus::Unpaid.code() {
        return Ok(Callback::Error); // §4.2: this route never credits
    }
    if order.callback_url.is_empty() {
        return Ok(Callback::SuccessText);
    }
    let apikey = members::Entity::find_by_id(order.user_id)
        .one(db)
        .await?
        .and_then(|m| m.apikey)
        .unwrap_or_default();
    let datetime = chrono::Local::now().format("%Y%m%d%H%M%S").to_string();
    let pairs = notify_pairs(&order, &apikey, &datetime);
    Ok(Callback::Form {
        html: auto_form(&order.callback_url, &pairs),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pairs() -> Vec<(String, String)> {
        vec![
            ("memberid".into(), "10062".into()),
            ("orderid".into(), "E1".into()),
            ("sign".into(), "ABC".into()),
        ]
    }

    #[test]
    fn form_mirrors_sethtml_shape_and_field_order() {
        let html = auto_form("https://m.test/return", &pairs());
        assert!(html.starts_with(
            "<form id=\"Form1\" name=\"Form1\" method=\"post\" action=\"https://m.test/return\">"
        ));
        // Insertion order preserved, exactly the PHP foreach.
        let i = html.find("name=\"memberid\"").unwrap();
        let j = html.find("name=\"orderid\"").unwrap();
        let k = html.find("name=\"sign\"").unwrap();
        assert!(i < j && j < k);
        assert!(html.ends_with("</form><script>document.Form1.submit();</script>"));
        assert!(html.contains("<input type=\"hidden\" name=\"orderid\" value=\"E1\">"));
    }

    #[test]
    fn values_and_url_are_attribute_escaped() {
        let mut p = pairs();
        p.push(("attach".into(), "x\"><script>alert(1)</script>".into()));
        let html = auto_form(r#"https://m.test/a"b?q=1&x=2"#, &p);
        assert!(html.contains("action=\"https://m.test/a&quot;b?q=1&amp;x=2\""));
        assert!(!html.contains("value=\"x\"><script>"));
        assert!(html.contains("&quot;"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn order_id_prefers_out_trade_no_then_fallbacks() {
        let mut m = BTreeMap::new();
        assert_eq!(order_id_of(&m), None);
        m.insert("pay_orderid".into(), "".into()); // empty is absent
        assert_eq!(order_id_of(&m), None);
        m.insert("orderid".into(), "O3".into());
        m.insert("pay_orderid".into(), "O2".into());
        assert_eq!(order_id_of(&m), Some("O2")); // out_trade_no missing → next
        m.insert("out_trade_no".into(), "O1".into());
        assert_eq!(order_id_of(&m), Some("O1")); // the adapter-order name wins
    }
}
