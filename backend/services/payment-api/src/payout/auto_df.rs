//! The 自动代付 (auto-payout) system parameters (`spec/04` §10.1, legacy
//! `Cli/AutodfController::index`). Every one of them lives on the single
//! `issystem = 1` `tikuan_configs` row: the master switch, the daily run
//! window, the per-order arrival ceiling and the per-merchant same-day count /
//! amount caps.
//!
//! [`AutoDfConfig`] is the money-unit view of those columns; [`AutoDfConfig::in_window`]
//! and [`over_cap`] are pure (offline-unit-testable) reproductions of the
//! legacy's `strtotime` gate and its `>=` per-order skips, and
//! [`AutoDfConfig::submit_gate`] folds them into the [`SubmitGate`] the
//! execution queue drives. The switch + window decide *whether* the sweep runs
//! (the worker checks them before pulling anything); the ceiling rides the
//! pull, the caps ride the per-order loop inside the sweep.
//!
//! [`AutoDfRepo`] is the read/write half over the same row — the back-office
//! settings CRUD (validate-before-write, like
//! [`super::payout_channel::PayoutChannelRepo`]) whose stored columns
//! [`AutoDfConfig::load`] then reads back for the queue.

use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

use crate::data::tikuan_configs;
use crate::payout::exec::SubmitGate;
use crate::state::{GatewayError, GatewayResult};

/// The §10.1 retry valve: pull only orders with `auto_submit_try < 5` (the
/// legacy hard-codes 5; there is no config column for it).
pub const AUTO_SUBMIT_TRY_CAP: i32 = 5;
/// The §10.1 batch size: 10 orders per sweep (`->limit(0,10)`).
pub const AUTO_SUBMIT_LIMIT: u64 = 10;

/// The auto-payout parameters read off the platform (`issystem = 1`) row. All
/// money figures are money units (the legacy's 元 decimals scaled on import).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDfConfig {
    /// Master switch (`auto_df_switch`): the sweep is a no-op when `false`.
    pub switch: bool,
    /// Per-order arrival ceiling (`auto_df_maxmoney`), `None` when `0`.
    pub max_money: Option<i64>,
    /// Daily run-window start (legacy `auto_df_stime`), a raw `HH:MM` string.
    pub stime: String,
    /// Daily run-window end (legacy `auto_df_etime`), a raw `HH:MM` string.
    pub etime: String,
    /// Per-merchant same-day auto count cap (`auto_df_max_count`), `0` = off.
    pub max_count: i64,
    /// Per-merchant same-day auto amount cap (`auto_df_max_sum`), `0` = off.
    pub max_sum: i64,
}

impl AutoDfConfig {
    /// Projects the platform entity row onto the auto-df view. A `0` money
    /// ceiling reads as [`None`] (no filter).
    pub fn from_model(m: &tikuan_configs::Model) -> Self {
        AutoDfConfig {
            switch: m.auto_df_switch != 0,
            max_money: (m.auto_df_maxmoney > 0).then_some(m.auto_df_maxmoney),
            stime: m.auto_df_stime.clone(),
            etime: m.auto_df_etime.clone(),
            max_count: m.auto_df_max_count,
            max_sum: m.auto_df_max_sum,
        }
    }

    /// The all-off default (no `issystem` row present → auto is disabled).
    pub fn disabled() -> Self {
        AutoDfConfig {
            switch: false,
            max_money: None,
            stime: String::new(),
            etime: String::new(),
            max_count: 0,
            max_sum: 0,
        }
    }

    /// Is `now_ts` (unix seconds, local clock) inside the configured run
    /// window? An empty or unparseable `stime` / `etime` means "no time
    /// restriction" — the switch is the only gate then (a deliberate
    /// modernisation over the legacy, which would freeze on a bad `strtotime`;
    /// it matches the `allow_end = 0 → no window` convention already used for
    /// the withdrawal window). With both set the window is inclusive of the
    /// end minute (the legacy's `+59s`), and a start-after-end value is read
    /// as an overnight span.
    pub fn in_window(&self, now_ts: i64) -> bool {
        match (parse_hm(&self.stime), parse_hm(&self.etime)) {
            (Some(start), Some(end)) => {
                let now = minute_of_day(now_ts);
                if start <= end {
                    now >= start && now <= end
                } else {
                    now >= start || now <= end
                }
            }
            _ => true,
        }
    }

