//! Merchant Google-authenticator binding — the port of
//! `User/AccountController::google / unbindGoogle` (`spec/05` §10), the write
//! side that closes the loop with [`crate::merchant::twofactor::verify_google`]
//! (which reads the very `google_secret_key` column produced here). It rides
//! the [`crate::totp`] TOTP kernel and a Redis holder for the *pending*
//! (not-yet-confirmed) secret, faithfully replacing the legacy
//! `session('user_google_secret_key')`.
//!
//! Flow: [`bind_initiate`] mints (or reuses) a pending secret — the DB is left
//! untouched; [`bind_confirm`] requires a live pending secret AND a valid TOTP
//! code, then persists it **only while the member is still unbound** (the
//! legacy `where(google_secret_key = '')`). [`unbind`] simply clears the column
//! (no TOTP, no unconditional gate — the legacy guards it only behind the SMS
//! factor).
//!
//! Faithfulness / scope (registered as a decision memory):
//! - the legacy SMS branch (`send.saveProfile` / `send.unbindGoogle`) sits
//!   behind `smsStatus()`; the [`crate::merchant::twofactor::sms_status`] seam
//!   is `false`, so it is skipped entirely (no [`crate::sms`] needed here);
//! - legacy `google()` mis-manages the `auth_type = 4` counter (clears on a
//!   WRONG code, records nothing on a RIGHT one) — this port uses the
//!   consistent orchestration shared by every other factor (record on wrong /
//!   clear on success, gate first) so the lockout is meaningful and a bind-page
//!   miss never wipes profile/withdrawal's accumulated failures;
//! - the pending secret + counter are keyed by `uid` (legacy: per-browser PHP
//!   session); a shared multi-session merchant shares them — accepted;
//! - QR-image rendering is a frontend concern (like the captcha image), so
//!   [`bind_initiate`] returns the secret plus a plain `otpauth://` URI.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::members;
use crate::merchant::apikey::lockout_msg;
use crate::merchant::MembersRepo;
use crate::ratelimit::{AuthKind, AuthLimiter};
use crate::state::{GatewayError, GatewayResult};
use crate::totp;

/// How long an un-confirmed pending secret survives — stands in for the legacy
/// PHP session lifetime (`user_google_secret_key`); 30 min is ample to scan
/// and confirm.
pub const PENDING_TTL_SECS: u64 = 1800;

/// The empty-code reject (`google()` L913).
pub const MSG_CODE_EMPTY: &str = "请输入验证码";
/// The no-pending-secret reject (`google()` L917 `$this->error`).
pub const MSG_NO_PENDING: &str = "绑定失败，请刷新页面重试";
/// The wrong-code reject (`google()` L921 — note: no trailing `！`).
pub const MSG_CODE_WRONG: &str = "谷歌安全码错误";
/// The bind success message (`google()` L926).
pub const MSG_BIND_OK: &str = "绑定成功";
/// The unbind success message (`unbindGoogle()` L961).
pub const MSG_UNBIND_OK: &str = "解绑成功";

fn pending_key(uid: i64) -> String {
    format!("payment:google:pending:{uid}")
}

/// The Redis holder for a merchant's pending (un-confirmed) Google secret,
/// replacing the legacy `session('user_google_secret_key')`. Cheap to clone.
#[derive(Clone)]
pub struct PendingSecrets {
    redis: ConnectionManager,
}

impl PendingSecrets {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }

    /// The stored pending secret, if any (and unexpired).
    pub async fn get(&self, uid: i64) -> Option<String> {
        let mut conn = self.redis.clone();
        conn.get::<_, Option<String>>(pending_key(uid))
            .await
            .unwrap_or(None)
    }

    /// Stores (or refreshes) the pending secret under [`PENDING_TTL_SECS`].
    pub async fn put(&self, uid: i64, secret: &str) {
        let mut conn = self.redis.clone();
        let _: Result<(), _> = conn
            .set_ex::<_, _, ()>(pending_key(uid), secret, PENDING_TTL_SECS)
            .await;
    }

    /// Drops the pending secret (flow confirmed or abandoned).
    pub async fn clear(&self, uid: i64) {
        let mut conn = self.redis.clone();
        let _: Result<i64, _> = conn.del(pending_key(uid)).await;
    }
}

/// The outcome of [`bind_initiate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Initiate {
    /// Not yet bound — the pending secret to enroll (plus its `otpauth://` URI).
    Secret { secret: String, otpauth: String },
    /// The member already has a bound secret.
    AlreadyBound,
}

