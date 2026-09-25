//! The merchant outbound notify — the `EditMoney` returntype=0 branch
//! (`spec/02` §4.6, §4.2 step 3). After a settle wins the 0→1 CAS the
//! platform POSTs the settlement reply to the merchant's stored
//! `pay_notifyurl`; a reply body containing `ok` (case-insensitive, the
//! exact legacy `strstr` rule) advances the order 1→2 via
//! [`crate::ledger::LedgerService::mark_order_notified`]. Any other reply —
//! or a transport failure — leaves the order at `1` for the reissue sweep
//! (§7, later round).
//!
//! The wire message is reproduced verbatim: six plain fields signed with
//! the merchant `apikey` through the same [`create_sign`] as the inbound
//! order signature, `sign` appended, `attach` carried but NOT signed
//! (§4.6's note). Amount is rendered 2-dp 元 (`"1.00"` — the merchant's
//! own wire format; the legacy float could print `"1"`, recorded as a
//! deliberate improvement in the 语义差异清单).
//!
//! Like the legacy — which sits outside the settle `if` and re-POSTs even
//! on a duplicate upstream callback — the caller spawns this for EVERY
//! handled notify, settled or already-settled alike; merchants dedupe by
//! orderid, exactly as they did against the PHP.

use std::time::Duration;

use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::data::{members, notify_logs, orders};
use crate::gateway::sign::create_sign;
use crate::ledger::{notify_acked, LedgerService};
use crate::money::units_to_yuan_2dp;
use crate::state::GatewayError;

/// The legacy `curl --max-time 10` on the notify POST.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the exact form pairs POSTed to the merchant (§4.6): the six
/// signed fields, then `sign`, then the unsigned `attach` when present.
/// `datetime` is injected so the message is a pure function (unit-testable
/// against a hand-computed signature).
pub fn notify_pairs(order: &orders::Model, apikey: &str, datetime: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = vec![
        ("memberid".into(), order.mch_id.clone()),
        ("orderid".into(), order.order_id.clone()),
        ("transaction_id".into(), order.order_id.clone()),
        ("amount".into(), units_to_yuan_2dp(order.amount)),
        ("datetime".into(), datetime.to_string()),
        ("returncode".into(), "00".into()),
    ];
    let sign = create_sign(apikey, pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    pairs.push(("sign".into(), sign));
    if let Some(attach) = order.attach.as_deref().filter(|a| !a.is_empty()) {
        pairs.push(("attach".into(), attach.to_string()));
    }
    pairs
}

/// Did the merchant's reply confirm receipt? The verbatim legacy rule is
/// re-exported from the ledger state machine ([`crate::ledger::notify_acked`])
/// so settle-side and notify-side can never drift apart.
pub fn reply_acked(body: &str) -> bool {
    notify_acked(body)
}

/// The outcome of one outbound notify attempt (audit/log vocabulary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// No POST was made: empty `notify_url` or a merchant without an
    /// apikey (legacy would POST to a signed-with-"" URL and fail anyway).
    Skipped(&'static str),
    /// The merchant replied and [`reply_acked`] decided (order 1→2 only on
    /// `true`; `false` keeps it at 1 for the reissue sweep).
    Replied { acked: bool, status: u16 },
    /// Transport failure (timeout / DNS / refused) — order stays at 1.
    Unreachable(String),
}

/// The outbound sender: one shared reqwest client over the pool + ledger.
pub struct MerchantNotifier {
    http: reqwest::Client,
    db: DatabaseConnection,
    ledger: std::sync::Arc<LedgerService>,
}

impl MerchantNotifier {
    pub fn new(db: DatabaseConnection, ledger: std::sync::Arc<LedgerService>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(NOTIFY_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self { http, db, ledger }
    }

    /// Fire-and-forget the notify for one order (the channel callback must
    /// answer upstream immediately; the merchant POST is the legacy's
    /// after-the-response curl, here on its own task).
    pub fn spawn(self: &std::sync::Arc<Self>, order_id: String) {
        let this = self.clone();
        tokio::spawn(async move {
            match this.notify_order(&order_id).await {
                Ok(outcome) => tracing::info!(%order_id, ?outcome, "merchant notify done"),
                Err(e) => tracing::warn!(%order_id, error = ?e, "merchant notify error"),
            }
        });
    }

    /// One synchronous notify round-trip: load order + merchant apikey,
    /// POST the signed reply, and CAS 1→2 on an `ok` acknowledgement.
    pub async fn notify_order(&self, order_id: &str) -> Result<NotifyOutcome, GatewayNotifyError> {
        let order = orders::Entity::find()
            .filter(orders::Column::OrderId.eq(order_id))
            .one(&self.db)
            .await
            .map_err(GatewayNotifyError::Db)?;
        let Some(order) = order else {
            return Ok(NotifyOutcome::Skipped("order not found"));
        };
        if order.notify_url.is_empty() {
            return Ok(NotifyOutcome::Skipped("empty notify_url"));
        }
        let member = members::Entity::find_by_id(order.user_id)
            .one(&self.db)
            .await
            .map_err(GatewayNotifyError::Db)?;
        let apikey = member
            .as_ref()
            .and_then(|m| m.apikey.clone())
            .unwrap_or_default();
        if apikey.is_empty() {
            return Ok(NotifyOutcome::Skipped("merchant without apikey"));
        }

        let datetime = chrono::Local::now().format("%Y%m%d%H%M%S").to_string();
        let pairs = notify_pairs(&order, &apikey, &datetime);
        let notify_str = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let resp = match self.http.post(&order.notify_url).form(&pairs).send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                // The legacy logged the failed curl too (httpCode 0); the
                // reissue history needs the failed attempts as much as the
                // delivered ones.
                self.log_attempt(&order, &notify_str, 0, &msg, false).await;
                return Ok(NotifyOutcome::Unreachable(msg));
            }
        };
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let acked = reply_acked(&body);
        self.log_attempt(&order, &notify_str, status as i32, &body, acked)
            .await;
        // §4.2 step 3: only an `ok` reply advances, and only via the CAS —
        // a concurrent sweep can never double-flip the row.
        if acked {
            let moved = self
                .ledger
                .mark_order_notified(&order.order_id)
                .await
                .map_err(GatewayNotifyError::Ledger)?;
            tracing::info!(order_id = %order.order_id, moved, "merchant notify acked");
        }
        Ok(NotifyOutcome::Replied { acked, status })
    }

    /// Appends one audit row (`spec/02` §4.6, the legacy `log_server_notify`
    /// file line made queryable). Best-effort by design: a log write must
    /// never fail the notify — the outcome (and the 1→2 CAS) already
    /// happened, and a lost audit line is recoverable from the order state.
    async fn log_attempt(
        &self,
        order: &orders::Model,
        notify_str: &str,
        http_code: i32,
        contents: &str,
        acked: bool,
    ) {
        let row = notify_logs::ActiveModel {
            order_id: Set(order.order_id.clone()),
            notify_url: Set(order.notify_url.clone()),
            notify_str: Set(notify_str.to_string()),
            http_code: Set(http_code),
            contents: Set(contents.to_string()),
            acked: Set(acked as i32),
            create_time: Set(crate::data::now_ts()),
            ..Default::default()
        };
        if let Err(e) = row.insert(&self.db).await {
            tracing::warn!(order_id = %order.order_id, error = ?e, "notify log write failed");
        }
    }
}

