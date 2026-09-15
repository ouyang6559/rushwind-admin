//! Captcha — 6-char alphanumeric challenges rendered as PNG:
//! 6 chars from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, 10-minute TTL,
//! Redis key `gowind:captcha:{id}`, verify-and-delete on match.

use rand::Rng;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

pub const CAPTCHA_SOURCE: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CAPTCHA_TTL_SECS: u64 = 600;

fn captcha_key(id: &str) -> String {
    format!("gowind:captcha:{id}")
}

/// Generates a captcha: returns (id, base64 png data-url, answer).
pub async fn generate(redis: &ConnectionManager) -> Result<(String, String, String), String> {
    let mut conn = redis.clone();
    let id = uuid::Uuid::now_v7().simple().to_string();
    let chars: String = (0..6)
        .map(|_| {
            let idx = rand::rng().random_range(0..CAPTCHA_SOURCE.len());
            CAPTCHA_SOURCE.as_bytes()[idx] as char
        })
        .collect();
    let b64 = render_png_base64(&chars)?;
    let _: redis::RedisResult<()> = conn
        .set_ex(captcha_key(&id), chars.clone(), CAPTCHA_TTL_SECS)
        .await;
    drop(conn);
    Ok((id, format!("data:image/png;base64,{b64}"), chars))
}

/// Verify-and-delete: a match consumes the row; anything else (missing,
/// expired, mismatched) fails.
pub async fn verify(redis: &ConnectionManager, id: &str, value: &str) -> bool {
    if id.is_empty() || value.is_empty() {
        return false;
    }
    let mut conn = redis.clone();
    let stored: Option<String> = conn.get(captcha_key(id)).await.unwrap_or(None);
    match stored {
        Some(answer) if answer == value => {
            let _: Result<i64, _> = conn.del(captcha_key(id)).await;
            true
        }
        _ => false,
    }
}

/// Renders the 6 characters into a noisy PNG, base64-encoded. The
/// reference rides base64Captcha's DriverString renderer; any legible
/// PNG matches the wire contract (the image is random per call anyway).
fn render_png_base64(chars: &str) -> Result<String, String> {
    let mut cap = captcha::Captcha::new();
    let glyphs: Vec<char> = chars.chars().collect();
    cap.set_chars(&glyphs);
    cap.apply_filter(captcha::filters::Noise::new(0.3));
    cap.view(240, 80);
    cap.as_base64().ok_or_else(|| "captcha render".to_string())
}