    /// The [`SubmitGate`] this config drives the auto sweep with: the fixed
    /// §10.1 valve + batch, the per-order ceiling, and the per-merchant daily
    /// caps (checked inside the sweep).
    pub fn submit_gate(&self) -> SubmitGate {
        SubmitGate {
            try_cap: AUTO_SUBMIT_TRY_CAP,
            max_money: self.max_money,
            limit: AUTO_SUBMIT_LIMIT,
            max_count: self.max_count,
            max_sum: self.max_sum,
        }
    }

    /// Loads the platform (`issystem = 1`) auto-df config; [`AutoDfConfig::disabled`]
    /// when no such row exists.
    pub async fn load<C: ConnectionTrait>(db: &C) -> GatewayResult<Self> {
        let row = tikuan_configs::Entity::find()
            .filter(tikuan_configs::Column::Issystem.eq(1))
            .one(db)
            .await
            .map_err(crate::state::db_err)?;
        Ok(match row {
            Some(m) => AutoDfConfig::from_model(&m),
            None => AutoDfConfig::disabled(),
        })
    }
}

/// The back-office write payload for the platform (`issystem = 1`) auto-payout
/// row — the six §10.1 columns a settings form submits in full. Money figures
/// ride money units (the same representation [`AutoDfConfig`] stores), so a `0`
/// ceiling / cap means "unlimited"; the window strings are raw `HH:MM`
/// (empty = unrestricted). Unlike [`AutoDfConfig`] it carries the concrete
/// integer ceiling (not an `Option`) so a form round-trips losslessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDfSettings {
    /// Master switch (`auto_df_switch`).
    pub switch: bool,
    /// Per-order arrival ceiling, money units (`0` = none).
    pub max_money: i64,
    /// Run-window start, `HH:MM` (empty = unrestricted).
    pub stime: String,
    /// Run-window end, `HH:MM` (empty = unrestricted).
    pub etime: String,
    /// Per-merchant same-day count cap (`0` = unlimited).
    pub max_count: i64,
    /// Per-merchant same-day amount cap, money units (`0` = unlimited).
    pub max_sum: i64,
}

impl AutoDfSettings {
    /// Projects a stored platform row onto the write payload.
    pub fn from_model(m: &tikuan_configs::Model) -> Self {
        AutoDfSettings {
            switch: m.auto_df_switch != 0,
            max_money: m.auto_df_maxmoney,
            stime: m.auto_df_stime.clone(),
            etime: m.auto_df_etime.clone(),
            max_count: m.auto_df_max_count,
            max_sum: m.auto_df_max_sum,
        }
    }

    /// The all-off payload a missing platform row reads as (mirrors
    /// [`AutoDfConfig::disabled`]).
    pub fn off() -> Self {
        AutoDfSettings {
            switch: false,
            max_money: 0,
            stime: String::new(),
            etime: String::new(),
            max_count: 0,
            max_sum: 0,
        }
    }

    /// The exec view this write produces — the same projection
    /// [`AutoDfConfig::from_model`] applies to a stored row (a `0` ceiling
    /// becomes `None`).
    pub fn to_config(&self) -> AutoDfConfig {
        AutoDfConfig {
            switch: self.switch,
            max_money: (self.max_money > 0).then_some(self.max_money),
            stime: self.stime.clone(),
            etime: self.etime.clone(),
            max_count: self.max_count,
            max_sum: self.max_sum,
        }
    }

