//! Captcha — 6-char alphanumeric challenges rendered as PNG:
//! 6 chars drawn from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, 10-minute TTL,
//! Redis key `admin:captcha:{id}`, verify-and-delete on match.

use rand::Rng;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

pub const CAPTCHA_SOURCE: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CAPTCHA_TTL_SECS: u64 = 600;

fn captcha_key(id: &str) -> String {
    format!("admin:captcha:{id}")
}

/// Generates a captcha: returns (id, base64 png data-url, answer).
pub async fn generate(redis: &ConnectionManager) -> Result<(String, String, String), String> {
    let mut conn = redis.clone();
    let id = uuid::Uuid::now_v7().simple().to_string();
    let (b64, chars) = render_png_base64()?;
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

/// Renders a noisy 6-char PNG; returns (base64 png, answer). The crate
/// draws its own random picks from the configured alphabet — the answer
/// is read back off the render, so image and stored answer always agree.
fn render_png_base64() -> Result<(String, String), String> {
    let mut cap = captcha::Captcha::new();
    let glyphs: Vec<char> = CAPTCHA_SOURCE.chars().collect();
    cap.set_chars(&glyphs);
    cap.add_chars(6);
    let answer = cap.chars_as_string();
    cap.apply_filter(captcha::filters::Noise::new(0.3));
    // Crop to the crate's documented canvas (220x120); larger crops
    // underflow its centering math.
    cap.view(220, 120);
    let b64 = cap.as_base64().ok_or_else(|| "captcha render".to_string())?;
    Ok((b64, answer))
}
