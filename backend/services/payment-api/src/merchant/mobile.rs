//! Merchant mobile bind / change — the port of
//! `User/AccountController::bindMobile / bindMobileShow / editMobile /
//! editMobileShow` (`spec/05` §10), built on the [`crate::sms`] verification
//! kernel. Unlike the profile / bank-card / password writes (whose SMS factor
//! is gated on the `sms_is_open` seam and so passes straight through), the
//! mobile write is gated by a **delivered SMS code unconditionally**, so every
//! `*_confirm` here verifies that code before touching the row — with no
//! provider wired the code can never be received, keeping the flow
//! fail-closed (§10 security note in the module decision memory).
//!
//! `editMobile` is a two-step old→new machine carried by the [`SmsCodes`]
//! phase flag (the legacy `session('editmobile')` state):
//! 1. send to the member's OLD mobile → confirm → phase advances (nothing
//!    written yet), replying the `editOldMobile` sentinel so the console moves
//!    to the new-phone step;
//! 2. send to the NEW mobile → confirm → the mobile is written and the phase
//!    clears, replying `editNewMobile`.
//!
//! The `editMobile` step is the only one the legacy wraps in the auth-failure
//! limiter (`check_auth_error(uid, 2)` = [`AuthKind::MerchantSms`]); binding has
//! no such counter, mirrored here. Every fn takes the primitives (not
//! `AppState`) so the DB-gated tests drive them directly.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::members;
use crate::merchant::apikey::lockout_msg;
use crate::merchant::MembersRepo;
use crate::ratelimit::{AuthKind, AuthLimiter};
use crate::sms::{self, SmsCodes};
use crate::state::{GatewayError, GatewayResult};

/// The legacy `send()` call index (template code) for binding.
const CALL_BIND: &str = "bindMobile";
/// The legacy `send()` call index for the two-step change.
const CALL_EDIT: &str = "editMobile";

/// The `editMobile` first-step reject when the member has no bound mobile
/// (`editMobile` L797).
pub const MSG_NO_BOUND_MOBILE: &str = "您未绑定手机号码！";
/// The `editMobile` second-step reject when the new mobile is blank
/// (`editMobile` L792).
pub const MSG_MOBILE_EMPTY: &str = "手机号码不能为空！";

/// The outcome of a `*_send` (issue) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// A code was issued and dispatched (legacy `{status: 1}`).
    Sent,
    /// `editMobile` step one but the member has no bound mobile.
    EmptyOld,
    /// `editMobile` step two with a blank new mobile.
    EmptyNew,
}

/// The outcome of `bindMobileShow` (a code-gated single write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// The code verified and the mobile was written; `status` is the affected
    /// row count (legacy `{status: $res}` — 0 if the member vanished).
    Saved { status: u64 },
    /// The code was missing / wrong / expired — nothing written.
    BadCode,
}

/// The outcome of `editMobileShow` (the limiter-wrapped two-step machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// The auth-failure limiter is tripped; `msg` carries the `sec2Time` window.
    Locked { msg: String },
    /// The code was wrong / expired — a failure was recorded, nothing written.
    BadCode,
    /// Step one done: the OLD mobile's code verified, the phase advanced.
    OldVerified,
    /// Step two done: the NEW mobile's code verified and the mobile written.
    Saved { status: u64 },
}

/// `bindMobile` (L780-785): issues a `bindMobile` code to `mobile` and
/// dispatches it. Faithfully performs no blank-number guard (legacy sends to
/// whatever the form posts); the write happens later in [`bind_confirm`].
pub async fn bind_send(sms: &SmsCodes, uid: i64, mobile: &str) -> GatewayResult<SendOutcome> {
    let code = sms
        .issue(CALL_BIND, uid)
        .await
        .map_err(GatewayError::Internal)?;
    sms::dispatch(mobile, &code);
    Ok(SendOutcome::Sent)
}

