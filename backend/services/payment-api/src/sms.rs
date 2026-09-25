//! Merchant SMS verification-code kernel — the Redis port of the legacy
//! `User/BaseController::send` / `checkSessionTime` pair (`spec/05` §5/§10).
//! A 6-digit code is drawn, stored under a short TTL keyed by
//! `(call_index, uid)`, and consumed on a correct verify (verify-and-delete).
//!
//! Faithfulness notes (registered as a decision memory):
//! - the legacy code is `substr(implode('', shuffle(range(0,9))), 0, 6)` —
//!   SIX *distinct* digits — so [`generate_code`] reproduces that exactly
//!   (a partial Fisher-Yates), not an independent per-digit draw;
//! - the legacy dual session keys (`send.<callIndex>` for the code and
//!   `send.<callIndex>|<code>` for the issue time, checked `< EXPIRE`)
//!   collapse to a single Redis key under an equal 300s TTL — the observable
//!   (valid within 300s, then gone) is identical;
//! - delivery is a [`dispatch`] no-op seam: the real provider (aliyun / smsbao)
//!   and its `pay_sms.is_open` switch, plus the `getSmsTemplateCode` template
//!   lookup, are NOT modeled here. Until a provider is wired, an issued code is
//!   stored (so the flow is fully testable) but never reaches a phone, which
//!   keeps every code-gated write **fail-closed** — no new insecurity.

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use sea_orm::{DatabaseConnection, EntityTrait, QueryOrder};

use crate::channel::sign::md5_hex_lower;
use crate::data::sms_configs;
use crate::state::{GatewayError, GatewayResult};

/// Code length — `BaseController::LENGTH = 6` (`spec/05` §5.2).
pub const SMS_LEN: usize = 6;
/// Time-to-live — `BaseController::EXPIRE = 300`s.
pub const SMS_TTL_SECS: u64 = 300;
/// The two-step `editMobile` phase flag lives an hour — long enough to cover
/// the old→new round trip, short enough to expire a abandoned flow.
pub const EDIT_PHASE_TTL_SECS: u64 = 3600;

/// The legacy wrong / expired verification-code reject (`验证码错误`).
pub const MSG_SMS_CODE_WRONG: &str = "验证码错误";

fn code_key(call_index: &str, uid: i64) -> String {
    format!("payment:sms:code:{call_index}:{uid}")
}

fn phase_key(uid: i64) -> String {
    format!("payment:sms:editphase:{uid}")
}

/// Draws a [`SMS_LEN`]-digit code of SIX *distinct* digits, faithfully
/// reproducing PHP's `shuffle(range(0,9))` then `substr(..., 0, 6)`: a partial
/// Fisher-Yates over `0..=9` leaving the first [`SMS_LEN`] positions random and
/// non-repeating.
pub fn generate_code() -> String {
    let mut digits: [u8; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    let mut rng = rand::rng();
    for i in 0..SMS_LEN {
        let j = rand::Rng::random_range(&mut rng, i..digits.len());
        digits.swap(i, j);
    }
    digits[..SMS_LEN]
        .iter()
        .map(|d| char::from(b'0' + d))
        .collect()
}

/// The provider seam: records a "would-send" trace and reports success without
/// touching any real gateway. Swapping in aliyun / smsbao (+ the `pay_sms`
/// config and template lookup) is a Phase-7 delivery concern.
pub fn dispatch(mobile: &str, code: &str) -> bool {
    tracing::debug!(mobile = %mobile, code = %code, "sms dispatch (provider seam): would-send verification code");
    true
}

/// A Redis-backed SMS-code issuer/verifier plus the `editMobile` two-step
/// phase flag (cheap to clone; shares the pool).
#[derive(Clone)]
pub struct SmsCodes {
    redis: ConnectionManager,
}

impl SmsCodes {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }

    /// Issues a fresh code for `(call_index, uid)`: generates it, overwrites any
    /// prior active code under the key with a [`SMS_TTL_SECS`] expiry, and
    /// returns the plaintext for the caller to [`dispatch`]. The HTTP handlers
    /// deliberately never surface this value to the client.
    pub async fn issue(&self, call_index: &str, uid: i64) -> Result<String, String> {
        let code = generate_code();
        let mut conn = self.redis.clone();
        conn.set_ex::<_, _, ()>(code_key(call_index, uid), code.clone(), SMS_TTL_SECS)
            .await
            .map_err(|e| format!("sms store: {e}"))?;
        Ok(code)
    }

    /// Verify-and-consume: a matching, unexpired code is deleted and accepted;
    /// a missing / expired / mismatched / empty code fails.
    pub async fn verify(&self, call_index: &str, uid: i64, code: &str) -> bool {
        if code.is_empty() {
            return false;
        }
        let mut conn = self.redis.clone();
        let stored: Option<String> = conn.get(code_key(call_index, uid)).await.unwrap_or(None);
        match stored {
            Some(answer) if answer == code => {
                let _: Result<i64, _> = conn.del(code_key(call_index, uid)).await;
                true
            }
            _ => false,
        }
    }

    /// Marks the `editMobile` flow as past the old-phone step (phase 1).
    pub async fn set_edit_phase(&self, uid: i64) {
        let mut conn = self.redis.clone();
        let _: Result<(), _> = conn
            .set_ex::<_, _, ()>(phase_key(uid), "1", EDIT_PHASE_TTL_SECS)
            .await;
    }

    /// Whether the `editMobile` flow is at the new-phone step.
    pub async fn edit_phase(&self, uid: i64) -> bool {
        let mut conn = self.redis.clone();
        conn.get::<_, Option<String>>(phase_key(uid))
            .await
            .unwrap_or(None)
            .is_some()
    }

    /// Clears the `editMobile` phase flag (flow done or abandoned).
    pub async fn clear_edit_phase(&self, uid: i64) {
        let mut conn = self.redis.clone();
        let _: Result<i64, _> = conn.del(phase_key(uid)).await;
    }
}

