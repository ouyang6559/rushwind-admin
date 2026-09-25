//! Post-settle risk observation — the accounting-time counter legs of the
//! legacy `EditMoney`, the section that ran after the money transaction
//! committed and before the merchant notify (`spec/06` §5.1, P:368-404 and
//! the member block P:264-269). [`observe_settlement`] is the one call the
//! notify handler makes on a freshly-settled order; the screening side
//! (pre-order `check`) already lives in [`super::rules`] + [`super::counters`].
//!
//! Legs, with the legacy's exact conditions and money bases:
//! - **channel** (`saveOfflineStatus`): only while online + controlled +
//!   `all_money > 0` — else not even accumulated (P:473) — `+pay_amount`
//!   into the day bucket, offline trip at `>= all_money`;
//! - **sub-account**: the same day leg keyed by the account's own bucket,
//!   but with `is_defined = 0` the whole config row is the CHANNEL's
//!   (P:385-388) — only the accumulated total stays the account's, so an
//!   inheriting account trips on the channel's cap; plus the unit-window
//!   leg (`+1` count, `+pay_actualamount`) whenever `unit_interval` is on,
//!   which an inheriting account never has (pay_channel carries no unit
//!   fields, `spec/06` §2.1);
//! - **merchant**: unconditional `+pay_actualamount` into the day bucket
//!   with no cap and no trip — the legacy member `exp` update always
//!   accumulates and the URC screens at order time instead (P:264-269);
//!   plus the unit-window leg whenever the served rule row
//!   ([`super::config::load_merchant_config`]) carries one.
//!
//! Record divergences: the legacy `bcadd` read-modify-write races
//! (`spec/06` §9.3) are gone — Redis `INCR` answers with the running total
//! the trip reads; the legacy bumps the merchant's unit counters even with
//! no rule row configured, which here stays uncounted — without a window
//! length there is no bucket to count into and nothing screens on it.

use sea_orm::{DatabaseConnection, EntityTrait};

use super::config::{load_merchant_config, unit_rule};
use super::rules::UnitRule;
use super::{RiskGate, Scope};
use crate::data::{channel_accounts, channels, orders};
use crate::state::GatewayResult;

/// The `saveOfflineStatus` admission (P:473): accumulate only while the
/// subject is online, controlled, and carries a positive day cap.
pub fn day_leg_active(control: i32, offline: i32, all_money: i64) -> bool {
    control == 1 && offline == 1 && all_money > 0
}

/// The config row the account's day leg runs on: `is_defined = 0` swaps in
/// the channel's control/offline/cap wholesale (P:385-388) while the
/// accumulation stays keyed by the account.
pub fn account_day_config(
    acct: &channel_accounts::Model,
    ch: Option<&channels::Model>,
) -> (i32, i32, i64) {
    if acct.is_defined == 0 {
        let Some(ch) = ch else {
            // No channel row to inherit — the legacy would read a null
            // config and fail the admission; mirror that by disabling.
            return (0, 0, 0);
        };
        return (ch.control_status, ch.offline_status, ch.all_money);
    }
    (acct.control_status, acct.offline_status, acct.all_money)
}

/// The account's unit throttle, if lit: only a self-defined account can
/// have one (an inheriting account reads the channel row, which carries no
/// `unit_interval` — P:395 sees it absent), and only a known time unit
/// counts as a window.
pub fn account_unit_config(acct: &channel_accounts::Model) -> Option<UnitRule> {
    if acct.is_defined == 0 {
        return None;
    }
    unit_rule(
        acct.unit_interval,
        &acct.time_unit,
        acct.unit_number,
        acct.unit_all_money,
    )
}

/// What one observation run did — `*_counted` marks an admitted leg,
/// `*_tripped` one whose day total reached the cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    pub channel_counted: bool,
    pub channel_tripped: bool,
    pub account_counted: bool,
    pub account_tripped: bool,
    pub account_unit_counted: bool,
    pub merchant_counted: bool,
    pub merchant_unit_counted: bool,
}

