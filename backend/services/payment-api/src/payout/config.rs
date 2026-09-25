//! The withdrawal configuration view, its pure validation rules and the thin
//! DB repositories (`spec/04` §2, §3). The rules mirror the legacy
//! `saveClearing` guard chain verbatim; each returns the caller-facing legacy
//! message on breach (`Some`) so the sequence is offline-unit-testable exactly
//! like [`crate::risk::rules`].
//!
//! [`PayoutConfig`] is the modernized, money-unit view of one
//! `tikuan_configs` row; [`choose_effective`] reproduces the personal-vs-system
//! priority merge (the time window is *always* forced from the system row,
//! §3.3 step 3); [`PayoutConfigRepo`] is the only DB-touching piece here.

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect};

use crate::data::{tikuan_configs, tikuan_holidays};
use crate::money::units_to_yuan;
use crate::payout::fee::{FeeKind, FeeRule};
use crate::risk::counters::ymd;

/// The clock-derived context of one withdrawal request (UTC). Built from a
/// unix timestamp so the rules read as pure comparisons over integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTime {
    /// The `Ymd` day key (e.g. `20260920`), for the holiday match.
    pub ymd: String,
    /// Hour of day (0-23).
    pub hour: u32,
    /// Day of week, `0` = Sunday .. `6` = Saturday (legacy `date('w')`).
    pub weekday: u32,
    /// Day of month (1-31, legacy `date('j')`).
    pub day_of_month: u32,
}

impl RequestTime {
    /// Derives the request clock from unix seconds (UTC).
    pub fn from_ts(now_ts: i64) -> Self {
        use chrono::{DateTime, Datelike, Timelike};
        match DateTime::from_timestamp(now_ts, 0) {
            Some(dt) => RequestTime {
                ymd: ymd(now_ts),
                hour: dt.hour(),
                // number_from_sunday is 1..=7; the legacy `date('w')` is 0..=6.
                weekday: dt.weekday().number_from_sunday() - 1,
                day_of_month: dt.day(),
            },
            None => RequestTime {
                ymd: ymd(now_ts),
                hour: 0,
                weekday: 4,
                day_of_month: 1,
            },
        }
    }
}

/// The modernized view of a `tikuan_configs` row (money units + RATE_SCALE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayoutConfig {
    /// Settlement interval tag (`0` T+0, `1` T+1, `7` weekly, `30` monthly).
    pub t1zt: i32,
    /// Withdraw settings enabled.
    pub tkzt: i32,
    /// 0 system rule / 1 user rule.
    pub systemxz: i32,
    /// Single-transaction minimum, money units.
    pub tkzx_money: i64,
    /// Single-transaction maximum, money units.
    pub tkzd_money: i64,
    /// Same-day total cap, money units (0 = unlimited).
    pub dayzd_money: i64,
    /// Same-day count cap (0 = unlimited).
    pub dayzd_num: i64,
    /// Allowed window start hour.
    pub allow_start: u32,
    /// Allowed window end hour (0 = no window).
    pub allow_end: u32,
    /// Per-card same-day cap, money units (0 = disabled).
    pub daycardzd_money: i64,
    /// The fee rule derived from `tktype` / `sxfrate` / `sxffixed` /
    /// `tk_charge_type`.
    pub fee: FeeRule,
}

fn hours(v: i32) -> u32 {
    v.max(0) as u32
}

impl PayoutConfig {
    /// Projects an entity row onto the config view.
    pub fn from_model(m: &tikuan_configs::Model) -> Self {
        PayoutConfig {
            t1zt: m.t1zt,
            tkzt: m.tkzt,
            systemxz: m.systemxz,
            tkzx_money: m.tkzx_money,
            tkzd_money: m.tkzd_money,
            dayzd_money: m.dayzd_money,
            dayzd_num: m.dayzd_num,
            allow_start: hours(m.allow_start),
            allow_end: hours(m.allow_end),
            daycardzd_money: m.daycardzd_money,
            fee: FeeRule {
                kind: if m.tk_type == 1 {
                    FeeKind::Fixed
                } else {
                    FeeKind::Percent
                },
                fixed: m.sxf_fixed,
                rate_scaled: m.sx_rate,
                charge_from_balance: m.tk_charge_type == 1,
            },
        }
    }

    /// The settlement `t` value: `t1zt` when positive else `0` (§2.2 — note
    /// `t0zt` never participates in the legacy).
    pub fn settlement_t(&self) -> i32 {
        if self.t1zt > 0 {
            self.t1zt
        } else {
            0
        }
    }
}

