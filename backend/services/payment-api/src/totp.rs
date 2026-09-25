//! Google-Authenticator TOTP (`Org\Util\GoogleAuthenticator`, RFC 6238) — the
//! merchant-side second factor the `saveProfile` write gates on (`spec/05`
//! §10). The port is byte-faithful to the legacy `getCode` / `verifyCode`:
//! a base32 secret, `floor(unix_secs / 30)` as a big-endian 64-bit counter,
//! HMAC-SHA1, the classic dynamic truncation to `10^6`, and a
//! `±discrepancy` window of 30s slices (`config.php:40` ships
//! `google_discrepancy = 2`). Pure compute only — no external service — so the
//! whole module is offline-testable against the RFC 6238 vectors.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

/// The TOTP time step in seconds (`getCode`: `floor(time() / 30)`).
pub const TOTP_STEP: i64 = 30;
/// The displayed code length (`_codeLength = 6`).
pub const CODE_DIGITS: usize = 6;
/// `google_discrepancy` default (`config.php:40`) — the accepted drift in
/// 30s slices, so a code valid from `-2*30s` to `+2*30s`.
pub const GOOGLE_DISCREPANCY: i64 = 2;

/// RFC 4648 base32 alphabet (the legacy `_getBase32LookupTable` sans padding).
const BASE32_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The default secret length (`createSecret($secretLength = 16)`).
pub const SECRET_LEN: usize = 16;

/// Generates a fresh base32 shared secret, the `createSecret` port: 16
/// characters, one per CSPRNG byte mapped through `alphabet[byte & 31]` (the
/// legacy `random_bytes(16)` loop) — so every char is from `A-Z2-7`, the
/// padding `=` (alphabet index 32) never appears.
pub fn create_secret() -> String {
    let mut buf = [0u8; SECRET_LEN];
    rand::rng().fill_bytes(&mut buf);
    buf.iter()
        .map(|&b| BASE32_ALPHABET[(b & 31) as usize] as char)
        .collect()
}

/// Decodes a base32 secret. Case-insensitive; `=`, spaces and `-` are ignored
/// (Google Authenticator keys are grouped with spaces). Any other out-of-range
/// byte fails to `None`. An all-empty / padding-only input yields an empty key.
pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut lookup = [0xFFu8; 256];
    for (i, &c) in BASE32_ALPHABET.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut bits = 0u32;
    let mut acc = 0u32;
    for &b in input.as_bytes() {
        if matches!(b, b'=' | b' ' | b'\t' | b'\r' | b'\n' | b'-') {
            continue;
        }
        let val = lookup[b.to_ascii_uppercase() as usize];
        if val == 0xFF {
            return None;
        }
        acc = (acc << 5) | val as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// RFC 4226 dynamic truncation of a 20-byte HMAC-SHA1 digest to a
/// zero-padded `digits`-char decimal code.
fn dynamic_trunc(hmac: &[u8], digits: usize) -> String {
    let offset = (hmac[hmac.len() - 1] & 0x0F) as usize;
    let bin = ((hmac[offset] & 0x7F) as u32) << 24
        | (hmac[offset + 1] as u32) << 16
        | (hmac[offset + 2] as u32) << 8
        | (hmac[offset + 3] as u32);
    let modulo = 10u32.pow(digits as u32);
    format!("{:0width$}", bin % modulo, width = digits)
}

/// Computes the TOTP code for `secret_b32` at `unix_secs`, using the standard
/// [`TOTP_STEP`] / [`CODE_DIGITS`]. `None` only when the secret is malformed
/// base32 (or the HMAC key cannot be built from it).
pub fn totp_code(secret_b32: &str, unix_secs: i64) -> Option<String> {
    let key = base32_decode(secret_b32)?;
    let counter = unix_secs.div_euclid(TOTP_STEP);
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).ok()?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    Some(dynamic_trunc(&digest, CODE_DIGITS))
}