/// The notify task's failure surface: DB/ledger faults only (a merchant
/// HTTP failure is an [`NotifyOutcome::Unreachable`], never an error).
#[derive(Debug)]
pub enum GatewayNotifyError {
    Db(sea_orm::DbErr),
    Ledger(GatewayError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::sign::md5_upper;

    fn order(attach: Option<&str>) -> orders::Model {
        orders::Model {
            id: 1,
            mch_id: "10062".into(),
            order_id: "E2018121607104235823".into(),
            amount: 1_000_000,
            poundage: 8_000,
            actual_amount: 992_000,
            cost: 8_000,
            apply_date: 0,
            success_date: None,
            bank_code: "903".into(),
            notify_url: "https://m.test/notify".into(),
            callback_url: String::new(),
            status: 1,
            channel_code: None,
            out_trade_id: None,
            num: 0,
            last_reissue_time: 0,
            sign_key: None,
            account: None,
            user_id: 62,
            channel_id: 5,
            account_id: 9,
            t: 0,
            lock_status: 0,
            attach: attach.map(String::from),
            product_name: None,
        }
    }

    #[test]
    fn message_signs_the_six_fields_and_appends_unsigned_attach() {
        let pairs = notify_pairs(&order(Some("meta")), "SECRET", "20260921120000");
        // The signed link, ksort-ed: amount < datetime < memberid <
        // orderid < returncode < transaction_id. `sign` and `attach` are
        // OUT of the signature set (§4.6).
        let link = "amount=100.00&datetime=20260921120000&memberid=10062&\
                    orderid=E2018121607104235823&returncode=00&transaction_id=E2018121607104235823&key=SECRET";
        let want = md5_upper(link.as_bytes());
        let sign = pairs
            .iter()
            .find(|(k, _)| k == "sign")
            .map(|(_, v)| v.clone())
            .expect("sign present");
        assert_eq!(sign, want);
        assert_eq!(
            pairs.last().map(|(k, v)| (k.as_str(), v.as_str())),
            Some(("attach", "meta"))
        );
    }

    #[test]
    fn absent_attach_stays_out_of_the_body() {
        let pairs = notify_pairs(&order(None), "K", "20260921120000");
        assert!(!pairs.iter().any(|(k, _)| k == "attach"));
        // An empty attach string is dropped just like the PHP omitted it.
        let pairs = notify_pairs(&order(Some("")), "K", "20260921120000");
        assert!(!pairs.iter().any(|(k, _)| k == "attach"));
    }

    #[test]
    fn reply_rule_is_the_raw_strstr_ok_substring() {
        assert!(reply_acked("ok"));
        assert!(reply_acked("OK"));
        assert!(reply_acked("Okay, done")); // lowercases to an "ok" prefix — the legacy quirk
        assert!(!reply_acked("fail"));
        assert!(!reply_acked(""));
    }
}
