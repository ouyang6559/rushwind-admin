//! The pure risk-rule engine — the four detection families of the legacy
//! `RiskcontrolLogic` base class (`spec/06` §3): the trading time window
//! (§3.3), the per-transaction amount bounds (§3.4), the same-day accumulated
//! total (§3.5) and the unit-time (s/i/h/d) count+amount throttle (§3.6).
//!
//! Every function is stateless and takes its clock / observed counters as
//! arguments, so the whole engine is offline-unit-testable; the Redis reads
//! that populate [`Counters`] live in [`super::counters`] and the sequencing
//! mirrors `monitoringData` (trading time → amount → daily total → unit
//! throttle). Money bounds and amounts are integer money units
//! (`1/10000 元`), matching [`crate::money`]; a bound of `0` means
//! "unbounded" exactly as the legacy `0` sentinel does.

use crate::risk::{Decision, RuleKind};

/// The unit-time (windowed) throttle — the `unit_interval × time_unit`
/// interval plus its count and amount caps (`spec/06` §2.2). A cap of `0`
/// disables that dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitRule {
    /// Window length in seconds.
    pub window_secs: i64,
    /// Max transactions per window (`unit_number`); 0 = unlimited.
    pub max_count: i64,
    /// Max accumulated amount per window, money units (`unit_all_money`);
    /// 0 = unlimited.
    pub max_amount: i64,
}

/// A fully assembled rule set for one subject (channel / account / merchant).
/// The zero value [`RuleConfig::UNLIMITED`] disables every check, so an
/// unconfigured subject fails open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuleConfig {
    /// Trading window start hour (0 = no window).
    pub start_hour: u32,
    /// Trading window end hour (0 = no window).
    pub end_hour: u32,
    /// Per-transaction minimum, money units (0 = no floor).
    pub min_amount: i64,
    /// Per-transaction maximum, money units (0 = no ceiling).
    pub max_amount: i64,
    /// Same-day accumulated cap, money units (0 = unlimited).
    pub daily_limit: i64,
    /// The unit-time throttle, if any.
    pub unit: Option<UnitRule>,
}

impl RuleConfig {
    /// The all-disabled config (equivalent to [`Default`]).
    pub const UNLIMITED: RuleConfig = RuleConfig {
        start_hour: 0,
        end_hour: 0,
        min_amount: 0,
        max_amount: 0,
        daily_limit: 0,
        unit: None,
    };

    /// Whether every family is disabled (nothing to observe).
    pub fn is_unlimited(&self) -> bool {
        self == &RuleConfig::UNLIMITED
    }
}

/// The observed counter state a check reads (fed from Redis; a Redis miss
/// reads as all-zero, i.e. a fresh window / new day).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Same-day accumulated amount so far, money units.
    pub daily_amount: i64,
    /// Whether the current unit window is open (has trades in it).
    pub window_open: bool,
    /// Transactions counted in the open window.
    pub window_count: i64,
    /// Amount counted in the open window, money units.
    pub window_amount: i64,
}

/// One request being screened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    /// Local hour of the request (0-23) — the legacy `date('H')`.
    pub hour: u32,
    /// Requested amount, money units — `pay_amount`.
    pub amount: i64,
}

/// Builds an [`Attempt`] from a unix timestamp (UTC epoch seconds).
pub fn attempt_at(now_ts: i64, amount: i64) -> Attempt {
    Attempt {
        hour: hour_of(now_ts),
        amount,
    }
}

/// Hour-of-day (0-23) from unix seconds (UTC).
pub fn hour_of(now_ts: i64) -> u32 {
    now_ts.div_euclid(3600).rem_euclid(24) as u32
}

/// §3.3 `tradingTime`: when both bounds are set, admit only
/// `start <= hour <= end`; the legacy does not support a cross-midnight
/// window (the admin form rejects `start > end`). A zero bound disables it.
pub fn trading_time(start: u32, end: u32, hour: u32) -> Option<Decision> {
    if start == 0 || end == 0 {
        return None;
    }
    if hour < start || hour > end {
        Some(Decision::Reject {
            rule: RuleKind::TradingTime,
            message: format!("交易时间段[{start}点-{end}点]"),
        })
    } else {
        None
    }
}

/// §3.4 `scopeOfAmount`: the 4-case `(min, max)` table (`0` = that end
/// unbounded).
pub fn scope_of_amount(min: i64, max: i64, amount: i64) -> Option<Decision> {
    let breach = match (min, max) {
        (0, 0) => false,
        (0, m) => amount > m,
        (n, 0) => amount < n,
        (n, m) => amount < n || amount > m,
    };
    if !breach {
        return None;
    }
    let message = match (min, max) {
        (0, m) => format!("单笔交易最大金额[{m}]"),
        (n, 0) => format!("单笔交易最小金额[{n}]"),
        (n, m) => format!("单笔交易金额范围[{n}-{m}]"),
    };
    Some(Decision::Reject {
        rule: RuleKind::ScopeOfAmount,
        message,
    })
}