/// The outcome of [`bind_confirm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindResult {
    /// The `auth_type = 4` limiter is tripped; `msg` carries the `sec2Time` window.
    Locked { msg: String },
    /// No code posted (legacy `{0,'请输入验证码'}`).
    EmptyCode,
    /// No pending secret to confirm against (legacy error page).
    NoPending,
    /// The TOTP code failed to verify; a failure was recorded, nothing written.
    BadCode,
    /// Verified and persisted; `status` is the affected-row count (0 on an
    /// already-bound race — the legacy still replies `绑定成功` either way).
    Bound { status: u64 },
}

/// The outcome of [`unbind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnbindResult {
    /// The secret column was cleared (legacy `{1,'解绑成功'}`).
    Unbound,
}

/// `google()` GET (L873-889): mint or reuse the pending secret for an unbound
/// member (the DB is untouched). An already-bound member gets
/// [`Initiate::AlreadyBound`] and its active secret is deliberately not
/// re-served.
pub async fn bind_initiate(
    db: &DatabaseConnection,
    pending: &PendingSecrets,
    uid: i64,
) -> GatewayResult<Initiate> {
    let member = MembersRepo::new(db)
        .by_id(uid)
        .await?
        .ok_or_else(|| GatewayError::BadRequest("账户不存在".to_string()))?;
    let bound = !member
        .google_secret_key
        .clone()
        .unwrap_or_default()
        .trim()
        .is_empty();
    if bound {
        return Ok(Initiate::AlreadyBound);
    }
    let secret = match pending.get(uid).await {
        Some(s) => s,
        None => {
            let s = totp::create_secret();
            pending.put(uid, &s).await;
            s
        }
    };
    let otpauth = format!("otpauth://totp/{}?secret={}", member.username, secret);
    Ok(Initiate::Secret { secret, otpauth })
}

/// `google()` POST (L890-933): the legacy order — the `auth_type = 4` lockout
/// gate first (read-only), then the empty-code and pending-secret guards, then
/// the TOTP verify. A miss records a failure and rejects; a hit clears the
/// counter and persists the secret **only while the member is still unbound**
/// (mirroring the `where(google_secret_key = '')`, tolerant of a NULL column),
/// then drops the pending secret.
pub async fn bind_confirm(
    db: &DatabaseConnection,
    pending: &PendingSecrets,
    limiter: &AuthLimiter,
    uid: i64,
    code: &str,
    now: i64,
) -> GatewayResult<BindResult> {
    if limiter.is_locked(AuthKind::GoogleTotp, uid).await {
        let secs = limiter.retry_after(AuthKind::GoogleTotp, uid).await;
        return Ok(BindResult::Locked {
            msg: lockout_msg(secs),
        });
    }
    if code.is_empty() {
        return Ok(BindResult::EmptyCode);
    }
    let Some(secret) = pending.get(uid).await else {
        return Ok(BindResult::NoPending);
    };
    if !totp::verify_code(&secret, code, totp::GOOGLE_DISCREPANCY, now) {
        limiter.record_fail(AuthKind::GoogleTotp, uid).await;
        return Ok(BindResult::BadCode);
    }
    limiter.clear(AuthKind::GoogleTotp, uid).await;
    let res = members::Entity::update_many()
        .col_expr(
            members::Column::GoogleSecretKey,
            Expr::value(Some(secret.clone())),
        )
        .filter(members::Column::Id.eq(uid))
        .filter(
            Condition::any()
                .add(members::Column::GoogleSecretKey.eq(""))
                .add(members::Column::GoogleSecretKey.is_null()),
        )
        .exec(db)
        .await?;
    pending.clear(uid).await;
    Ok(BindResult::Bound {
        status: res.rows_affected,
    })
}

/// `unbindGoogle()` POST (L941-964): clear the secret column (empty string, as
/// legacy stores it). No TOTP and no unconditional gate — the legacy guards it
/// only behind the SMS factor (skipped at `sms_status() == false`), so with a
/// live panel session the write proceeds.
pub async fn unbind(db: &DatabaseConnection, uid: i64) -> GatewayResult<UnbindResult> {
    members::Entity::update_many()
        .col_expr(
            members::Column::GoogleSecretKey,
            Expr::value(Some(String::new())),
        )
        .filter(members::Column::Id.eq(uid))
        .exec(db)
        .await?;
    Ok(UnbindResult::Unbound)
}
