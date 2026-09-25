//! The merchant / channel gateway — the wire-compatible HTTP surface,
//! registered as hand-written axum routes (not proto codegen). It maps the
//! legacy PATHINFO URLs (`spec/00` §3) onto handlers:
//!
//! - unified order .......... `Pay/Index/index`  (`pay_md5sign`)
//! - order query ............ `Pay/Trade/query`
//! - sync callback .......... `Pay_<code>_callbackurl.html`
//! - async notify ........... `Pay_<code>_notifyurl.html`
//!
//! Phase 0 wired the signature verification end to end; Phase 3 adds the
//! real settle and the channel dispatch tail.

pub mod callback;
pub mod dfpay;
pub mod dispatch;
pub mod handlers;
pub mod notify;
pub mod reissue;
pub mod sign;
pub mod verify;