    /// Rejects a negative money / count bound and a malformed (present but
    /// unparseable) window string — the validate-before-write guard mirroring
    /// [`super::payout_channel::PayoutChannelRepo`]. An empty window is allowed
    /// (unrestricted); a start-after-end span is a legal overnight window.
    pub fn validate(&self) -> Result<(), GatewayError> {
        if self.max_money < 0 {
            return Err(GatewayError::BadRequest("单笔到账上限不能为负".into()));
        }
        if self.max_count < 0 {
            return Err(GatewayError::BadRequest("当日笔数上限不能为负".into()));
        }
        if self.max_sum < 0 {
            return Err(GatewayError::BadRequest("当日总额上限不能为负".into()));
        }
        if !self.stime.trim().is_empty() && parse_hm(&self.stime).is_none() {
            return Err(GatewayError::BadRequest("开始时间格式应为 HH:MM".into()));
        }
        if !self.etime.trim().is_empty() && parse_hm(&self.etime).is_none() {
            return Err(GatewayError::BadRequest("结束时间格式应为 HH:MM".into()));
        }
        Ok(())
    }
}

/// Reads and maintains the platform (`issystem = 1`) auto-payout row, generic
/// over the connection (like [`super::payout_channel::PayoutChannelRepo`] and
/// [`super::config::PayoutConfigRepo`]) so the back-office CRUD rides a
/// transaction. This is the write half the §10.1 sweep's [`AutoDfConfig::load`]
/// reads back; the HTTP surface is deferred with the rest of the platform
/// back-office (Phase 7, JWT + RBAC), exactly as the channel CRUD was.
pub struct AutoDfRepo<'a, C: ConnectionTrait> {
    db: &'a C,
}

impl<'a, C: ConnectionTrait> AutoDfRepo<'a, C> {
    pub fn new(db: &'a C) -> Self {
        Self { db }
    }

    /// The current platform settings; [`AutoDfSettings::off`] when a fresh
    /// install carries no `issystem = 1` row yet.
    pub async fn load(&self) -> GatewayResult<AutoDfSettings> {
        let row = self.system_row().await?;
        Ok(row
            .as_ref()
            .map(AutoDfSettings::from_model)
            .unwrap_or_else(AutoDfSettings::off))
    }

    /// Writes the six columns onto the platform row, creating the
    /// (`user_id = 0`, `issystem = 1`) row on a fresh install so operations are
    /// not pinned to migration defaults. Validates first; window strings are
    /// trimmed and a `0` ceiling / cap is stored verbatim (reads back as
    /// unlimited / open).
    pub async fn save(&self, s: AutoDfSettings) -> GatewayResult<tikuan_configs::Model> {
        s.validate()?;
        let switch = i32::from(s.switch);
        let stime = s.stime.trim().to_string();
        let etime = s.etime.trim().to_string();
        if let Some(m) = self.system_row().await? {
            let mut am: tikuan_configs::ActiveModel = m.into();
            am.auto_df_switch = Set(switch);
            am.auto_df_maxmoney = Set(s.max_money);
            am.auto_df_stime = Set(stime);
            am.auto_df_etime = Set(etime);
            am.auto_df_max_count = Set(s.max_count);
            am.auto_df_max_sum = Set(s.max_sum);
            return Ok(am.update(self.db).await?);
        }
        // No platform row yet: seed a minimal enabled withdrawal row and stamp
        // the auto-df columns. Money bounds default to unlimited (0); the id is
        // left to the sequence.
        let am = tikuan_configs::ActiveModel {
            user_id: Set(0),
            t1zt: Set(0),
            tkzt: Set(1),
            systemxz: Set(0),
            issystem: Set(1),
            tkzx_money: Set(0),
            tkzd_money: Set(0),
            dayzd_money: Set(0),
            dayzd_num: Set(0),
            allow_start: Set(0),
            allow_end: Set(0),
            daycardzd_money: Set(0),
            tk_type: Set(0),
            sx_rate: Set(0),
            sxf_fixed: Set(0),
            tk_charge_type: Set(0),
            auto_df_switch: Set(switch),
            auto_df_maxmoney: Set(s.max_money),
            auto_df_stime: Set(stime),
            auto_df_etime: Set(etime),
            auto_df_max_count: Set(s.max_count),
            auto_df_max_sum: Set(s.max_sum),
            ..Default::default()
        };
        Ok(am.insert(self.db).await?)
    }

    /// The single `issystem = 1` row (the platform invariant
    /// [`AutoDfConfig::load`] assumes).
    async fn system_row(&self) -> GatewayResult<Option<tikuan_configs::Model>> {
        Ok(tikuan_configs::Entity::find()
            .filter(tikuan_configs::Column::Issystem.eq(1))
            .one(self.db)
            .await?)
    }
}