/// The per-request money snapshot the guards compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawRequest {
    /// The requested withdrawal principal (`tkmoney`), money units.
    pub amount: i64,
    /// The merchant's current available balance, money units.
    pub balance: i64,
    /// The card's already-withdrawn total today, money units (0 if N/A).
    pub card_today_sum: i64,
}

/// The merchant's same-day withdrawal roll-up (`tklist` + `wttklist`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DailyState {
    /// Count of withdrawals created today.
    pub today_count: i64,
    /// Sum of `tkmoney` withdrawn today, money units.
    pub today_sum: i64,
}

/// §2.4 — a withdrawal on a blackout day (`Ymd` match) is refused.
pub fn check_holiday(now: &RequestTime, holidays: &[i64]) -> Option<String> {
    if holidays.iter().any(|&h| ymd(h) == now.ymd) {
        Some("节假日暂时无法提款！".to_string())
    } else {
        None
    }
}

/// §2.2 / §3.3 step 5 — the weekly (`t==7`, Monday) and monthly (`t==30`,
/// day 1) settlement-cycle gates. `t==1`/`t==0` impose no calendar rule.
pub fn check_settlement_cycle(t: i32, now: &RequestTime) -> Option<String> {
    match t {
        7 if now.weekday != 1 => Some("只有每周一可以申请提现".to_string()),
        30 if now.day_of_month != 1 => Some("只有每月1号可以申请提现".to_string()),
        _ => None,
    }
}

/// §2.3 — the half-open window `[allow_start, allow_end)`; an `allow_end` of
/// `0` disables the window.
pub fn check_time_window(start: u32, end: u32, hour: u32) -> Option<String> {
    if end != 0 && (start > hour || end <= hour) {
        Some(format!("不在结算时间内，算时间段为 {start}:00 - {end}:00"))
    } else {
        None
    }
}

/// §3.3 step 8 — the single-transaction bounds and the balance sufficiency.
pub fn check_amount(cfg: &PayoutConfig, req: &WithdrawRequest) -> Option<String> {
    if cfg.tkzx_money > 0 && req.amount < cfg.tkzx_money {
        return Some(format!(
            "单笔提现最小金额为{}",
            units_to_yuan(cfg.tkzx_money)
        ));
    }
    if cfg.tkzd_money > 0 && req.amount > cfg.tkzd_money {
        return Some(format!(
            "单笔提现最大金额为{}",
            units_to_yuan(cfg.tkzd_money)
        ));
    }
    if req.balance < req.amount {
        return Some("账户余额不足".to_string());
    }
    None
}

/// §3.3 step 9 — the same-day count and amount caps (a `0` cap is unlimited).
pub fn check_daily(cfg: &PayoutConfig, daily: &DailyState, amount: i64) -> Option<String> {
    if cfg.dayzd_num > 0 && daily.today_count >= cfg.dayzd_num {
        return Some("超出当日提款次数！".to_string());
    }
    if cfg.dayzd_money > 0 {
        if daily.today_sum >= cfg.dayzd_money {
            return Some("超出当日提款额度！".to_string());
        }
        if daily.today_sum + amount > cfg.dayzd_money {
            let remaining = cfg.dayzd_money - daily.today_sum;
            return Some(format!(
                "提现额度不足！今日剩余 {}",
                units_to_yuan(remaining)
            ));
        }
    }
    None
}

/// §3.3 step 9 — the per-card same-day cap (`daycardzdmoney > 0` only).
pub fn check_card(cfg: &PayoutConfig, card_today_sum: i64, amount: i64) -> Option<String> {
    if cfg.daycardzd_money <= 0 {
        return None;
    }
    if card_today_sum >= cfg.daycardzd_money {
        return Some("该银行卡今日提现已超额！".to_string());
    }
    if card_today_sum + amount > cfg.daycardzd_money {
        let remaining = cfg.daycardzd_money - card_today_sum;
        return Some(format!("银行卡今日剩余额度 {}", units_to_yuan(remaining)));
    }
    None
}

/// The full `saveClearing` guard chain in order; the first breach decides.
/// Returns `Ok(())` when every rule admits the request.
pub fn check_withdrawal(
    cfg: &PayoutConfig,
    req: &WithdrawRequest,
    now: &RequestTime,
    holidays: &[i64],
    daily: &DailyState,
) -> Result<(), String> {
    let checks: [Option<String>; 6] = [
        check_holiday(now, holidays),
        check_settlement_cycle(cfg.settlement_t(), now),
        check_time_window(cfg.allow_start, cfg.allow_end, now.hour),
        check_amount(cfg, req),
        check_daily(cfg, daily, req.amount),
        check_card(cfg, req.card_today_sum, req.amount),
    ];
    match checks.into_iter().flatten().next() {
        Some(msg) => Err(msg),
        None => Ok(()),
    }
}

