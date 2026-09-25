//! The merchant / agent panel HTTP surface (`spec/04` §6.3–§6.5,
//! `User/WithdrawalController`). Unlike the wire-compatible gateway (which a
//! downstream merchant authenticates with an in-form `pay_md5sign`), the panel
//! is a logged-in web console: the legacy gates every action behind
//! `session('user_auth')` (`isLogin`). We keep that shape — a SERVER-SIDE
//! session stored in Redis, issued on the panel login and replayed on each
//! call via a bearer token — which is the faithful port of PHP's server-side
//! session (and, unlike a stateless JWT, lets a logout / single-sign-on kick
//! revoke a live session immediately, mirroring `session_random`).
//!
//! The one fund-moving action wired here is the downstream payout-API review
//! batch (§6.5 `dfPassBatch` / `dfRejectBatch`); it is scoped to the acting
//! merchant so a console can only ever move its OWN `df_api_order` rows.

pub mod handlers;
pub mod session;