/// Feeds one freshly-settled order into the counters, reading the two config
/// rows fresh (like the legacy re-`find`s them after commit). A missing
/// channel / account row skips just that leg — the legacy `find() === false`
/// fell through the admission check the same way.
pub async fn observe_settlement(
    db: &DatabaseConnection,
    gate: &RiskGate,
    order: &orders::Model,
    now_ts: i64,
) -> GatewayResult<Report> {
    let channel = channels::Entity::find_by_id(order.channel_id)
        .one(db)
        .await?;
    let account = channel_accounts::Entity::find_by_id(order.account_id)
        .one(db)
        .await?;

    let mut report = Report::default();

    if let Some(ch) = channel {
        if day_leg_active(ch.control_status, ch.offline_status, ch.all_money) {
            report.channel_counted = true;
            report.channel_tripped = gate
                .count_daily(Scope::Channel, ch.id, order.amount, ch.all_money, now_ts)
                .await;
        }

        if let Some(acct) = account {
            let (control, offline, cap) = account_day_config(&acct, Some(&ch));
            if day_leg_active(control, offline, cap) {
                report.account_counted = true;
                report.account_tripped = gate
                    .count_daily(Scope::ChannelAccount, acct.id, order.amount, cap, now_ts)
                    .await;
            }
            if let Some(unit) = account_unit_config(&acct) {
                report.account_unit_counted = true;
                // The legacy feeds the unit amount bucket the SETTLED net,
                // not the face amount (P:402 `pay_actualamount`).
                gate.count_unit(
                    Scope::ChannelAccount,
                    acct.id,
                    unit,
                    order.actual_amount,
                    now_ts,
                )
                .await;
            }
        }
    }

    // Merchant day bucket: unconditional, uncapped (P:264-268).
    report.merchant_counted = true;
    gate.count_daily(
        Scope::Merchant,
        order.user_id,
        order.actual_amount,
        0,
        now_ts,
    )
    .await;

    // The merchant unit window rides the served URC rule row: the legacy
    // P:264-268 bump only matters while something screens on it, and a
    // window length is what locates the bucket at all.
    if let Some(cfg) = load_merchant_config(db, order.user_id).await? {
        if let Some(unit) = unit_rule(
            cfg.unit_interval,
            &cfg.time_unit,
            cfg.unit_number,
            cfg.unit_all_money,
        ) {
            report.merchant_unit_counted = true;
            gate.count_unit(
                Scope::Merchant,
                order.user_id,
                unit,
                order.actual_amount,
                now_ts,
            )
            .await;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(
        is_defined: i32,
        control: i32,
        offline: i32,
        all_money: i64,
    ) -> channel_accounts::Model {
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
            control_status: control,
            offline_status: offline,
            is_defined,
            all_money,
            unit_interval: 0,
            time_unit: "s".into(),
            unit_number: 0,
            unit_all_money: 0,
            start_time: 0,
            end_time: 0,
            min_money: 0,
            max_money: 0,
        }
    }

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
            all_money: 500,
            start_time: 0,
            end_time: 0,
            min_money: 0,
            max_money: 0,
        }
    }

    #[test]
    fn admission_needs_online_controlled_and_a_positive_cap() {
        assert!(day_leg_active(1, 1, 1));
        assert!(!day_leg_active(0, 1, 1), "not controlled");
        assert!(!day_leg_active(1, 0, 1), "already offline");
        assert!(!day_leg_active(1, 1, 0), "cap 0 = no counting at all");
    }

    #[test]
    fn inheriting_account_runs_on_the_channels_row() {
        let ch = channel();
        // Own config says off/uncapped; the channel's says counting at 500.
        let inherited = acct(0, 0, 0, 0);
        assert_eq!(account_day_config(&inherited, Some(&ch)), (1, 1, 500));
        let defined = acct(1, 0, 0, 0);
        assert_eq!(account_day_config(&defined, Some(&ch)), (0, 0, 0));
        // Nothing to inherit → the leg stays shut.
        assert_eq!(account_day_config(&inherited, None), (0, 0, 0));
    }

    #[test]
    fn unit_throttle_needs_own_config_and_a_known_window() {
        let mut defined = acct(1, 1, 1, 0);
        assert!(account_unit_config(&defined).is_none(), "interval 0");
        defined.unit_interval = 5;
        defined.time_unit = "i".into();
        defined.unit_number = 3;
        defined.unit_all_money = 50_000;
        let unit = account_unit_config(&defined).expect("5i window");
        assert_eq!(unit.window_secs, 300);
        assert_eq!((unit.max_count, unit.max_amount), (3, 50_000));
        // An inheriting account never sees a unit_interval (P:395).
        let inheriting = acct(0, 1, 1, 0);
        assert!(account_unit_config(&inheriting).is_none());
    }
}