/// Constant-time-ish equality for equal-length ASCII codes (the legacy
/// `timingSafeEquals` / `hash_equals`), so a wrong length fails fast.
fn code_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// The faithful `verifyCode` port: reject anything not exactly 6 ASCII digits,
/// then accept a match on any slice in `[-discrepancy ..= +discrepancy]` around
/// `unix_secs`.
pub fn verify_code(secret_b32: &str, code: &str, discrepancy: i64, unix_secs: i64) -> bool {
    if code.len() != CODE_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    for i in -discrepancy..=discrepancy {
        if let Some(calc) = totp_code(secret_b32, unix_secs + i * TOTP_STEP) {
            if code_matches(&calc, code) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 SHA1 test secret `ASCII "12345678901234567890"` in base32.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn create_secret_is_sixteen_base32_chars_and_random() {
        let a = create_secret();
        let b = create_secret();
        assert_eq!(a.chars().count(), SECRET_LEN);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "{a}"
        );
        // only the A-Z2-7 alphabet (never the `0 1 8 9` or padding)
        assert!(a.chars().all(|c| BASE32_ALPHABET.contains(&(c as u8))));
        assert_ne!(a, b, "two CSPRNG draws must differ");
    }

    #[test]
    fn create_secret_roundtrips_through_base32_decode() {
        let secret = create_secret();
        // a generated secret must be decodable (16 chars = 10 bytes)
        assert_eq!(base32_decode(&secret).map(|v| v.len()), Some(10));
    }

    #[test]
    fn base32_roundtrips_the_rfc_secret() {
        let decoded = base32_decode(RFC_SECRET).unwrap();
        assert_eq!(decoded, b"12345678901234567890".to_vec());
    }

    #[test]
    fn base32_ignores_case_spaces_and_padding() {
        // Case-insensitive.
        assert_eq!(
            base32_decode(&RFC_SECRET.to_ascii_lowercase()),
            base32_decode(RFC_SECRET)
        );
        // Spaces and `=` padding are skipped, not decoded.
        assert_eq!(
            base32_decode("GEZDGNBV GY3TQOJQ=="),
            base32_decode("GEZDGNBVGY3TQOJQ")
        );
    }

    #[test]
    fn base32_rejects_out_of_alphabet_bytes() {
        // `1` (one) and `8`/`9`/`0` are not in the RFC 4648 alphabet.
        assert!(base32_decode("ABC1").is_none());
        assert!(base32_decode("AB=CD!").is_none());
    }

    #[test]
    fn matches_the_rfc_6238_sha1_vectors() {
        // Known 8-digit values, truncated to 6 by `mod 10^6`.
        assert_eq!(totp_code(RFC_SECRET, 59).unwrap(), "287082"); // 94287082
        assert_eq!(totp_code(RFC_SECRET, 1_111_111_109).unwrap(), "081804"); // 07081804
        assert_eq!(totp_code(RFC_SECRET, 1_234_567_890).unwrap(), "005924"); // 89005924
        assert_eq!(totp_code(RFC_SECRET, 2_000_000_000).unwrap(), "279037"); // 69279037
    }

    #[test]
    fn verify_accepts_within_the_window_and_rejects_outside() {
        let now = 59;
        let valid = totp_code(RFC_SECRET, now).unwrap();
        assert!(verify_code(RFC_SECRET, &valid, GOOGLE_DISCREPANCY, now));
        // Two steps earlier / later is still inside `±2`.
        let past = totp_code(RFC_SECRET, now - 2 * TOTP_STEP).unwrap();
        let future = totp_code(RFC_SECRET, now + 2 * TOTP_STEP).unwrap();
        assert!(verify_code(RFC_SECRET, &past, GOOGLE_DISCREPANCY, now));
        assert!(verify_code(RFC_SECRET, &future, GOOGLE_DISCREPANCY, now));
        // Three steps out is beyond the window.
        let too_old = totp_code(RFC_SECRET, now - 3 * TOTP_STEP).unwrap();
        assert!(!verify_code(RFC_SECRET, &too_old, GOOGLE_DISCREPANCY, now));
    }

    #[test]
    fn verify_rejects_non_six_digit_codes() {
        assert!(!verify_code(RFC_SECRET, "", GOOGLE_DISCREPANCY, 59));
        assert!(!verify_code(RFC_SECRET, "12345", GOOGLE_DISCREPANCY, 59));
        assert!(!verify_code(RFC_SECRET, "1234567", GOOGLE_DISCREPANCY, 59));
        assert!(!verify_code(RFC_SECRET, "abcdef", GOOGLE_DISCREPANCY, 59));
    }
}
