//! The upstream-notify verification core (Phase 3b) — the generic form of
//! the per-channel `notifyurl` guard the legacy implemented 60 times over.
//! Given the path channel code and the raw upstream form, it resolves the
//! referenced order, confirms the code names the order's own channel, and
//! hands the message to the registered adapter's `verify_notify` with the
//! order's FROZEN signing snapshot (`orderadd` stored the `key`) — the key
//! that signed the request is the key that verifies its callback.
//!
//! Pure decision, DB reads only: the settle, the risk observation and the
//! merchant outbound notify stay with the [`crate::gateway::handlers`]
//! caller, exactly as they were with the legacy `EditMoney`.

use std::collections::BTreeMap;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::channel::{CallbackReq, ChannelCred, ChannelRegistry, NotifyOk};
use crate::data::{channels, orders};

/// The verification verdict for one `/notify/{code}` POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyCheck {
    /// Verified and the trade is SUCCESSFUL — settle, then reply `ack`.
    Proceed { order_id: String, ack: String },
    /// Verified but the trade is NOT successful — reply `ack` (the legacy
    /// `exit('trade fail')` family), move no money, keep no retry pressure.
    AckOnly(String),
    /// Unverifiable — unknown order, channel-code mismatch, missing adapter,
    /// bad signature, order-id mismatch. Reply `FAIL`; nothing settles.
    Reject,
}

/// Runs the full verification chain for one upstream notify.
pub async fn check_notify(
    db: &DatabaseConnection,
    registry: &ChannelRegistry,
    code: &str,
    form: &BTreeMap<String, String>,
) -> NotifyCheck {
    let Some(adapter) = registry.get(code) else {
        tracing::warn!(%code, "notify for an unconfigured channel");
        return NotifyCheck::Reject;
    };
    let req = CallbackReq {
        form: form.clone(),
        raw_body: String::new(),
    };
    // The order id is a channel-specific field (out_trade_no / merReqNo / …);
    // the adapter reads it before the key-bearing order row is in hand.
    let Some(order_id) = adapter.notify_order_id(&req) else {
        tracing::warn!(%code, "notify without its order id field");
        return NotifyCheck::Reject;
    };

    // Resolve the referenced order (for its frozen signing snapshot) and
    // confirm the path code really names this order's channel — a forged
    // notify on a foreign code must never reach the settle.
    let order = match orders::Entity::find()
        .filter(orders::Column::OrderId.eq(&order_id))
        .one(db)
        .await
    {
        Ok(Some(o)) => o,
        Ok(None) => {
            tracing::warn!(%code, order_id, "notify for an unknown order");
            return NotifyCheck::Reject;
        }
        Err(e) => {
            tracing::warn!(%code, order_id, error = ?e, "notify order read failed");
            return NotifyCheck::Reject;
        }
    };
    let channel_code = channels::Entity::find_by_id(order.channel_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.code);
    let code_matches = channel_code
        .as_deref()
        .map(|c| c.eq_ignore_ascii_case(code))
        .unwrap_or(false);
    if !code_matches {
        tracing::warn!(%code, order_id, channel = ?channel_code, "notify code does not name the order's channel");
        return NotifyCheck::Reject;
    }

    // The upstream authentication gate: the adapter owns the rule.
    let cred = ChannelCred {
        sign_key: order.sign_key.clone().unwrap_or_default(),
        ..Default::default()
    };
    let ok: NotifyOk = match adapter.verify_notify(&cred, &req) {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(%code, order_id, error = ?e, "notify signature verification failed");
            return NotifyCheck::Reject;
        }
    };
    if ok.platform_order_id != order_id {
        tracing::warn!(%code, order_id, read = %ok.platform_order_id, "notify order-id mismatch");
        return NotifyCheck::Reject;
    }
    if !ok.success {
        // A verified but failed trade: acknowledged, no money moves.
        return NotifyCheck::AckOnly(ok.ack);
    }
    NotifyCheck::Proceed {
        order_id,
        ack: ok.ack,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_verdicts_debug_clone_and_compare() {
        // The verdict is carried across spawn/log boundaries; pin the derives.
        let a = NotifyCheck::AckOnly("trade fail".into());
        assert_eq!(a, a.clone());
        assert!(!format!("{a:?}").is_empty());
        assert_ne!(
            a,
            NotifyCheck::Reject,
            "AckOnly and Reject drive different replies"
        );
    }
}
