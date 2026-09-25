//! Risk control — the four rule families and the three-level gate
//! (`spec/06-risk-control-route.md`, `spec/00` §7).
//!
//! The detection logic is split into a *pure* engine ([`rules`]) that turns a
//! config, an observed counter snapshot and an attempt into a [`Decision`],
//! and a Redis-backed counter store ([`counters`]) that materialises that
//! snapshot. [`RiskGate`] sequences the two: read the buckets, run
//! [`rules::evaluate`], and (post-settle) record the trade — mirroring the
//! legacy `userRiskcontrol` / `setChannelApiControl` call sites. The daily
//! offline-reset cron ([`offline`]) keeps only the legacy plan's DB
//! restore; the counter zeroing is absorbed by the self-expiring buckets.

pub mod config;
pub mod counters;
pub mod observe;
pub mod offline;
pub mod rules;

pub use counters::{RiskCounters, Scope};
pub use rules::{Attempt, Counters, RuleConfig, UnitRule};

/// A single risk rule dimension (`RiskcontrolLogic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// Allowed trading window (start/end hours of day).
    TradingTime,
    /// Per-transaction amount bounds.
    ScopeOfAmount,
    /// Current-day accumulated amount.
    TheTotalVolume,
    /// Count + amount within a unit interval (s/i/h/d).
    UnitTimeOperate,
    /// The subject is offline (DB flag or the trip's Redis marker) while
    /// controlled — the legacy `已下线` branch, ahead of the rule chain.
    Offline,
    /// The request's referer host is not on the merchant's 防封域名 list
    /// (URC::controlDomain, between the amount and total legs).
    Domain,
}

/// The risk gate outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Pass,
    /// Rejected by `rule` with the caller-facing `message`.
    Reject {
        rule: RuleKind,
        message: String,
    },
}

impl Decision {
    /// The breaching [`RuleKind`], or `None` when the request passed.
    pub fn rule_kind(&self) -> Option<RuleKind> {
        match self {
            Decision::Pass => None,
            Decision::Reject { rule, .. } => Some(*rule),
        }
    }

    /// The caller-facing message of a rejection, `None` on a pass.
    pub fn message(&self) -> Option<String> {
        match self {
            Decision::Pass => None,
            Decision::Reject { message, .. } => Some(message.clone()),
        }
    }
}

/// The orchestrator tying the pure engine to the Redis counters. Cheap to
/// clone (shares the connection manager through [`RiskCounters`]).
#[derive(Clone)]
pub struct RiskGate {
    counters: RiskCounters,
}

impl RiskGate {
    pub fn new(redis: redis::aio::ConnectionManager) -> Self {
        Self {
            counters: RiskCounters::new(redis),
        }
    }

    /// Screens one request for `scope`/`id` against `cfg`. An unconfigured
    /// subject fails open (no Redis read); otherwise the current counters are
    /// snapshotted at `now_ts` and fed through [`rules::evaluate`].
    pub async fn check(
        &self,
        scope: Scope,
        id: i64,
        cfg: &RuleConfig,
        amount: i64,
        now_ts: i64,
    ) -> Decision {
        if cfg.is_unlimited() {
            return Decision::Pass;
        }
        let counters = self.counters.snapshot(scope, id, cfg.unit, now_ts).await;
        let attempt = rules::attempt_at(now_ts, amount);
        rules::evaluate(cfg, &counters, &attempt)
    }

    /// Day accumulation + the `>= cap` offline trip — the
    /// `saveOfflineStatus` leg (`spec/06` §5.1): add `amount` to the
    /// subject's running day total and, once the total reaches `cap`
    /// (0 = unlimited), raise the offline marker until midnight. Returns
    /// whether this trade tripped it. A Redis failure fails open: no
    /// accumulation is observable and nothing trips.
    pub async fn count_daily(
        &self,
        scope: Scope,
        id: i64,
        amount: i64,
        cap: i64,
        now_ts: i64,
    ) -> bool {
        let Some(total) = self.counters.incr_daily(scope, id, amount, now_ts).await else {
            return false;
        };
        if !rules::trips_offline(total, cap) {
            return false;
        }
        self.counters
            .set_offline(scope, id, counters::secs_to_end_of_day(now_ts))
            .await;
        true
    }

    /// The unit-window side of a settled trade (count +1, amount `+n` into
    /// the current fixed window), skipped while the throttle is disabled.
    pub async fn count_unit(
        &self,
        scope: Scope,
        id: i64,
        unit: UnitRule,
        amount: i64,
        now_ts: i64,
    ) {
        self.counters
            .incr_unit(scope, id, unit, amount, now_ts)
            .await;
    }

    /// Whether the subject currently carries an offline marker (the probe
    /// the routing side will read; lives here for callers holding a gate
    /// rather than the raw counters).
    pub async fn is_offline(&self, scope: Scope, id: i64) -> bool {
        self.counters.is_offline(scope, id).await
    }
}
