//! Captcha — 6-char alphanumeric challenges rendered as PNG:
//! 6 chars drawn from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`, 10-minute TTL,
//! Redis key `admin:captcha:{id}`, verify-and-delete on match.

use rand::Rng;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

pub const CAPTCHA_SOURCE: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CAPTCHA_TTL_SECS: u64 = 600;

/// Candidate stroke hues: dark and saturated, one picked per challenge so
/// consecutive challenges do not look mechanically identical. Every entry
/// keeps all channels far below the white background, so the undulating
/// text stays high-contrast and legible.
const CAPTCHA_HUES: [[u8; 3]; 6] = [
    [0x8b, 0x1a, 0x1a], // crimson
    [0x1a, 0x1a, 0x8b], // navy
    [0x1a, 0x6b, 0x1a], // forest
    [0x5c, 0x1a, 0x5c], // plum
    [0x1a, 0x5c, 0x5c], // teal
    [0x6b, 0x3a, 0x0a], // rust
];

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

/// Renders the challenge as a 240x80 PNG and returns `(base64 png, answer)`.
///
/// Pipeline: six glyphs laid side by side, one horizontal sine wave so the
/// run undulates, a crop that keeps the text centered, and finally a random
/// deep hue recoloring the strokes. The answer is read back off the render,
/// so image and stored answer always agree.
///
/// The glyph pool is [`CAPTCHA_SOURCE`] intersected with the renderer
/// font's repertoire: the font cannot draw a few source glyphs (`L`), and
/// an unfiltered pick silently drops them — shortening some answers below
/// six characters, and in the extreme (nearly all picks dropped) starving
/// the cropper's centering math. With only drawable glyphs in the pool,
/// every pick renders and the six-glyph run measures 142–265 px across,
/// which pins the crop window provably inside the renderer's canvas
/// (left ≥ 51, right ≤ 352, top 109, bottom 189).
fn render_png_base64() -> Result<(String, String), String> {
    let mut cap = captcha::Captcha::new();
    let supported = cap.supported_chars();
    let glyphs: Vec<char> = CAPTCHA_SOURCE
        .chars()
        .filter(|c| supported.contains(c))
        .collect();
    if glyphs.len() < 6 {
        return Err("captcha font repertoire covers too few glyphs".to_string());
    }
    cap.set_chars(&glyphs);
    cap.add_chars(6);
    let answer = cap.chars_as_string();
    cap.apply_filter(captcha::filters::Wave::new(2.0, 12.0).horizontal());
    cap.view(240, 80);
    let hue = CAPTCHA_HUES[rand::rng().random_range(0..CAPTCHA_HUES.len())];
    cap.set_color(hue);
    let b64 = cap
        .as_base64()
        .ok_or_else(|| "captcha render".to_string())?;
    Ok((b64, answer))
}
