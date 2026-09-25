//! Captcha challenge service — the Redis-backed port of the legacy
//! `Think\Verify` image captcha + SMS code flow (`spec/05` §5; target §13.2
//! "短信验证码会话放入 Redis, TTL 300s"). A 6-char human-readable challenge
//! is drawn from an ambiguity-free alphabet, stored under a short TTL keyed by
//! an opaque id, and consumed on a correct verify (verify-and-delete).
//!
//! The pure [`generate_code`] is offline-testable; issue/verify use Redis.
//! Image (PNG) rendering is a delivery concern for the merchant portal in
//! Phase 7 — the answer/verify contract (and the login-disabled note in §4.1)
//! are what the backend gates on, so it is intentionally code-only here.

use rand::Rng;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// Ambiguity-free alphabet (no `I O 0 1`), `captcha` crate's classic set.
pub const CAPTCHA_SOURCE: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Challenge length — `BaseController::LENGTH = 6` (`spec/05` §5.2).
pub const CAPTCHA_LEN: usize = 6;
/// Time-to-live — `BaseController::EXPIRE = 300`s (`spec/05` §5.2).
pub const CAPTCHA_TTL_SECS: u64 = 300;

fn captcha_key(id: &str) -> String {
    format!("payment:captcha:{id}")
}

/// Draws a [`CAPTCHA_LEN`]-char code from [`CAPTCHA_SOURCE`] via the OS
/// CSPRNG. Uppercase, matching the source set.
pub fn generate_code() -> String {
    let chars: Vec<char> = CAPTCHA_SOURCE.chars().collect();
    let mut rng = rand::rng();
    (0..CAPTCHA_LEN)
        .map(|_| chars[rng.random_range(0..chars.len())])
        .collect()
}

/// A Redis-backed captcha issuer/verifier (cheap to clone).
#[derive(Clone)]
pub struct Captcha {
    redis: ConnectionManager,
}

impl Captcha {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }

    /// Issues a challenge: returns an opaque `(id, code)` pair; the code is
    /// stored for [`verify`] and returned to be rendered/sent out-of-band.
    pub async fn issue(&self) -> Result<(String, String), String> {
        let id = crate::merchant::generate_apikey();
        let code = generate_code();
        let mut conn = self.redis.clone();
        conn.set_ex::<_, _, ()>(captcha_key(&id), code.clone(), CAPTCHA_TTL_SECS)
            .await
            .map_err(|e| format!("captcha store: {e}"))?;
        Ok((id, code))
    }

    /// Verify-and-delete: a correct, unexpired code consumes the row; a
    /// missing / expired / mismatched code fails. Empty inputs fail fast.
    pub async fn verify(&self, id: &str, code: &str) -> bool {
        if id.is_empty() || code.is_empty() {
            return false;
        }
        let mut conn = self.redis.clone();
        let stored: Option<String> = conn.get(captcha_key(id)).await.unwrap_or(None);
        match stored {
            Some(answer) if answer == code => {
                let _: Result<i64, _> = conn.del(captcha_key(id)).await;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_shape_is_fixed_length_from_the_source_alphabet() {
        let code = generate_code();
        assert_eq!(code.chars().count(), CAPTCHA_LEN);
        let source: Vec<char> = CAPTCHA_SOURCE.chars().collect();
        assert!(
            code.chars().all(|c| source.contains(&c)),
            "every char drawn from the alphabet: {code}"
        );
    }

    #[test]
    fn alphabet_drops_ambiguous_glyphs() {
        for c in ['I', 'O', '0', '1'] {
            assert!(!CAPTCHA_SOURCE.contains(c), "{c} must be excluded");
        }
    }
}
