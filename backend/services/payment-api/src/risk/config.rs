//! Order-side rule assembly — turning the DB config rows into the
//! [`RuleConfig`]s the [`RiskGate`] screens with (`spec/06` §4, the
//! `setChannelApiControl` / `orderadd` / `userRiskcontrol` call sites).
//!
//! Three subjects, three admission shapes:
//! - **channel** (CRC): controlled + online screens through
//!   trading → amount → daily-total (pay_channel carries no unit fields);
//!   `control_status = 0` passes untouched — even while offline (IXC:159).
//! - **sub-account** (CARC): admission on the ACCOUNT's own switches, but
//!   `is_defined = 0` evaluates on the CHANNEL's rule row (PC:385-388
//!   equivalent), full config or nothing — plus its own unit throttle.
//! - **merchant** (URC): no control column — `findConfigInfo` already hid
//!   `status = 0` rows, and no rule row at all means the whole gate is a
//!   no-op. The chain runs trading → amount → **domain** → daily-total →
//!   unit: the base class only chains trading + amount, the 防封域名 list
//!   sits between them and the total (URC::monitoringData), so the
//!   merchant cannot reuse [`RiskGate::check`]'s fixed order.

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use super::rules::{
    attempt_at, scope_of_amount, total_volume, trading_time, unit_operate, unit_window_secs,
    Attempt, Counters, RuleConfig, UnitRule,
};
use super::{Decision, RiskGate, RuleKind, Scope};
use crate::data::{channel_accounts, channels, user_riskcontrol_configs as urc};
use crate::state::GatewayResult;

/// One subject's screening kit: the admission switches read off the row
/// plus the assembled rule set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screening {
    /// `control_status` (anything but 1 = pass without screening).
    pub control: i32,
    /// `offline_status` (0 while controlled = `已下线`).
    pub offline: i32,
    pub rules: RuleConfig,
}

/// Assemble the four plain rule columns shared by channel and account rows.
fn rules_from(
    start_time: i32,
    end_time: i32,
    min_money: i64,
    max_money: i64,
    all_money: i64,
    unit: Option<UnitRule>,
) -> RuleConfig {
    RuleConfig {
        start_hour: start_time.max(0) as u32,
        end_hour: end_time.max(0) as u32,
        min_amount: min_money,
        max_amount: max_money,
        daily_limit: all_money,
        unit,
    }
}

/// The unit throttle of a self-defined row (0 = every dimension off);
/// shared by accounts and merchant configs.
pub fn unit_rule(interval: i32, time_unit: &str, number: i64, all_money: i64) -> Option<UnitRule> {
    let window_secs = unit_window_secs(interval, time_unit);
    (window_secs > 0).then_some(UnitRule {
        window_secs,
        max_count: number,
        max_amount: all_money,
    })
}

/// CRC kit: `pay_channel` row, no unit family (§2.1).
pub fn channel_screening(ch: &channels::Model) -> Screening {
    Screening {
        control: ch.control_status,
        offline: ch.offline_status,
        rules: rules_from(
            ch.start_time,
            ch.end_time,
            ch.min_money,
            ch.max_money,
            ch.all_money,
            None,
        ),
    }
}

/// CARC kit: admission switches are the account's own, the rule row is
/// inherited wholesale from the channel while `is_defined = 0` — which also
/// switches the unit throttle off (a channel row has none, P:395).
pub fn account_screening(acct: &channel_accounts::Model, ch: &channels::Model) -> Screening {
    let rules = if acct.is_defined == 0 {
        rules_from(
            ch.start_time,
            ch.end_time,
            ch.min_money,
            ch.max_money,
            ch.all_money,
            None,
        )
    } else {
        rules_from(
            acct.start_time,
            acct.end_time,
            acct.min_money,
            acct.max_money,
            acct.all_money,
            unit_rule(
                acct.unit_interval,
                &acct.time_unit,
                acct.unit_number,
                acct.unit_all_money,
            ),
        )
    };
    Screening {
        control: acct.control_status,
        offline: acct.offline_status,
        rules,
    }
}

/// URC kit from a served [`urc::Model`] row.
pub fn merchant_screening(cfg: &urc::Model) -> Screening {
    Screening {
        control: 1,
        offline: 1,
        rules: rules_from(
            cfg.start_time,
            cfg.end_time,
            cfg.min_money,
            cfg.max_money,
            cfg.all_money,
            unit_rule(
                cfg.unit_interval,
                &cfg.time_unit,
                cfg.unit_number,
                cfg.unit_all_money,
            ),
        ),
    }
}

