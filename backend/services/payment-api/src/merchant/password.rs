//! Merchant password hashing — the legacy `md5(password . salt)` storage
//! format (`Common/Common/function.php::generateUser` L815; login compare
//! `User/LoginController::check` L125). Wire compatibility with existing
//! `members.password` rows forces the same scheme here; the modernization to
//! `argon2id` (and dropping the plaintext `origin_password` / unsalted
//! `paypassword`, `spec/05` §12.1) is a Phase-7 migration concern, tracked in
//! the diff list — this module keeps the byte-for-byte legacy rule so current
//! rows still authenticate.

use rand::Rng;
use sea_orm::{ActiveModelTrait, Set};

use crate::data::members;
use crate::merchant::MembersRepo;
use crate::state::GatewayResult;

/// The default payment password seeded at registration (`md5('123456')`,
/// `generateUser` L818). The value is guarded by a second factor when the
/// merchant later views/resets the API key (`spec/05` §9).
pub const DEFAULT_PAY_PASSWORD: &str = "123456";

/// Lowercase hex MD5 — the exact output of PHP's `md5()` on the same bytes.
fn md5_lower(bytes: &[u8]) -> String {
    format!("{:x}", md5::compute(bytes))
}

/// A fresh 4-digit salt, matching `rand(1000,9999)` (`generateUser` L805);
/// stored alongside the hash and reused on every verification.
pub fn new_salt() -> String {
    rand::rng().random_range(1000..=9999).to_string()
}

/// Login password hash: `md5(password . salt)` (lowercase hex).
pub fn hash_password(password: &str, salt: &str) -> String {
    md5_lower(format!("{password}{salt}").as_bytes())
}

/// Verifies a login password against the stored hash. PHP compares with `==`
/// on two lowercase hex strings, so a case-sensitive equality is faithful.
pub fn verify_password(password: &str, salt: &str, stored: &str) -> bool {
    hash_password(password, salt) == stored
}

/// Payment-password hash: `md5(password)` with NO salt
/// (`AccountController::editPaypassword` L475; default `md5('123456')`).
pub fn hash_pay_password(password: &str) -> String {
    md5_lower(password.as_bytes())
}

/// Verifies a payment password against its stored (unsalted) hash.
pub fn verify_pay_password(password: &str, stored: &str) -> bool {
    hash_pay_password(password) == stored
}

// --- §10 password-change surface (editPassword / editPaypassword) -----------

/// The legacy reject when a form field is missing / the confirm mismatch / the
/// old password is wrong (`{status:0,'输入错误'}`).
pub const MSG_INPUT_ERR: &str = "输入错误";
/// The login-password reject when the new password equals the current one
/// (`{status:0,'请勿使用旧密码'}`, the MySQL 0-affected-rows branch).
pub const MSG_REUSE_OLD: &str = "请勿使用旧密码";
/// The login-password success message (`{status:1,'修改密码成功'}`).
pub const MSG_LOGIN_OK: &str = "修改密码成功";

/// The outcome of a password change, shaped for the legacy `ajaxReturn`
/// (`{status, msg?}`). The pay-password change carries no message on success
/// (legacy `{status: $res}`), and its new-equals-old case is a bare
/// `{status:0}` (no `msg`), unlike the login flow's `请勿使用旧密码`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PwdOutcome {
    /// Rejected before any write; `status` is `0`, `msg` may be absent.
    Rejected { status: u8, msg: Option<String> },
    /// The change was persisted; `msg` is present for login, absent for pay.
    Success { msg: Option<String> },
}

/// The shared `{status:0,'输入错误'}` rejection.
fn input_error() -> PwdOutcome {
    PwdOutcome::Rejected {
        status: 0,
        msg: Some(MSG_INPUT_ERR.to_string()),
    }
}

/// Pure login-password evaluation (`editPassword` L515-519). Rejects an empty
/// field, a confirm mismatch, or a wrong old password (`md5(old.salt)`) as
/// `输入错误`; a new password that hashes to the CURRENT stored one is the
/// legacy 0-affected-rows `请勿使用旧密码` (reproduced by explicit comparison,
/// not `rows_affected`, since Postgres counts a same-value UPDATE as 1 row).
/// Otherwise returns the new hash to persist.
fn eval_login(
    stored: &str,
    salt: &str,
    old: &str,
    new: &str,
    second: &str,
) -> Result<String, PwdOutcome> {
    if old.is_empty()
        || new.is_empty()
        || second.is_empty()
        || new != second
        || hash_password(old, salt) != stored
    {
        return Err(input_error());
    }
    let new_hash = hash_password(new, salt);
    if new_hash == stored {
        return Err(PwdOutcome::Rejected {
            status: 0,
            msg: Some(MSG_REUSE_OLD.to_string()),
        });
    }
    Ok(new_hash)
}

/// Pure pay-password evaluation (`editPaypassword` L471-475): same reject rules
/// against the unsalted `md5(old)`; a new-equals-old collapses to a bare
/// `{status:0}` (no message) — the legacy `{status: $res}` where an identical
/// `save` affected 0 rows.
fn eval_pay(stored: &str, old: &str, new: &str, second: &str) -> Result<String, PwdOutcome> {
    if old.is_empty()
        || new.is_empty()
        || second.is_empty()
        || new != second
        || hash_pay_password(old) != stored
    {
        return Err(input_error());
    }
    let new_hash = hash_pay_password(new);
    if new_hash == stored {
        return Err(PwdOutcome::Rejected {
            status: 0,
            msg: None,
        });
    }
    Ok(new_hash)
}