/// `bindMobileShow` POST (L249-257): verify the `bindMobile` code, and only on
/// a hit write `mobile`. A wrong / absent code rejects without writing; a
/// vanished member folds to `Saved { status: 0 }` (the legacy 0-row `save`).
pub async fn bind_confirm(
    db: &DatabaseConnection,
    sms: &SmsCodes,
    uid: i64,
    code: &str,
    mobile: &str,
) -> GatewayResult<BindOutcome> {
    if !sms.verify(CALL_BIND, uid, code).await {
        return Ok(BindOutcome::BadCode);
    }
    let res = members::Entity::update_many()
        .col_expr(
            members::Column::Mobile,
            Expr::value(Some(mobile.to_string())),
        )
        .filter(members::Column::Id.eq(uid))
        .exec(db)
        .await?;
    Ok(BindOutcome::Saved {
        status: res.rows_affected,
    })
}

/// `editMobile` (L787-802): decide the target from the phase — step two sends
/// to the posted NEW mobile (blank → [`SendOutcome::EmptyNew`]), step one to the
/// member's bound mobile (none → [`SendOutcome::EmptyOld`]) — then issue and
/// dispatch an `editMobile` code.
pub async fn edit_send(
    db: &DatabaseConnection,
    sms: &SmsCodes,
    uid: i64,
    new_mobile: &str,
) -> GatewayResult<SendOutcome> {
    if sms.edit_phase(uid).await {
        if new_mobile.is_empty() {
            return Ok(SendOutcome::EmptyNew);
        }
        let code = sms
            .issue(CALL_EDIT, uid)
            .await
            .map_err(GatewayError::Internal)?;
        sms::dispatch(new_mobile, &code);
        return Ok(SendOutcome::Sent);
    }
    let bound = MembersRepo::new(db)
        .by_id(uid)
        .await?
        .and_then(|m| m.mobile)
        .filter(|m| !m.is_empty());
    match bound {
        None => Ok(SendOutcome::EmptyOld),
        Some(target) => {
            let code = sms
                .issue(CALL_EDIT, uid)
                .await
                .map_err(GatewayError::Internal)?;
            sms::dispatch(&target, &code);
            Ok(SendOutcome::Sent)
        }
    }
}

/// `editMobileShow` POST (L276-300): the legacy order — lockout gate first (no
/// increment), then the code. A miss records a failure and rejects; a hit
/// clears the counter and advances the machine: step one stores the phase flag
/// ([`EditOutcome::OldVerified`]), step two writes the NEW mobile and clears it
/// ([`EditOutcome::Saved`]). No blank guard here — legacy validates the new
/// number on send, and reaching step two means a code was issued for it.
pub async fn edit_confirm(
    db: &DatabaseConnection,
    sms: &SmsCodes,
    limiter: &AuthLimiter,
    uid: i64,
    code: &str,
    new_mobile: &str,
) -> GatewayResult<EditOutcome> {
    if limiter.is_locked(AuthKind::MerchantSms, uid).await {
        let secs = limiter.retry_after(AuthKind::MerchantSms, uid).await;
        return Ok(EditOutcome::Locked {
            msg: lockout_msg(secs),
        });
    }
    if !sms.verify(CALL_EDIT, uid, code).await {
        limiter.record_fail(AuthKind::MerchantSms, uid).await;
        return Ok(EditOutcome::BadCode);
    }
    limiter.clear(AuthKind::MerchantSms, uid).await;
    if sms.edit_phase(uid).await {
        let res = members::Entity::update_many()
            .col_expr(
                members::Column::Mobile,
                Expr::value(Some(new_mobile.to_string())),
            )
            .filter(members::Column::Id.eq(uid))
            .exec(db)
            .await?;
        sms.clear_edit_phase(uid).await;
        Ok(EditOutcome::Saved {
            status: res.rows_affected,
        })
    } else {
        sms.set_edit_phase(uid).await;
        Ok(EditOutcome::OldVerified)
    }
}