/// URCC::findConfigInfo: the merchant's own enabled rule row when it exists
/// AND is flagged as user-defined (`systemxz = 1`); otherwise the enabled
/// platform row (`is_system = 1`); `None` when neither serves.
pub async fn load_merchant_config(
    db: &DatabaseConnection,
    user_id: i64,
) -> GatewayResult<Option<urc::Model>> {
    let own = urc::Entity::find()
        .filter(urc::Column::UserId.eq(user_id))
        .filter(urc::Column::Status.eq(1))
        .one(db)
        .await?;
    if matches!(&own, Some(c) if c.system_xz == 1) {
        return Ok(own);
    }
    Ok(urc::Entity::find()
        .filter(urc::Column::IsSystem.eq(1))
        .filter(urc::Column::Status.eq(1))
        .one(db)
        .await?)
}

/// URC::controlDomain: a blank whitelist allows any request; otherwise the
/// referer host must equal one `\r\n`-separated entry — a request without a
/// referer matches nothing (the legacy `parse_url` yields no host).
pub fn domain_violation(domain: &str, referer_host: Option<&str>) -> Option<Decision> {
    let list = domain.trim();
    if list.is_empty() {
        return None;
    }
    if list
        .split("\r\n")
        .any(|item| item == referer_host.unwrap_or(""))
    {
        return None;
    }
    Some(Decision::Reject {
        rule: RuleKind::Domain,
        message: "请求域名错误！".to_string(),
    })
}