/// §3.5 `theTotalVolume`: reject only when the day's total would *exceed* the
/// cap (`all_money < accum + amount`, so hitting the cap exactly is allowed).
pub fn total_volume(limit: i64, accum: i64, amount: i64) -> Option<Decision> {
    if limit != 0 && accum + amount > limit {
        Some(Decision::Reject {
            rule: RuleKind::TheTotalVolume,
            message: "当天总交易金额超额!".to_string(),
        })
    } else {
        None
    }
}

/// §3.6 `unitTimeOperate`: only applies while the window is open (an expired
/// window resets and admits). Allows `max_count` transactions (rejects on the
/// `max_count + 1`-th) and blocks when the window amount would exceed
/// `max_amount`.
pub fn unit_operate(unit: &UnitRule, c: &Counters, amount: i64) -> Option<Decision> {
    if !c.window_open {
        return None;
    }
    if unit.max_count != 0 && c.window_count + 1 > unit.max_count {
        return Some(Decision::Reject {
            rule: RuleKind::UnitTimeOperate,
            message: format!("单位时间最大交易笔数[{}]", unit.max_count),
        });
    }
    if unit.max_amount != 0 && c.window_amount + amount > unit.max_amount {
        return Some(Decision::Reject {
            rule: RuleKind::UnitTimeOperate,
            message: format!("单位时间总交易金额[{}]", unit.max_amount),
        });
    }
    None
}

/// Whether a first-trade timestamp still falls inside the window
/// (`now - first_ts <= window_secs`) — the legacy `time_lag` comparison.
pub fn window_active(first_ts: i64, now_ts: i64, window_secs: i64) -> bool {
    window_secs > 0 && first_ts > 0 && now_ts - first_ts <= window_secs
}

/// The `saveOfflineStatus` trip wire (P:479): the subject goes offline once
/// the running day total *reaches* the cap (`>=`, stricter than the
/// screening side's `>` — recorded in `spec/06` §3.5). A `cap` of 0 never
/// trips (unlimited).
pub fn trips_offline(total_after: i64, cap: i64) -> bool {
    cap > 0 && total_after >= cap
}

/// The `time_unit` switch (RC:119-131): `unit_interval × {s,i,h,d}` in
/// seconds. An interval of 0 disables the throttle; an unknown unit is
/// treated as disabled rather than silently mis-scaled.
pub fn unit_window_secs(interval: i32, time_unit: &str) -> i64 {
    if interval <= 0 {
        return 0;
    }
    match time_unit {
        "s" => interval as i64,
        "i" => interval as i64 * 60,
        "h" => interval as i64 * 3600,
        "d" => interval as i64 * 86_400,
        _ => 0,
    }
}