/// The personal-vs-system priority merge (§3.3 step 3): a user rule wins only
/// when it exists and is enabled (`systemxz == 1`); otherwise the system row.
/// Regardless of which wins, the **time window is forced from the system row**.
pub fn choose_effective(
    personal: Option<&tikuan_configs::Model>,
    system: &tikuan_configs::Model,
) -> PayoutConfig {
    let effective = match personal {
        Some(p) if p.systemxz == 1 => p,
        _ => system,
    };
    let mut cfg = PayoutConfig::from_model(effective);
    cfg.allow_start = hours(system.allow_start);
    cfg.allow_end = hours(system.allow_end);
    cfg
}

/// Loads the effective withdrawal configuration for a merchant: the enabled
/// system row (required — its absence means "提款已关闭"), optionally
/// overridden by the enabled personal row, per [`choose_effective`].
pub struct PayoutConfigRepo<'a, C: ConnectionTrait> {
    db: &'a C,
}

impl<'a, C: ConnectionTrait> PayoutConfigRepo<'a, C> {
    pub fn new(db: &'a C) -> Self {
        Self { db }
    }

    /// Resolves the effective config, or `None` when withdrawal is globally
    /// closed (no enabled `issystem = 1` row).
    pub async fn resolve(&self, user_id: i64) -> Result<Option<PayoutConfig>, DbErr> {
        let system = tikuan_configs::Entity::find()
            .filter(tikuan_configs::Column::Issystem.eq(1))
            .filter(tikuan_configs::Column::Tkzt.eq(1))
            .one(self.db)
            .await?;
        let Some(system) = system else {
            return Ok(None);
        };
        let personal = tikuan_configs::Entity::find()
            .filter(tikuan_configs::Column::UserId.eq(user_id))
            .filter(tikuan_configs::Column::Tkzt.eq(1))
            .one(self.db)
            .await?;
        Ok(Some(choose_effective(personal.as_ref(), &system)))
    }
}

/// Loads the platform withdrawal blackout days (`tikuan_holidays.datetimes`).
pub struct HolidayRepo<'a, C: ConnectionTrait> {
    db: &'a C,
}

impl<'a, C: ConnectionTrait> HolidayRepo<'a, C> {
    pub fn new(db: &'a C) -> Self {
        Self { db }
    }