/// Runs the §10 login-password change (`editPassword`) against the DB. The SMS
/// factor is the caller's (unwired, `sms_status() == false`) seam, so this only
/// performs the legacy verify-then-write. `Ok(Rejected)` for every reject branch
/// (no write); `Ok(Success)` after persisting `md5(new.salt)`. A missing member
/// folds to `输入错误` (the session gate normally guarantees existence).
pub async fn change_login_password(
    db: &sea_orm::DatabaseConnection,
    uid: i64,
    old: &str,
    new: &str,
    second: &str,
) -> GatewayResult<PwdOutcome> {
    let Some(member) = MembersRepo::new(db).by_id(uid).await? else {
        return Ok(input_error());
    };
    let new_hash = match eval_login(
        &member.password.clone(),
        &member.salt.clone(),
        old,
        new,
        second,
    ) {
        Ok(h) => h,
        Err(out) => return Ok(out),
    };
    let mut active: members::ActiveModel = member.into();
    active.password = Set(new_hash);
    active.update(db).await?;
    Ok(PwdOutcome::Success {
        msg: Some(MSG_LOGIN_OK.to_string()),
    })
}

/// Runs the §10 pay-password change (`editPaypassword`) against the DB (no
/// salt). Persists `md5(new)` on success and replies a message-less `Success`
/// (legacy `{status: 1}`).
pub async fn change_pay_password(
    db: &sea_orm::DatabaseConnection,
    uid: i64,
    old: &str,
    new: &str,
    second: &str,
) -> GatewayResult<PwdOutcome> {
    let Some(member) = MembersRepo::new(db).by_id(uid).await? else {
        return Ok(input_error());
    };
    let stored = member.pay_password.clone().unwrap_or_default();
    let new_hash = match eval_pay(&stored, old, new, second) {
        Ok(h) => h,
        Err(out) => return Ok(out),
    };
    let mut active: members::ActiveModel = member.into();
    active.pay_password = Set(Some(new_hash));
    active.update(db).await?;
    Ok(PwdOutcome::Success { msg: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salt_is_four_digits() {
        let s = new_salt();
        assert_eq!(s.len(), 4);
        assert!(s.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn pay_password_default_hash_is_the_public_md5_vector() {
        // md5("123456") is a widely published constant; guards the lowercase
        // hex rendering against an accidental uppercase/encoding change.
        assert_eq!(
            hash_pay_password("123456"),
            "e10adc3949ba59abbe56e057f20f883e"
        );
        assert!(verify_pay_password("123456", DEFAULT_PAY_PASSWORD_HASH));
    }

    const DEFAULT_PAY_PASSWORD_HASH: &str = "e10adc3949ba59abbe56e057f20f883e";

    #[test]
    fn password_roundtrips_and_rejects() {
        let salt = new_salt();
        let stored = hash_password("s3cret", &salt);
        assert!(verify_password("s3cret", &salt, &stored));
        assert!(!verify_password("wrong", &salt, &stored));
        // a different salt must not verify even for the right password
        assert!(!verify_password("s3cret", "0000", &stored));
    }

    #[test]
    fn password_hash_is_lowercase_hex_md5() {
        // md5("abc1234") — recomputed independently below to pin the layout
        // `password . salt` (no separator).
        let got = hash_password("abc", "1234");
        let want = format!("{:x}", md5::compute(b"abc1234"));
        assert_eq!(got, want);
        assert!(got
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn eval_login_rejects_bad_input_and_reuse() {
        let salt = "1234";
        let stored = hash_password("oldpass", salt);
        // wrong old → 输入错误
        assert_eq!(
            eval_login(&stored, salt, "nope", "newpass", "newpass"),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: Some(MSG_INPUT_ERR.to_string())
            })
        );
        // confirm mismatch → 输入错误
        assert_eq!(
            eval_login(&stored, salt, "oldpass", "newpass", "diff"),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: Some(MSG_INPUT_ERR.to_string())
            })
        );
        // an empty field → 输入错误
        assert_eq!(
            eval_login(&stored, salt, "oldpass", "", ""),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: Some(MSG_INPUT_ERR.to_string())
            })
        );
        // new == old → 请勿使用旧密码
        assert_eq!(
            eval_login(&stored, salt, "oldpass", "oldpass", "oldpass"),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: Some(MSG_REUSE_OLD.to_string())
            })
        );
    }

    #[test]
    fn eval_login_returns_the_new_hash_on_success() {
        let salt = "1234";
        let stored = hash_password("oldpass", salt);
        let got = eval_login(&stored, salt, "oldpass", "newpass", "newpass").unwrap();
        assert_eq!(got, hash_password("newpass", salt));
    }

    #[test]
    fn eval_pay_rejects_bad_input_and_silently_collapses_reuse() {
        let stored = hash_pay_password("123456");
        // wrong old → 输入错误
        assert_eq!(
            eval_pay(&stored, "999999", "888888", "888888"),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: Some(MSG_INPUT_ERR.to_string())
            })
        );
        // confirm mismatch → 输入错误
        assert_eq!(
            eval_pay(&stored, "123456", "888888", "777777"),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: Some(MSG_INPUT_ERR.to_string())
            })
        );
        // new == old → bare {status:0}, NO message (unlike login)
        assert_eq!(
            eval_pay(&stored, "123456", "123456", "123456"),
            Err(PwdOutcome::Rejected {
                status: 0,
                msg: None
            })
        );
    }

    #[test]
    fn eval_pay_returns_the_new_unsalted_hash_on_success() {
        let stored = hash_pay_password("123456");
        let got = eval_pay(&stored, "123456", "abcdef", "abcdef").unwrap();
        assert_eq!(got, hash_pay_password("abcdef"));
    }
}