// --- provider seam + `pay_sms` config (下发面, `spec/05` §5) ----------------

/// An outbound SMS gateway. Kept behind a trait so the concrete provider is
/// chosen from `pay_sms.sms_channel` at call time and a loopback stub can stand
/// in for the real HTTP in tests.
#[async_trait]
pub trait SmsProvider: Send + Sync {
    /// Delivers `content` to `mobile`; `Ok(())` on acceptance, `Err(reason)`
    /// otherwise.
    async fn send(&self, mobile: &str, content: &str) -> Result<(), String>;
}

/// The stand-in until a gateway is configured: logs and reports success. Also
/// the fallback for an unconfigured / half-filled channel row, so an operator
/// who flips `is_open` before entering credentials still gets a delivered
/// request rather than a panic (mirrors the legacy `default: return;`).
pub struct NoopProvider;

#[async_trait]
impl SmsProvider for NoopProvider {
    async fn send(&self, mobile: &str, content: &str) -> Result<(), String> {
        tracing::debug!(mobile = %mobile, content = %content, "sms dispatch (noop provider): would-send");
        Ok(())
    }
}

/// 短信宝 (smsbao) — the port of `Org\Util\SmsBao`: a plain HTTP GET to
/// `{gateway}sms?u=<user>&p=md5(<pass>)&m=<mobile>&c=<content>` whose response
/// body is a status code, `"0"` meaning success. `gateway` is injectable so a
/// test can point it at a loopback stub instead of the live API.
pub struct SmsBaoProvider {
    pub user: String,
    pub pass: String,
    pub gateway: String,
}

impl SmsBaoProvider {
    /// The production 短信宝 endpoint (legacy `$sms_gateway`).
    pub const DEFAULT_GATEWAY: &'static str = "http://api.smsbao.com/";

    pub fn new(user: impl Into<String>, pass: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            pass: pass.into(),
            gateway: Self::DEFAULT_GATEWAY.to_string(),
        }
    }

    /// Overrides the gateway base (loopback tests).
    pub fn with_gateway(mut self, gateway: impl Into<String>) -> Self {
        self.gateway = gateway.into();
        self
    }
}

#[async_trait]
impl SmsProvider for SmsBaoProvider {
    async fn send(&self, mobile: &str, content: &str) -> Result<(), String> {
        // The legacy hashes the password with md5 (lowercase hex) before sending.
        let pass = md5_hex_lower(self.pass.as_bytes());
        let url = format!("{}sms", self.gateway);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .query(&[
                ("u", self.user.as_str()),
                ("p", pass.as_str()),
                ("m", mobile),
                ("c", content),
            ])
            .send()
            .await
            .map_err(|e| format!("smsbao request: {e}"))?;
        let body = resp.text().await.map_err(|e| format!("smsbao read: {e}"))?;
        let code = body.trim();
        if code == "0" {
            Ok(())
        } else {
            Err(smsbao_status(code).to_string())
        }
    }
}

/// The 短信宝 status table (`SmsBao::$statusStr`), for the error string.
fn smsbao_status(code: &str) -> &'static str {
    match code {
        "-1" => "参数不全",
        "-2" => "服务器空间不支持,请确认支持curl或者fsocket",
        "30" => "密码错误",
        "40" => "账号不存在",
        "41" => "余额不足",
        "42" => "帐户已过期",
        "43" => "IP地址限制",
        "50" => "内容含有敏感词",
        _ => "短信发送失败",
    }
}

/// The effective dispatch view of the singleton `pay_sms` row: the `is_open`
/// gate, the `sms_channel` pick, the 短信宝 credentials, and the 【签名】 used to
/// compose the code message. Secrets stay in the DB row exactly as the legacy
/// stores them; the rewrite's own tests never seed live values.
#[derive(Debug, Clone)]
pub struct SmsConfig {
    pub is_open: bool,
    pub sms_channel: String,
    pub smsbao_user: String,
    pub smsbao_pass: String,
    pub sign_name: String,
}

