//! 找回密码 — the port of `User/LoginController::sendUserCode` + `forgetpwd`
//! (L380-456, `spec/05` §5 / §10).
//!
//! The legacy找回密码 rides an **EMAIL** code, not SMS: [`send_user_code`]
//! draws a 5-digit code (`rand(10000,99999)` — an independent draw, digits may
//! repeat, unlike the SMS kernel's distinct-6), mails it via the [`EmailProvider`]
//! seam, and files a `pay_user_code` row (`type = 0`, `endtime = now + 600`).
//! [`reset_password`] matches the newest live row on
//! `username + email + code + status = 0 + type = 0 + endtime > now`, then
//! writes `md5(new_password . salt)` (the login hash, [`password::hash_password`])
//! onto the member and consumes the code (`status = 1`, `uptime = now`).
//!
//! These are PRE-LOGIN endpoints, so nothing here touches a panel session.
//!
//! Delivery is a seam: the real SMTP (`pay_email` config + PHPMailer) is NOT
//! modeled — until it is wired, the code is stored (the flow stays fully
//! testable) but never reaches an inbox, so a reset can only complete with a
//! code the caller otherwise obtained out-of-band. This mirrors the SMS kernel's
//! fail-closed posture and never weakens an existing guarantee. The email BODY
//! is narrowed from the legacy `sendFindpwdemail` (which composes a site
//! name / login URL from `websiteconfig`) to the essential code + validity line.

use async_trait::async_trait;
use rand::Rng;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::data::{members, user_codes};
use crate::merchant::password;
use crate::state::{GatewayError, GatewayResult};

/// The找回密码 purpose discriminator (`pay_user_code.type = 0`).
pub const TYPE_FINDPWD: i32 = 0;
/// Code validity — the legacy `endtime = time() + 600` (10 minutes).
pub const CODE_TTL_SECS: i64 = 600;

/// The email-delivery seam. The real SMTP provider is a later concern;
/// [`NoopEmailProvider`] logs and reports success so the flow is exercisable.
#[async_trait]
pub trait EmailProvider: Send + Sync {
    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<(), String>;
}

/// The stand-in mailer: records a "would-send" trace and reports success.
pub struct NoopEmailProvider;

#[async_trait]
impl EmailProvider for NoopEmailProvider {
    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<(), String> {
        tracing::debug!(to = %to, subject = %subject, body = %html, "findpwd email dispatch (provider seam): would-send");
        Ok(())
    }
}

/// Outcome of [`send_user_code`], mapped by the handler to the legacy `msg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// A fresh code was mailed + filed.
    Sent,
    /// No member matches the username + email pair (`用户或邮箱不正确`).
    UserNotFound,
    /// The mailer rejected the send (`发送邮件失败`).
    SendFailed,
}

/// Outcome of [`reset_password`], mapped by the handler to the legacy `msg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOutcome {
    /// Password rewritten and the code consumed.
    Success,
    /// No live row matched the code (`验证码不正确或过期`) or no member matched.
    CodeInvalid,
}

/// Draws a 5-digit code, faithfully `rand(10000,99999)` — digits may repeat
/// (unlike the SMS kernel), the range itself guarantees the leading-5 length.
pub fn generate_code() -> String {
    let n = rand::rng().random_range(10_000..=99_999);
    n.to_string()
}

/// The findpwd body: the code + its 10-minute validity (narrowed from
/// `sendFindpwdemail`, which also embedded site name / login URL).
pub fn code_html(code: &str) -> String {
    format!("找回密码验证码：<span style='color:#F30;'>{code}</span> ,十分钟内有效 <br />此为系统邮件，请勿回复。")
}

async fn member_by_username_email(
    db: &DatabaseConnection,
    username: &str,
    email: &str,
) -> GatewayResult<Option<members::Model>> {
    members::Entity::find()
        .filter(members::Column::Username.eq(username))
        .filter(members::Column::Email.eq(email))
        .one(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// `sendUserCode`: verify the account, mail a fresh code, file it. The member
/// gate runs before any send so an unknown account never triggers a mail.
pub async fn send_user_code(
    db: &DatabaseConnection,
    mailer: &dyn EmailProvider,
    username: &str,
    email: &str,
    now_ts: i64,
) -> GatewayResult<SendOutcome> {
    if member_by_username_email(db, username, email)
        .await?
        .is_none()
    {
        return Ok(SendOutcome::UserNotFound);
    }
    let code = generate_code();
    if mailer
        .send(email, "找回密码", &code_html(&code))
        .await
        .is_err()
    {
        return Ok(SendOutcome::SendFailed);
    }
    user_codes::ActiveModel {
        r#type: Set(TYPE_FINDPWD),
        code: Set(Some(code)),
        username: Set(Some(username.to_string())),
        email: Set(Some(email.to_string())),
        mobile: Set(None),
        status: Set(0),
        ctime: Set(Some(now_ts)),
        uptime: Set(None),
        endtime: Set(Some(now_ts + CODE_TTL_SECS)),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(SendOutcome::Sent)
}

/// `forgetpwd`: match a live code, rewrite the login password, consume the
/// code. A field-validation failure (empty / mismatched passwords) is the
/// handler's job; this covers the code + member + write legs only.
pub async fn reset_password(
    db: &DatabaseConnection,
    username: &str,
    email: &str,
    code: &str,
    new_password: &str,
    now_ts: i64,
) -> GatewayResult<ResetOutcome> {
    // Newest live code for this account + code.
    let Some(used) = user_codes::Entity::find()
        .filter(user_codes::Column::Username.eq(username))
        .filter(user_codes::Column::Email.eq(email))
        .filter(user_codes::Column::Code.eq(code))
        .filter(user_codes::Column::Status.eq(0))
        .filter(user_codes::Column::Type.eq(TYPE_FINDPWD))
        .filter(user_codes::Column::Endtime.gt(now_ts))
        .order_by_desc(user_codes::Column::Id)
        .one(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?
    else {
        return Ok(ResetOutcome::CodeInvalid);
    };
    let Some(member) = member_by_username_email(db, username, email).await? else {
        return Ok(ResetOutcome::CodeInvalid);
    };
    let new_hash = password::hash_password(new_password, &member.salt);
    members::Entity::update_many()
        .col_expr(members::Column::Password, Expr::value(new_hash))
        .filter(members::Column::Id.eq(member.id))
        .exec(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    // Consume the code.
    user_codes::Entity::update_many()
        .col_expr(user_codes::Column::Status, Expr::value(1))
        .col_expr(user_codes::Column::Uptime, Expr::value(now_ts))
        .filter(user_codes::Column::Id.eq(used.id))
        .exec(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(ResetOutcome::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_five_digits() {
        for _ in 0..200 {
            let c = generate_code();
            assert_eq!(c.len(), 5, "{c}");
            assert!(c.chars().all(|d| d.is_ascii_digit()), "{c}");
        }
    }

    #[test]
    fn code_html_carries_the_code_and_validity() {
        let html = code_html("12345");
        assert!(html.contains("12345"), "{html}");
        assert!(html.contains("十分钟内有效"), "{html}");
    }
}