    /// All configured holiday midnight timestamps (at most a year's worth,
    /// matching the legacy `limit(366)`).
    pub async fn load(&self) -> Result<Vec<i64>, DbErr> {
        let rows = tikuan_holidays::Entity::find()
            .limit(366)
            .all(self.db)
            .await?;
        Ok(rows.into_iter().map(|r| r.datetime).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: i64 = 10_000; // 1 元

    fn cfg() -> PayoutConfig {
        PayoutConfig {
            t1zt: 1,
            tkzt: 1,
            systemxz: 0,
            tkzx_money: 10 * K,
            tkzd_money: 1000 * K,
            dayzd_money: 5000 * K,
            dayzd_num: 10,
            allow_start: 9,
            allow_end: 18,
            daycardzd_money: 2000 * K,
            fee: FeeRule {
                kind: FeeKind::Percent,
                fixed: 0,
                rate_scaled: 20_000,
                charge_from_balance: false,
            },
        }
    }

    fn time(hour: u32, weekday: u32, day: u32) -> RequestTime {
        RequestTime {
            ymd: "20260920".into(),
            hour,
            weekday,
            day_of_month: day,
        }
    }

    fn req(amount: i64, balance: i64) -> WithdrawRequest {
        WithdrawRequest {
            amount,
            balance,
            card_today_sum: 0,
        }
    }

    fn model(
        user_id: i64,
        issystem: i32,
        systemxz: i32,
        start: i32,
        end: i32,
    ) -> tikuan_configs::Model {
        tikuan_configs::Model {
            id: user_id,
            user_id,
            t1zt: 1,
            tkzt: 1,
            systemxz,
            issystem,
            tkzx_money: 10 * K,
            tkzd_money: 1000 * K,
            dayzd_money: 5000 * K,
            dayzd_num: 10,
            allow_start: start,
            allow_end: end,
            daycardzd_money: 2000 * K,
            tk_type: 0,
            sx_rate: 20_000,
            sxf_fixed: 0,
            tk_charge_type: 0,
            ..Default::default()
        }
    }

    #[test]
    fn holiday_match_is_by_utc_day() {
        let holidays = vec![1_700_000_000, 1_800_000_000];
        let on = RequestTime::from_ts(1_700_000_000); // lands on the first holiday
        assert_eq!(ymd(holidays[0]), on.ymd, "test precondition");
        assert_eq!(
            check_holiday(&on, &holidays).unwrap(),
            "节假日暂时无法提款！"
        );
        let off = RequestTime {
            ymd: "19990101".into(),
            ..on
        };
        assert!(check_holiday(&off, &holidays).is_none());
    }

    #[test]
    fn weekly_and_monthly_cycle_gates() {
        assert!(check_settlement_cycle(7, &time(12, 1, 15)).is_none()); // Monday ok
        assert!(check_settlement_cycle(7, &time(12, 2, 15)).is_some()); // Tue
        assert!(check_settlement_cycle(30, &time(12, 3, 1)).is_none()); // day 1
        assert!(check_settlement_cycle(30, &time(12, 3, 2)).is_some());
        assert!(check_settlement_cycle(1, &time(12, 5, 20)).is_none()); // T+1: no gate
    }

    #[test]
    fn window_is_half_open() {
        assert!(check_time_window(9, 18, 8).is_some());
        assert!(check_time_window(9, 18, 9).is_none());
        assert!(check_time_window(9, 18, 17).is_none());
        assert!(check_time_window(9, 18, 18).is_some()); // end exclusive
        assert!(check_time_window(9, 0, 3).is_none()); // end 0 = unlimited
    }

    #[test]
    fn amount_bounds_and_balance() {
        let c = cfg();
        assert!(check_amount(&c, &req(9 * K, 100 * K)).is_some()); // below min
        assert!(check_amount(&c, &req(1001 * K, 5000 * K)).is_some()); // above max
        assert!(check_amount(&c, &req(100 * K, 50 * K)).is_some()); // insufficient balance
        assert!(check_amount(&c, &req(100 * K, 100 * K)).is_none()); // == balance ok
    }

    #[test]
    fn daily_count_and_amount() {
        let c = cfg();
        let at_count = DailyState {
            today_count: 10,
            today_sum: 0,
        };
        assert_eq!(
            check_daily(&c, &at_count, 100 * K).unwrap(),
            "超出当日提款次数！"
        );
        let over = DailyState {
            today_count: 1,
            today_sum: 5000 * K,
        };
        assert_eq!(check_daily(&c, &over, 1).unwrap(), "超出当日提款额度！");
        let spill = DailyState {
            today_count: 1,
            today_sum: 4950 * K,
        };
        let msg = check_daily(&c, &spill, 100 * K).unwrap();
        assert!(msg.starts_with("提现额度不足！今日剩余"), "{msg}");
    }

    #[test]
    fn per_card_cap_only_when_configured() {
        let mut c = cfg();
        assert!(check_card(&c, 1900 * K, 50 * K).is_none());
        assert!(check_card(&c, 1900 * K, 200 * K).is_some());
        c.daycardzd_money = 0; // disabled
        assert!(check_card(&c, 9_999_999, 100 * K).is_none());
    }

    #[test]
    fn guard_chain_reports_first_breach() {
        // holiday precedes the (also breaching) window violation
        let c = cfg();
        let holidays = vec![1_700_000_000];
        let on_holiday = RequestTime {
            ymd: ymd(1_700_000_000),
            hour: 3,
            weekday: 0,
            day_of_month: 14,
        };
        let r = check_withdrawal(
            &c,
            &req(K, K),
            &on_holiday,
            &holidays,
            &DailyState::default(),
        );
        assert_eq!(r.unwrap_err(), "节假日暂时无法提款！");
    }

    #[test]
    fn guard_chain_admits_a_clean_request() {
        let c = cfg();
        let now = time(12, 3, 20);
        let daily = DailyState {
            today_count: 1,
            today_sum: 100 * K,
        };
        assert!(check_withdrawal(&c, &req(100 * K, 500 * K), &now, &[], &daily).is_ok());
    }

    #[test]
    fn user_rule_wins_but_window_stays_system() {
        let system = model(0, 1, 0, 9, 18);
        let personal = model(42, 0, 1, 0, 0);
        let merged = choose_effective(Some(&personal), &system);
        // personal limits adopted...
        assert_eq!(merged.tkzx_money, 10 * K);
        // ...but the time window is forced from the system row
        assert_eq!((merged.allow_start, merged.allow_end), (9, 18));
    }

    #[test]
    fn disabled_user_rule_falls_back_to_system() {
        let system = model(0, 1, 0, 9, 18);
        let personal = model(42, 0, 0, 1, 2); // systemxz = 0 → ignored
        let merged = choose_effective(Some(&personal), &system);
        assert_eq!((merged.allow_start, merged.allow_end), (9, 18));
        // and a personal row with systemxz=0 must not leak its (1,2) window
        assert_ne!((merged.allow_start, merged.allow_end), (1, 2));
    }
}