impl SmsConfig {
    /// Reads the singleton config row (lowest `id`, matching `M('sms')->find()`
    /// on a fresh install). `None` when the table is empty — treated as closed.
    pub async fn load(db: &DatabaseConnection) -> GatewayResult<Option<SmsConfig>> {
        let row = sms_configs::Entity::find()
            .order_by_asc(sms_configs::Column::Id)
            .one(db)
            .await
            .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
        Ok(row.map(|c| SmsConfig {
            is_open: c.is_open == 1,
            sms_channel: c.sms_channel,
            smsbao_user: c.smsbao_user,
            smsbao_pass: c.smsbao_pass,
            sign_name: c.sign_name.unwrap_or_default(),
        }))
    }

    /// Builds the provider for this row's channel. Only `smsbao` with both
    /// credentials set reaches a real gateway; anything else (unset channel,
    /// half-filled creds, or `aliyun` — not yet ported) falls back to
    /// [`NoopProvider`] so a misconfiguration can never send a malformed
    /// request nor crash the caller.
    pub fn provider(&self) -> Box<dyn SmsProvider> {
        if self.sms_channel == "smsbao"
            && !self.smsbao_user.is_empty()
            && !self.smsbao_pass.is_empty()
        {
            Box::new(SmsBaoProvider::new(
                self.smsbao_user.clone(),
                self.smsbao_pass.clone(),
            ))
        } else {
            Box::new(NoopProvider)
        }
    }
}

/// The real-value port of `smsStatus()`: `true` only when a `pay_sms` row
/// exists with `is_open = 1`. Replaces the previous constant-`false` seam once
/// a caller opts into DB-backed gating.
pub async fn sms_status(db: &DatabaseConnection) -> GatewayResult<bool> {
    Ok(SmsConfig::load(db)
        .await?
        .map(|c| c.is_open)
        .unwrap_or(false))
}

/// The verification-code body 短信宝 sends (the legacy `sendSMS` inline
/// `【签名】您的验证码为：…`, since 短信宝 ignores the template table).
pub fn code_message(sign_name: &str, code: &str) -> String {
    format!("【{sign_name}】您的验证码为：{code}，该验证码5分钟内有效，请勿泄露他人。")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn code_is_six_distinct_digits() {
        for _ in 0..200 {
            let code = generate_code();
            assert_eq!(code.chars().count(), SMS_LEN);
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
            let uniq: HashSet<char> = code.chars().collect();
            assert_eq!(uniq.len(), SMS_LEN, "digits must not repeat: {code}");
        }
    }

    #[test]
    fn dispatch_reports_success_without_a_provider() {
        // The seam is a pure always-true stand-in until a gateway is wired.
        assert!(dispatch("13800000000", "123456"));
    }

    #[test]
    fn code_message_wraps_sign_and_code() {
        let msg = code_message("多宝", "123456");
        assert!(msg.starts_with("【多宝】您的验证码为：123456"), "{msg}");
        assert!(msg.contains("5分钟"), "{msg}");
    }

    /// A one-shot loopback HTTP gateway replying `body`; resolves to
    /// (`base_url`, join handle yielding the raw request bytes).
    fn loopback_gateway(body: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
            buf
        });
        (format!("http://127.0.0.1:{port}/"), handle)
    }

    #[tokio::test]
    async fn smsbao_provider_posts_and_accepts_zero() {
        let (base, server) = loopback_gateway("0");
        let provider = SmsBaoProvider::new("acct", "secret").with_gateway(base);
        let content = code_message("多宝", "123456");
        provider
            .send("13800000000", &content)
            .await
            .expect("body 0 is success");
        let req = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(req.starts_with("GET /sms?"), "{req}");
        assert!(req.contains("u=acct"), "{req}");
        assert!(req.contains("m=13800000000"), "{req}");
        // the password is sent hashed, never in clear
        assert!(
            req.contains(&format!("p={}", md5_hex_lower(b"secret"))),
            "{req}"
        );
        assert!(
            !req.contains("secret"),
            "clear password must not leak: {req}"
        );
    }

    #[tokio::test]
    async fn smsbao_provider_maps_error_status() {
        let (base, _server) = loopback_gateway("40");
        let provider = SmsBaoProvider::new("acct", "secret").with_gateway(base);
        let err = provider
            .send("13800000000", "x")
            .await
            .expect_err("40 is 账号不存在");
        assert_eq!(err, "账号不存在");
    }

    #[tokio::test]
    async fn provider_falls_back_to_noop_when_not_fully_configured() {
        // smsbao with a blank credential → Noop (no malformed request).
        let half = SmsConfig {
            is_open: true,
            sms_channel: "smsbao".into(),
            smsbao_user: "u".into(),
            smsbao_pass: String::new(),
            sign_name: "多宝".into(),
        };
        assert!(half.provider().send("1", "c").await.is_ok());
        // aliyun (not yet ported) → Noop too.
        let ali = SmsConfig {
            is_open: true,
            sms_channel: "aliyun".into(),
            smsbao_user: String::new(),
            smsbao_pass: String::new(),
            sign_name: String::new(),
        };
        assert!(ali.provider().send("1", "c").await.is_ok());
    }
}
