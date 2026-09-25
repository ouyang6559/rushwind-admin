//! The live payout-channel HTTP adapters (`spec/04` §9): concrete
//! [`PayoutExec`] implementations that POST a booked order to an upstream
//! 代付 gateway and normalise its answer back to [`ExecResp`].
//!
//! The adapters are STATELESS — every secret and endpoint rides the
//! [`PayoutChannelCfg`] handed in by the sweep (mirroring the legacy, which
//! read the `pay_for_another` row per order), so a unit struct can be
//! registered once and shared across the batch. Each one owns a single
//! `reqwest::Client` with a fixed timeout so a hung upstream reads as a
//! transport fault ([`ChannelError::Upstream`]) — the legacy's
//! `result === FALSE`, which [`PayoutService::submit_one`] answers by dropping
//! the `df_lock` WITHOUT folding (§8.2).
//!
//! The request-building and the response→[`ExecResp`] normalisation are pure
//! functions so they are offline-unit-testable; the network is exercised by a
//! loopback mock server in `tests/payout_channel.rs`.
//!
//! Two families ship here, covering the dominant 代付 wire styles:
//! - [`mgzf`] 蘑菇支付 — a form-urlencoded POST signed with the sorted
//!   `k=v&…` + secret-suffix MD5 (the [`crate::channel::sign::easy_pay_sign`]
//!   family, already byte-proven against the PHP layout);
//! - [`yibao`] 易宝 — a JSON POST signed `md5(body . '|' . key)` and carried
//!   in an `Api-Sign` header.
//!
//! ShanDe / Kx / AliTransfer are NOT ported here: they sign with X.509 / RSA
//! SDKs whose libraries never landed in this repo (`spec/04` §9.2 flags the
//! ShanDe `Handle` class as missing), so they stay in the pending set exactly
//! as the missing收款 adapters do.

pub mod mgzf;
pub mod yibao;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::data::payout_orders;
use crate::payout::exec::{PayoutExec, PayoutRegistry};

/// The per-request upstream timeout. Long enough for a slow 代付 gateway,
/// short enough that a hung one fails the drive rather than stalling a sweep.
pub(crate) const PAYOUT_TIMEOUT: Duration = Duration::from_secs(15);

/// A shared, cheaply-clonable HTTP client every live adapter posts through.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PAYOUT_TIMEOUT)
        .build()
        .expect("payout reqwest client")
}

/// The payee identity the upstreams key on: OUR platform order no (the
/// legacy's wttklist `orderid`), used as `out_trade_no` / `orderId`.
pub(crate) fn out_trade_no(order: &payout_orders::Model) -> &str {
    &order.order_no
}

/// The bank card / account / name, defaulted to empty where the order has no
/// snapshot (the legacy `trim(null)` rendered empty strings too).
pub(crate) fn bank_of(order: &payout_orders::Model) -> (&str, &str, &str) {
    (
        order.cardnumber.as_deref().unwrap_or_default(),
        order.accountname.as_deref().unwrap_or_default(),
        order.bankname.as_deref().unwrap_or_default(),
    )
}

/// Builds the registry with the built-in live adapters pre-registered, so a
/// sweep driven by `pay_for_another.code` finds [`mgzf::Mgzf`] / [`yibao::Yibao`]
/// without the caller wiring them by hand. Empty codes never collide.
impl PayoutRegistry {
    /// The startup registry: every known built-in adapter, keyed by code.
    pub fn with_known_adapters() -> Self {
        let mut reg = Self::new();
        reg.register(mgzf::Mgzf::new());
        reg.register(yibao::Yibao::new());
        reg
    }

    /// Whether a channel code maps to a built-in adapter (ops diagnostics).
    pub fn is_known(code: &str) -> bool {
        matches!(code.to_ascii_lowercase().as_str(), "mgzf" | "yibao")
    }
}

/// The known built-in adapters as a code → adapter map (a lighter sibling of
/// [`crate::channel::ChannelRegistry::assemble`] — here the set is fixed and
/// small, so the registry just carries them all).
pub fn known_adapters() -> BTreeMap<String, Arc<dyn PayoutExec>> {
    let mut m = BTreeMap::new();
    for a in [
        Arc::new(mgzf::Mgzf::new()) as Arc<dyn PayoutExec>,
        Arc::new(yibao::Yibao::new()) as Arc<dyn PayoutExec>,
    ] {
        m.insert(a.code().to_ascii_lowercase(), a);
    }
    m
}

/// The submit / query URL join helpers, faithful to each gateway's path.
pub(crate) fn join_gateway(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_carries_every_known_adapter() {
        let reg = PayoutRegistry::with_known_adapters();
        assert!(reg.contains("MGZF"));
        assert!(reg.contains("yibao")); // case-insensitive lookup
        assert_eq!(known_adapters().len(), 2);
        assert!(PayoutRegistry::is_known("Mgzf"));
        assert!(!PayoutRegistry::is_known("ShanDe"));
    }

    #[test]
    fn join_gateway_is_path_safe() {
        // The base's trailing slash is trimmed; the caller owns the leading
        // slash of the path (mgzf passes "", yibao "/withdraw/create").
        assert_eq!(
            join_gateway("https://x/mgzf/", "/settle"),
            "https://x/mgzf/settle"
        );
        assert_eq!(join_gateway("https://x", "/a/b"), "https://x/a/b");
        assert_eq!(join_gateway("https://x/gw/", ""), "https://x/gw");
    }
}