/// The §10.1 per-merchant daily caps, checked the instant before an order is
/// claimed: `true` when either a positive count / amount cap is already met by
/// the merchant's same-day `is_auto = 1` tally (the legacy `>=` skips).
pub fn over_cap(max_count: i64, today_count: i64, max_sum: i64, today_sum: i64) -> bool {
    (max_count > 0 && today_count >= max_count) || (max_sum > 0 && today_sum >= max_sum)
}

/// `HH:MM` → minutes-since-midnight. Trims whitespace; empty or malformed
/// (missing `:`, non-numeric, out of range) is `None`.
fn parse_hm(s: &str) -> Option<i32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (h, m) = s.split_once(':')?;
    let h: i32 = h.trim().parse().ok()?;
    let m: i32 = m.trim().parse().ok()?;
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    Some(h * 60 + m)
}

/// Local minute-of-day for a unix timestamp.
fn minute_of_day(now_ts: i64) -> i32 {
    use chrono::{DateTime, Local, Timelike};
    DateTime::from_timestamp(now_ts, 0)
        .map(|dt| {
            let dt = dt.with_timezone(&Local);
            (dt.hour() * 60 + dt.minute()) as i32
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(switch: i32, maxmoney: i64, s: &str, e: &str, mc: i64, ms: i64) -> AutoDfConfig {
        AutoDfConfig {
            switch: switch != 0,
            max_money: (maxmoney > 0).then_some(maxmoney),
            stime: s.into(),
            etime: e.into(),
            max_count: mc,
            max_sum: ms,
        }
    }

    #[test]
    fn from_model_blanks_zero_ceiling_and_reads_switch() {
        let m = tikuan_configs::Model {
            auto_df_switch: 1,
            auto_df_maxmoney: 0,
            auto_df_stime: "09:00".into(),
            auto_df_etime: "18:00".into(),
            auto_df_max_count: 20,
            auto_df_max_sum: 500_000,
            ..Default::default()
        };
        let c = AutoDfConfig::from_model(&m);
        assert!(c.switch);
        assert_eq!(c.max_money, None, "a 0 ceiling means no filter");
        assert_eq!(c.max_count, 20);
        assert_eq!(c.max_sum, 500_000);
    }

    #[test]
    fn submit_gate_carries_valve_batch_ceiling_and_caps() {
        let g = cfg(1, 100_000, "", "", 3, 900_000).submit_gate();
        assert_eq!(g.try_cap, AUTO_SUBMIT_TRY_CAP);
        assert_eq!(g.limit, AUTO_SUBMIT_LIMIT);
        assert_eq!(g.max_money, Some(100_000));
        assert_eq!(g.max_count, 3);
        assert_eq!(g.max_sum, 900_000);
    }

    #[test]
    fn empty_window_is_unrestricted_at_every_hour() {
        let c = cfg(1, 0, "", "", 0, 0);
        for ts in [0i64, 5 * 3600, 23 * 3600 + 59 * 60] {
            assert!(c.in_window(ts), "no window set → always in, ts={ts}");
        }
    }

    #[test]
    fn same_day_window_is_inclusive_of_the_end_minute() {
        let c = cfg(1, 0, "09:00", "18:30", 0, 0);
        assert!(
            !c.in_window(day_at(8, 59)),
            "one minute before start is out"
        );
        assert!(c.in_window(day_at(9, 0)), "start is inclusive");
        assert!(c.in_window(day_at(18, 30)), "end minute is inclusive");
        assert!(!c.in_window(day_at(18, 31)), "one minute after end is out");
    }

    #[test]
    fn overnight_window_wraps_across_midnight() {
        let c = cfg(1, 0, "22:00", "02:00", 0, 0);
        assert!(c.in_window(day_at(23, 0)), "late-evening tick runs");
        assert!(c.in_window(day_at(1, 0)), "early-morning tick runs");
        assert!(
            !c.in_window(day_at(12, 0)),
            "midday is out of a night window"
        );
    }

    #[test]
    fn malformed_or_partial_window_is_unrestricted() {
        assert!(cfg(1, 0, "not-a-time", "18:00", 0, 0).in_window(day_at(3, 0)));
        assert!(cfg(1, 0, "09:00", "", 0, 0).in_window(day_at(3, 0)));
        assert!(cfg(1, 0, "99:99", "18:00", 0, 0).in_window(day_at(3, 0)));
    }

    #[test]
    fn over_cap_skips_at_the_legacy_ge_boundary() {
        // count cap 3: two today → allowed, three → skip.
        assert!(!over_cap(3, 2, 0, 0));
        assert!(over_cap(3, 3, 0, 0));
        // amount cap 900k, sum 900k already met → skip.
        assert!(!over_cap(0, 0, 900_000, 899_999));
        assert!(over_cap(0, 0, 900_000, 900_000));
        // both caps off → never skip.
        assert!(!over_cap(0, 999, 0, 999_999_999));
    }

    #[test]
    fn parse_hm_accepts_trimmed_hh_mm_only() {
        assert_eq!(parse_hm(" 09:05 "), Some(9 * 60 + 5));
        assert_eq!(parse_hm(""), None);
        assert_eq!(parse_hm("09"), None);
        assert_eq!(parse_hm("24:00"), None);
        assert_eq!(parse_hm("09:60"), None);
    }

    #[test]
    fn settings_validate_accepts_full_and_open_forms() {
        let full = AutoDfSettings {
            switch: true,
            max_money: 100_000,
            stime: "09:00".into(),
            etime: "18:00".into(),
            max_count: 3,
            max_sum: 900_000,
        };
        assert!(full.validate().is_ok());
        // An empty / whitespace window is unrestricted, not a format error; an
        // overnight span (start after end) is a legal window.
        let open = AutoDfSettings {
            stime: String::new(),
            etime: "   ".into(),
            ..full.clone()
        };
        assert!(open.validate().is_ok());
        let overnight = AutoDfSettings {
            stime: "22:00".into(),
            etime: "02:00".into(),
            ..full
        };
        assert!(overnight.validate().is_ok());
    }

    #[test]
    fn settings_validate_rejects_negative_bounds_and_bad_clock() {
        let base = AutoDfSettings::off();
        assert!(AutoDfSettings {
            max_money: -1,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(AutoDfSettings {
            max_count: -1,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(AutoDfSettings {
            max_sum: -1,
            ..base.clone()
        }
        .validate()
        .is_err());
        // A present-but-unparseable window is rejected (empty is allowed).
        assert!(AutoDfSettings {
            stime: "25:00".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(AutoDfSettings {
            etime: "9am".into(),
            ..base
        }
        .validate()
        .is_err());
    }

    #[test]
    fn settings_round_trip_model_projection_and_config_view() {
        let m = tikuan_configs::Model {
            auto_df_switch: 1,
            auto_df_maxmoney: 100_000,
            auto_df_stime: "09:00".into(),
            auto_df_etime: "18:00".into(),
            auto_df_max_count: 3,
            auto_df_max_sum: 900_000,
            ..Default::default()
        };
        let s = AutoDfSettings::from_model(&m);
        assert!(s.switch);
        // The write payload keeps the concrete integer (a form round-trips it).
        assert_eq!(s.max_money, 100_000);
        assert_eq!(s.to_config(), AutoDfConfig::from_model(&m));
        // A 0 ceiling stays 0 on the settings but projects to None on the config.
        let zero = AutoDfSettings::from_model(&tikuan_configs::Model::default());
        assert!(!zero.switch);
        assert_eq!(zero.max_money, 0);
        assert_eq!(zero.to_config().max_money, None);
    }

    /// A local-clock unix timestamp for `HH:MM` on an arbitrary day (the
    /// window test only reads minute-of-day, so the date is irrelevant).
    fn day_at(h: u32, m: u32) -> i64 {
        use chrono::Local;
        let today = Local::now().date_naive();
        let ndt = today
            .and_hms_opt(h, m, 30)
            .expect("valid wall time")
            .and_local_timezone(Local)
            .single()
            .expect("unambiguous local time (test host tz)");
        ndt.timestamp()
    }
}
