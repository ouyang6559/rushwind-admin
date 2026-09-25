//! Upstream-facing signature helpers — the platform → channel side of the
//! wire, which is a DIFFERENT family from the merchant → platform
//! [`crate::gateway::sign::create_sign`] (that one is uppercase + a `&key=`
//! suffix). Each legacy channel controller rolled its own; these reproduce
//! the two dominant styles byte-for-byte so the sample adapters and their
//! notify verification stay line-compatible with the PHP.
//!
//! All of them share the PHP loose-`==` "empty" rule (drop `""` and the
//! string `"0"`, plus the named sign keys) and `ksort` ascending byte order.

use std::collections::BTreeMap;

/// PHP `!empty` on a form value: `""` and `"0"` are empty (loose `==`).
pub fn php_empty(v: &str) -> bool {
    v.is_empty() || v == "0"
}

/// Lowercase hex MD5 (`md5(...)` in PHP, no `strtoupper`).
pub fn md5_hex_lower(bytes: &[u8]) -> String {
    format!("{:x}", md5::compute(bytes))
}

/// The 易支付 (`submit.php`) sign used by `WxSmController`:
/// `ksort` → skip `sign`/`sign_type`/empty → `k=v&` → `rtrim('&')` → append
/// the secret DIRECTLY (no `&key=`) → lowercase `md5`.
///
/// `params` is a key map; iteration is already ascending (BTreeMap).
pub fn easy_pay_sign(params: &BTreeMap<String, String>, secret: &str) -> String {
    let mut link = String::new();
    let mut first = true;
    for (k, v) in params {
        if k == "sign" || k == "sign_type" || php_empty(v) {
            continue;
        }
        if !first {
            link.push('&');
        }
        link.push_str(k);
        link.push('=');
        link.push_str(v);
        first = false;
    }
    link.push_str(secret);
    md5_hex_lower(link.as_bytes())
}

/// Joins `(k, v)` pairs (already sorted) as `k=v&k=v…` (no trailing `&`).
pub fn link_string(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The 睿支付 (`RzfkjController`) sign: drop `signature`/empty, merge the
/// secret in as a NORMAL sorted parameter (`pubKey=<secret>`), `ksort`,
/// join `k=v&`, lowercase `md5` — i.e. the secret rides inside the sorted
/// set rather than as a suffix. `extra` is the `(key, secret)` pair.
pub fn pubkey_link_sign(
    params: &BTreeMap<String, String>,
    extra_key: &str,
    secret: &str,
) -> String {
    let mut sorted: BTreeMap<&str, &str> = params
        .iter()
        .filter(|(k, v)| *k != "signature" && !php_empty(v))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sorted.insert(extra_key, secret);
    let pairs: Vec<(&str, &str)> = sorted.into_iter().collect();
    md5_hex_lower(link_string(&pairs).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn md5_is_lowercase_hex() {
        assert_eq!(md5_hex_lower(b""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn easy_pay_sign_reproduces_the_php_layout() {
        // Mirrors WxSmController::Pay: ksort, skip sign/sign_type/empty, then
        // append the secret with NO separator, lowercase md5.
        let params = map(&[
            ("pid", "858580"),
            ("type", "wxpay"),
            ("out_trade_no", "P20260919001"),
            ("notify_url", "http://p/notify"),
            ("return_url", "http://p/back"),
            ("name", "pay"),
            ("money", "100.00"),
            ("sitename", ""),     // dropped (empty)
            ("sign", ""),         // dropped (sign key)
            ("sign_type", "MD5"), // dropped (sign_type key)
        ]);
        let got = easy_pay_sign(&params, "SECRET");
        let expected_link = "money=100.00&name=pay&notify_url=http://p/notify\
                             &out_trade_no=P20260919001&pid=858580&return_url=http://p/back\
                             &type=wxpaySECRET";
        assert_eq!(got, md5_hex_lower(expected_link.as_bytes()));
    }

    #[test]
    fn easy_pay_sign_drops_zero_valued_params() {
        let a = map(&[("x", "0"), ("y", "1")]);
        let b = map(&[("y", "1")]);
        assert_eq!(easy_pay_sign(&a, "K"), easy_pay_sign(&b, "K"));
    }

    #[test]
    fn pubkey_link_sign_sorts_secret_as_a_param() {
        // The pubKey lands inside the sorted set (its ASCII position matters).
        let params = map(&[
            ("sp_userid", "10001"),
            ("money", "100"),
            ("signature", "IGNORED"), // dropped
            ("memo", ""),             // dropped (empty)
        ]);
        let got = pubkey_link_sign(&params, "pubKey", "TOPSECRET");
        // sorted keys: money, pubKey, sp_userid → "money=100&pubKey=TOPSECRET&sp_userid=10001"
        let expected = md5_hex_lower(b"money=100&pubKey=TOPSECRET&sp_userid=10001");
        assert_eq!(got, expected);
    }

    #[test]
    fn link_string_has_no_trailing_amp() {
        assert_eq!(link_string(&[("a", "1"), ("b", "2")]), "a=1&b=2");
        assert_eq!(link_string(&[]), "");
    }
}