/// The `monitoringData` chain (`spec/06` §3.1): the first family that breaches
/// decides the outcome; a fully-disabled config passes.
pub fn evaluate(cfg: &RuleConfig, c: &Counters, req: &Attempt) -> Decision {
    if let Some(d) = trading_time(cfg.start_hour, cfg.end_hour, req.hour) {
        return d;
    }
    if let Some(d) = scope_of_amount(cfg.min_amount, cfg.max_amount, req.amount) {
        return d;
    }
    if let Some(d) = total_volume(cfg.daily_limit, c.daily_amount, req.amount) {
        return d;
    }
    if let Some(unit) = cfg.unit {
        if let Some(d) = unit_operate(&unit, c, req.amount) {
            return d;
        }
    }
    Decision::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: i64 = 10_000; // 1 元 in money units

    #[test]
    fn unlimited_config_passes_everything() {
        assert!(RuleConfig::UNLIMITED.is_unlimited());
        assert_eq!(
            evaluate(
                &RuleConfig::UNLIMITED,
                &Counters::default(),
                &attempt_at(0, 1_000 * K)
            ),
            Decision::Pass
        );
    }

    #[test]
    fn trading_window_boundaries() {
        // 9-18 window
        assert!(trading_time(9, 18, 8).is_some());
        assert!(trading_time(9, 18, 9).is_none()); // inclusive start
        assert!(trading_time(9, 18, 18).is_none()); // inclusive end
        assert!(trading_time(9, 18, 19).is_some());
        // a zero bound disables it
        assert!(trading_time(0, 0, 3).is_none());
        assert!(trading_time(9, 0, 3).is_none());
    }

    #[test]
    fn amount_bounds_four_case_table() {
        // both bounds
        assert!(scope_of_amount(10 * K, 100 * K, 5 * K).is_some());
        assert!(scope_of_amount(10 * K, 100 * K, 10 * K).is_none()); // == min ok
        assert!(scope_of_amount(10 * K, 100 * K, 100 * K).is_none()); // == max ok
        assert!(scope_of_amount(10 * K, 100 * K, 101 * K).is_some());
        // ceiling only
        assert!(scope_of_amount(0, 100 * K, 100 * K).is_none());
        assert!(scope_of_amount(0, 100 * K, 101 * K).is_some());
        // floor only
        assert!(scope_of_amount(10 * K, 0, 9 * K).is_some());
        assert!(scope_of_amount(10 * K, 0, 10 * K).is_none());
        // unbounded
        assert!(scope_of_amount(0, 0, 999 * K).is_none());
    }

    #[test]
    fn daily_total_blocks_only_on_strict_excess() {
        // limit 100, already 90, +10 == 100 → allowed (not >)
        assert!(total_volume(100 * K, 90 * K, 10 * K).is_none());
        // +11 → 101 > 100 → blocked
        assert!(total_volume(100 * K, 90 * K, 11 * K).is_some());
        // limit 0 → unlimited
        assert!(total_volume(0, 1_000_000, 1).is_none());
    }

    #[test]
    fn unit_throttle_count_amount_and_closed_window() {
        let unit = UnitRule {
            window_secs: 60,
            max_count: 3,
            max_amount: 50 * K,
        };
        // closed window admits regardless (reset path)
        let closed = Counters {
            window_open: false,
            window_count: 99,
            window_amount: 999 * K,
            ..Default::default()
        };
        assert!(unit_operate(&unit, &closed, 1).is_none());

        // 2 counted, the 3rd allowed (2+1 > 3 false), the 4th blocked
        assert!(unit_operate(
            &unit,
            &Counters {
                window_open: true,
                window_count: 2,
                ..Default::default()
            },
            K
        )
        .is_none());
        assert!(unit_operate(
            &unit,
            &Counters {
                window_open: true,
                window_count: 3,
                ..Default::default()
            },
            K
        )
        .is_some());

        // amount breach
        assert!(unit_operate(
            &unit,
            &Counters {
                window_open: true,
                window_count: 0,
                window_amount: 45 * K,
                ..Default::default()
            },
            6 * K
        )
        .is_some());
    }

    #[test]
    fn evaluate_sequence_reports_first_breach() {
        let cfg = RuleConfig {
            start_hour: 9,
            end_hour: 18,
            min_amount: 10 * K,
            daily_limit: 1000 * K,
            unit: Some(UnitRule {
                window_secs: 60,
                max_count: 1,
                max_amount: 0,
            }),
            ..Default::default()
        };
        // outside the window wins over the (also breaching) amount floor
        let d = evaluate(&cfg, &Counters::default(), &attempt_at(20 * 3600, K));
        assert_eq!(d.rule_kind(), Some(RuleKind::TradingTime));
        // inside the window, the amount floor now decides
        let d = evaluate(&cfg, &Counters::default(), &attempt_at(12 * 3600, K));
        assert_eq!(d.rule_kind(), Some(RuleKind::ScopeOfAmount));
    }

    #[test]
    fn window_active_uses_lag() {
        assert!(window_active(100, 160, 60)); // lag 60 <= 60
        assert!(!window_active(100, 161, 60)); // lag 61 > 60
        assert!(!window_active(0, 100, 60)); // no first trade
    }

    #[test]
    fn trip_fires_at_the_cap_not_beyond_it() {
        // `>=` all_money trips offline (saveOfflineStatus), cap 0 = never.
        assert!(trips_offline(100 * K, 100 * K));
        assert!(trips_offline(101 * K, 100 * K));
        assert!(!trips_offline(99 * K, 100 * K));
        assert!(!trips_offline(i64::MAX, 0));
    }

    #[test]
    fn unit_window_maps_time_units() {
        assert_eq!(unit_window_secs(30, "s"), 30);
        assert_eq!(unit_window_secs(5, "i"), 300);
        assert_eq!(unit_window_secs(2, "h"), 7200);
        assert_eq!(unit_window_secs(1, "d"), 86_400);
        assert_eq!(unit_window_secs(0, "i"), 0); // interval off
        assert_eq!(unit_window_secs(3, "x"), 0); // unknown unit disables
    }

    #[test]
    fn hour_of_wraps_utc() {
        assert_eq!(hour_of(0), 0);
        assert_eq!(hour_of(3600), 1);
        assert_eq!(hour_of(86400 - 1), 23);
        assert_eq!(hour_of(86400 + 5 * 3600), 5);
    }
}
