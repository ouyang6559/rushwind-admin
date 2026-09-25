//! The payment service library face (juhepay rewrite): configuration,
//! the shared runtime state, the SeaORM data layer, the wire-compatible
//! merchant/channel gateway (hand-written axum routes + MD5 signing), the
//! ledger/channel/routing/risk seams, and the assembly transports. The
//! binary (`main.rs`) is a thin assembly entry over this surface.

pub mod backoffice;
pub mod captcha;
pub mod channel;
pub mod config;
pub mod data;
pub mod gateway;
pub mod ledger;
pub mod merchant;
pub mod migration;
pub mod money;
pub mod panel;
pub mod payout;
pub mod rate;
pub mod ratelimit;
pub mod reconcile;
pub mod risk;
pub mod routing;
pub mod server;
pub mod sms;
pub mod state;
pub mod totp;
