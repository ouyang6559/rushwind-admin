//! Credential crypto primitives:
//! the AES-128-CBC front-end password layer (go-utils/crypto, key
//! `f51d66a73d8a0927`, IV = the key itself, PKCS#7), bcrypt hashing
//! (default cost 10), SHA-256 (access-key secrets), AES-256-GCM (MFA
//! secret at-rest, `enc:` prefix), and RFC 6238 TOTP.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use sha2::{Digest, Sha256};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// go-utils/crypto DefaultAESKey — 16 bytes, also reused as the IV.
pub const DEFAULT_AES_KEY: &[u8; 16] = b"f51d66a73d8a0927";

/// The AES-256-GCM key for `enc:` secrets (MFA factors): derived from
/// env `GOWIND_CRYPTO_KEY`; unset ⇒ factors are stored in plaintext.
pub fn crypto_key() -> Option<[u8; 32]> {
    let raw = std::env::var("GOWIND_CRYPTO_KEY").ok()?;
    let mut key = [0u8; 32];
    let bytes = raw.as_bytes();
    key[..bytes.len().min(32)].copy_from_slice(&bytes[..bytes.len().min(32)]);
    Some(key)
}

/// Decrypts the login credential: base64(AES-128-CBC(key, iv=key, PKCS7)).
#[allow(dead_code)] // wired with the encrypted-credential login branch
pub fn decrypt_login_credential(plain_credential: &str) -> Result<String, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(plain_credential.trim())
        .map_err(|e| format!("credential base64: {e}"))?;
    decrypt_aes_cbc(&bytes).ok_or_else(|| "credential decrypt".to_string())
}

/// AES-128-CBC decrypt with the DefaultAESKey pair (bytes in). The
/// cipher build lacks the `alloc` pairing helper, so the buffer form runs
/// explicitly.
pub fn decrypt_aes_cbc(ciphertext: &[u8]) -> Option<String> {
    let mut out = vec![0u8; ciphertext.len()];
    let len = Aes128CbcDec::new(DEFAULT_AES_KEY.into(), DEFAULT_AES_KEY.into())
        .decrypt_padded_b2b_mut::<Pkcs7>(ciphertext, &mut out)
        .ok()?
        .len();
    out.truncate(len);
    String::from_utf8(out).ok()
}

/// bcrypt hash at default cost.
pub fn hash_password(password: &str) -> Result<String, String> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|e| format!("bcrypt: {e}"))
}

/// bcrypt verify — the raw `$2a$…` stored string.
pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// The timing-equalizer dummy hash verifies on the
/// user-not-found paths .
pub const DUMMY_PASSWORD_HASH: &str =
    "$2a$10$1sbpKmhQDpXLHnDnEQ1nLe3oOnYyP2bUJyqHcX2T0Fq1qfyoXOrPm";

pub fn dummy_verify() {
    let _ = bcrypt::verify("definitely-not-the-password", DUMMY_PASSWORD_HASH);
}

/// SHA-256 hex — the access-key secret storage form.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// AES-256-GCM encrypt with the `enc:` envelope (MFA secret at-rest).
/// Random 12-byte nonce prepended to the ciphertext, then base64.
pub fn encrypt_if_needed(plain: &str) -> String {
    match crypto_key() {
        None => plain.to_string(),
        Some(key) => {
            use aes_gcm::aead::Aead;
            use aes_gcm::Aes256Gcm;
            use aes_gcm::KeyInit;
            let cipher = Aes256Gcm::new((&key).into());
            let nonce_bytes = rand::random::<[u8; 12]>();
            let ct = cipher
                .encrypt((&nonce_bytes).into(), plain.as_bytes())
                .expect("aes-gcm encrypt");
            let mut packed = nonce_bytes.to_vec();
            packed.extend(ct);
            use base64::Engine as _;
            format!(
                "enc:{}",
                base64::engine::general_purpose::STANDARD.encode(packed)
            )
        }
    }
}

/// Reverses [`encrypt_if_needed`].
pub fn decrypt_if_needed(stored: &str) -> Result<String, String> {
    let Some(stripped) = stored.strip_prefix("enc:") else {
        return Ok(stored.to_string());
    };
    let key = crypto_key().ok_or("GOWIND_CRYPTO_KEY unset but secret is encrypted")?;
    use base64::Engine as _;
    let packed = base64::engine::general_purpose::STANDARD
        .decode(stripped)
        .map_err(|e| format!("secret base64: {e}"))?;
    if packed.len() < 13 {
        return Err("secret too short".into());
    }
    use aes_gcm::aead::Aead;
    use aes_gcm::Aes256Gcm;
    use aes_gcm::KeyInit;
    let cipher = Aes256Gcm::new((&key).into());
    let plain = cipher
        .decrypt((&packed[..12]).into(), &packed[12..])
        .map_err(|_| "secret decrypt".to_string())?;
    String::from_utf8(plain).map_err(|_| "secret utf8".to_string())
}

/// TOTP verify (RFC 6238, SHA-1, 6 digits, 30 s step, ±1 window) — the
/// reference's `pquerna/otp` `ValidateCustom` parameters.
pub fn totp_verify(secret_base32: &str, code: &str) -> bool {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let secret = match base32_decode(secret_base32) {
        Some(s) => s,
        None => return false,
    };
    let now = chrono::Utc::now().timestamp();
    for drift in [-1i64, 0, 1] {
        let counter = (now / 30 + drift) as u64;
        if totp_at(&secret, counter) == code {
            return true;
        }
    }
    false
}

fn totp_at(secret: &[u8], counter: u64) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<Sha1>::new_from_slice(secret).expect("hmac sha1");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary = ((digest[offset] as u32 & 0x7f) << 24)
        | ((digest[offset + 1] as u32) << 16)
        | ((digest[offset + 2] as u32) << 8)
        | digest[offset + 3] as u32;
    format!("{:06}", binary % 1_000_000)
}

use sha1::Sha1;

/// RFC 4648 base32 (no padding), the otpauth secret alphabet.
pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits = 0u32;
    let mut acc = 0u32;
    let mut out = Vec::new();
    for ch in input.trim_end_matches('=').bytes() {
        let upper = ch.to_ascii_uppercase();
        let value = ALPHABET.iter().position(|&a| a == upper)? as u32;
        acc = (acc << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut bits = 0u32;
    let mut acc = 0u32;
    for &byte in data {
        acc = (acc << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[(acc >> bits) as usize & 31] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[(acc << (5 - bits)) as usize & 31] as char);
    }
    out
}