/// The host of a referer URL (scheme-less forms included), mirroring the
/// legacy `parse_url($_SERVER['HTTP_REFERER'])['host']`.
pub fn referer_host(referer: &str) -> Option<&str> {
    let rest = match referer.find("://") {
        Some(i) => &referer[i + 3..],
        None => referer,
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    (!host.is_empty()).then_some(host)
}

/// Screen one channel/account candidate (CRC / CARC). `None` = admissible.
/// A controlled subject that is offline — by DB flag or the trip's Redis
/// marker — answers `已下线` before the rules run (CRC:19-27); an
/// uncontrolled one passes without either check (IXC:159).
pub async fn screen_subject(
    gate: &RiskGate,
    scope: Scope,
    id: i64,
    s: &Screening,
    amount: i64,
    now_ts: i64,
) -> Option<Decision> {
    if s.control != 1 {
        return None;
    }
    if s.offline != 1 || gate.is_offline(scope, id).await {
        return Some(Decision::Reject {
            rule: RuleKind::Offline,
            message: "已下线".to_string(),
        });
    }
    match gate.check(scope, id, &s.rules, amount, now_ts).await {
        Decision::Pass => None,
        reject => Some(reject),
    }
}

/// The merchant chain (URC::monitoringData): base rules → domain → total →
/// unit, each family answering with the legacy's own wording. A fully
/// disabled config skips the Redis snapshot like [`RiskGate::check`] does.
pub async fn merchant_decision(
    gate: &RiskGate,
    cfg: &urc::Model,
    referer: Option<&str>,
    amount: i64,
    now_ts: i64,
) -> Decision {
    let s = merchant_screening(cfg);
    let req = attempt_at(now_ts, amount);
    if let Some(d) = trading_time(s.rules.start_hour, s.rules.end_hour, req.hour) {
        return d;
    }
    if let Some(d) = scope_of_amount(s.rules.min_amount, s.rules.max_amount, req.amount) {
        return d;
    }
    if let Some(d) = domain_violation(&cfg.domain, referer.and_then(referer_host)) {
        return d;
    }
    if s.rules.is_unlimited() {
        return Decision::Pass;
    }
    let c = gate
        .counters
        .snapshot(Scope::Merchant, cfg.user_id, s.rules.unit, now_ts)
        .await;
    screen_after_scope(&s.rules, &c, &req).unwrap_or(Decision::Pass)
}

/// The tail shared by every chain once the base passed: daily total, then
/// the unit throttle (`spec/06` §3.1 ordering).
fn screen_after_scope(cfg: &RuleConfig, c: &Counters, req: &Attempt) -> Option<Decision> {
    if let Some(d) = total_volume(cfg.daily_limit, c.daily_amount, req.amount) {
        return Some(d);
    }
    if let Some(unit) = cfg.unit {
        if let Some(d) = unit_operate(&unit, c, req.amount) {
            return Some(d);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: i64 = 10_000;

    fn channel() -> channels::Model {
        channels::Model {
            id: 3,
            code: "T".into(),
            title: "t".into(),
            mch_id: None,
            sign_key: None,
            app_id: None,
            app_secret: None,
            gateway: None,
            page_return: None,
            server_return: None,
            default_rate: 0,
            fengding: 0,
            t0_default_rate: 0,
            t0_fengding: 0,
            status: 1,
            paytype: 1,
            unlock_domain: None,
            control_status: 1,
            offline_status: 1,
            all_money: 100 * K,
            start_time: 9,
            end_time: 18,
            min_money: 10 * K,
            max_money: 50 * K,
        }
    }

    fn account(is_defined: i32) -> channel_accounts::Model {
        channel_accounts::Model {
            id: 7,
            channel_id: 3,
            mch_id: None,
            sign_key: None,
            app_id: None,
            app_secret: None,
            title: None,
            weight: 1,
            status: 1,
            default_rate: 0,
            fengding: 0,
            t0_default_rate: 0,
            t0_fengding: 0,
            custom_rate: 0,
            control_status: 1,
            offline_status: 1,
            is_defined,
            all_money: 500 * K,
            unit_interval: 30,
            time_unit: "s".into(),
            unit_number: 5,
            unit_all_money: 200 * K,
            start_time: 8,
            end_time: 20,
            min_money: K,
            max_money: 60 * K,
        }
    }

    #[test]
    fn channel_kit_maps_the_row() {
        let s = channel_screening(&channel());
        assert_eq!((s.control, s.offline), (1, 1));
        assert_eq!(s.rules.start_hour, 9);
        assert_eq!(s.rules.end_hour, 18);
        assert_eq!((s.rules.min_amount, s.rules.max_amount), (10 * K, 50 * K));
        assert_eq!(s.rules.daily_limit, 100 * K);
        assert!(s.rules.unit.is_none(), "pay_channel has no unit fields");
    }

    #[test]
    fn defined_account_keeps_own_rules_including_unit() {
        let s = account_screening(&account(1), &channel());
        assert_eq!(s.rules.start_hour, 8);
        assert_eq!((s.rules.min_amount, s.rules.max_amount), (K, 60 * K));
        assert_eq!(s.rules.daily_limit, 500 * K);
        let unit = s.rules.unit.expect("30s window");
        assert_eq!(
            (unit.window_secs, unit.max_count, unit.max_amount),
            (30, 5, 200 * K)
        );
    }

    #[test]
    fn inheriting_account_screens_on_the_channel_row() {
        let s = account_screening(&account(0), &channel());
        assert_eq!(s.rules, channel_screening(&channel()).rules);
        // Admission switches stay the account's own (PC:75).
        let mut sick = account(0);
        sick.control_status = 0;
        assert_eq!(account_screening(&sick, &channel()).control, 0);
    }

    #[test]
    fn unknown_time_unit_switches_the_throttle_off() {
        let mut a = account(1);
        a.time_unit = "x".into();
        assert!(account_screening(&a, &channel()).rules.unit.is_none());
    }

    #[test]
    fn domain_whitelist_is_exact_host_matching() {
        // blank (the legacy ' ' default trims away) allows anything
        assert!(domain_violation(" ", None).is_none());
        assert!(domain_violation("", Some("a.com")).is_none());
        let list = "a.com\r\nb.com";
        assert!(domain_violation(list, Some("a.com")).is_none());
        assert!(domain_violation(list, Some("c.com")).is_some());
        // no referer at all → matches nothing → rejected
        assert!(domain_violation(list, None).is_some());
        // substrings do not match (the legacy `==`)
        assert!(domain_violation(list, Some("x.a.com")).is_some());
    }

    #[test]
    fn referer_host_parsing() {
        assert_eq!(referer_host("https://a.com/p?x=1"), Some("a.com"));
        assert_eq!(referer_host("http://a.com:8080/p"), Some("a.com:8080"));
        assert_eq!(referer_host("a.com/p"), Some("a.com"));
        assert_eq!(referer_host(""), None);
    }

    #[test]
    fn merchant_kit_carries_every_family() {
        let cfg = urc::Model {
            id: 1,
            user_id: 9,
            min_money: 5 * K,
            max_money: 0,
            all_money: 300 * K,
            start_time: 0,
            end_time: 0,
            unit_interval: 2,
            time_unit: "h".into(),
            unit_number: 20,
            unit_all_money: 0,
            is_system: 0,
            status: 1,
            domain: String::new(),
            system_xz: 1,
        };
        let s = merchant_screening(&cfg);
        assert_eq!(s.rules.min_amount, 5 * K);
        assert_eq!(s.rules.daily_limit, 300 * K);
        assert_eq!(s.rules.unit.map(|u| u.window_secs), Some(7200));
    }
}
