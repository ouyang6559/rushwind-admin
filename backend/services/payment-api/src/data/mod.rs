//! The SeaORM data layer: the entity catalog and the shared timestamp
//! helpers. Domain repositories (`MembersRepo`, rate loaders) live beside
//! their domains (`crate::merchant`, `crate::rate`), not here.

mod entities;

pub use entities::{
    articles, attachments, bankcards, blocked_logs, channel_accounts, channels,
    complaints_deposit_rules, complaints_deposits, invite_codes, loginrecords, members,
    money_changes, notify_logs, orders, payout_channels, payout_orders, product_users, products,
    reconciliations, redo_orders, sms_configs, sms_templates, tikuan_configs, tikuan_holidays,
    user_channel_accounts, user_codes, user_rates, user_riskcontrol_configs,
};

/// Wall clock for timestamp columns (naive local, matching the legacy
/// `int` unix columns being superseded by explicit datetime where needed).
pub fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// Current unix seconds (the legacy `int(11)` time columns).
pub fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}
