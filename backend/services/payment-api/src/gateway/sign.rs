//! The `pay_md5sign` algorithm, byte-for-byte compatible with the legacy
//! `PayController::createSign` (`juhepay/Application/Pay/Controller/
//! PayController.class.php:686-697`, re-stated in `spec/03-pay-gateway-
//! channel.md` §3.1).
//!
//! Steps (exactly as PHP):
//! 1. filter — keep only pairs whose value is PHP-`!empty` (drop `""` and
//!    the string `"0"`);
//! 2. sort — ascending by key, byte (ASCII) order (`ksort`);
//! 3. join — `k=v&` for each surviving pair (a trailing `&` is left on);
//! 4. append — `key=<secret>` directly (`...&key=SECRET`, no extra `&`);
//! 5. hash — `strtoupper(md5(...))`.

use std::collections::BTreeMap;

/// The merchant-facing signed field set for the unified order (the seven
/// `pay_*` params that participate in the signature; `pay_md5sign` itself
/// and the unsigned `pay_productname`/`pay_attach`/`ddlx` are excluded —
/// see `spec/03` §2.4 note).
pub const ORDER_SIGN_FIELDS: [&str; 7] = [
    "pay_memberid",
    "pay_orderid",
    "pay_amount",
    "pay_bankcode",
    "pay_applydate",
    "pay_notifyurl",
    "pay_callbackurl",
];

/// PHP `!empty` on a form string value: `""` and `"0"` are empty.
fn php_empty(v: &str) -> bool {
    v.is_empty() || v == "0"
}

/// Builds the sign-from fields out of a raw submitted form, restricted to
/// `fields`, dropping empty values, then signs. This mirrors how
/// `IndexController::verify` assembles `$requestarray` from `I('request.*')`.
pub fn sign_from_form(form: &BTreeMap<String, String>, fields: &[&str], secret: &str) -> String {
    let mut picked: Vec<(&str, &str)> = Vec::new();
    for &f in fields {
        if let Some(v) = form.get(f) {
            if !php_empty(v) {
                picked.push((f, v.as_str()));
            }
        }
    }
    create_sign(secret, picked)
}

/// `createSign($Md5key, $list)`: ksort + `k=v&`(non-empty) + `key=<secret>`
/// + `strtoupper(md5(..))`. `list` keys sort ascending via `BTreeMap`.
pub fn create_sign<'a, I>(secret: &str, list: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let sorted: BTreeMap<&str, &str> = list.into_iter().filter(|(_, v)| !php_empty(v)).collect();
    let mut link = String::new();
    for (k, v) in &sorted {
        link.push_str(k);
        link.push('=');
        link.push_str(v);
        link.push('&');
    }
    link.push_str("key=");
    link.push_str(secret);
    md5_upper(link.as_bytes())
}

/// Lowercase-then-uppercase hex MD5 of raw bytes.
pub fn md5_upper(bytes: &[u8]) -> String {
    let digest = md5::compute(bytes);
    format!("{:X}", digest)
}

/// Strict equality check (PHP `==` on the two uppercase hex strings).
pub fn verify_sign(expected: &str, computed: &str) -> bool {
    expected.eq_ignore_ascii_case(computed)
}

/// The payout-API `DfpayController::createSign` (`spec/04` §7.3): it signs
/// the WHOLE field map (unlike the payment side's fixed [`ORDER_SIGN_FIELDS`]
/// whitelist), dropping `pay_md5sign` and PHP-empty values. Reused both to
/// verify a submitted form (`add` signs the entire `$_POST`) and to sign the
/// query reply map (§7.5, where `pay_md5sign` is simply absent).
pub fn sign_pairs<'a, I>(secret: &str, list: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    create_sign(
        secret,
        list.into_iter().filter(|(k, _)| *k != "pay_md5sign"),
    )
}

/// `verify($_POST)` for the payout `add` entry: recompute the signature over
/// every posted key except `pay_md5sign` — any extra field the merchant
/// carries participates, exactly like the legacy forwarding `$_POST`.
pub fn sign_form_all(secret: &str, form: &BTreeMap<String, String>) -> String {
    sign_pairs(secret, form.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

/// The payout query REQUEST signature (§7.5): the legacy rebuilds the sign
/// from ONLY `{mchid, out_trade_no}` — not the whole request — before the
/// `pay_md5sign` compare.
pub fn sign_query_request(secret: &str, mch_id: &str, out_trade_no: &str) -> String {
    create_sign(secret, [("mchid", mch_id), ("out_trade_no", out_trade_no)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vectors() {
        // RFC 1321 vectors, uppercase.
        assert_eq!(md5_upper(b""), "D41D8CD98F00B204E9800998ECF8427E");
        assert_eq!(
            md5_upper(b"The quick brown fox jumps over the lazy dog"),
            "9E107D9D372BB6826BD81D3542A419D6"
        );
    }

    #[test]
    fn create_sign_matches_hand_computed_layout() {
        // ksort puts pay_amount before pay_memberid before pay_orderid; the
        // empty bankcode is dropped; the link string ends "&key=SECRET".
        let list = [
            ("pay_orderid", "ORD1"),
            ("pay_amount", "1.00"),
            ("pay_memberid", "10001"),
            ("pay_bankcode", ""), // dropped (empty)
        ];
        let got = create_sign("SECRET", list);
        // link = "pay_amount=1.00&pay_memberid=10001&pay_orderid=ORD1&key=SECRET"
        let want = md5_upper(b"pay_amount=1.00&pay_memberid=10001&pay_orderid=ORD1&key=SECRET");
        assert_eq!(got, want);
    }

    #[test]
    fn zero_string_value_is_dropped_like_php() {
        let with_zero = create_sign("K", [("a", "0"), ("b", "1")]);
        let without = create_sign("K", [("b", "1")]);
        assert_eq!(with_zero, without);
    }

    #[test]
    fn sign_from_form_recomputes_a_sample_order() {
        let mut form = BTreeMap::new();
        form.insert("pay_memberid".into(), "10001".into());
        form.insert("pay_orderid".into(), "20260919001".into());
        form.insert("pay_amount".into(), "100.00".into());
        form.insert("pay_bankcode".into(), "12".into());
        form.insert("pay_applydate".into(), "2026-09-19 10:00:00".into());
        form.insert("pay_notifyurl".into(), "http://m/notify".into());
        form.insert("pay_callbackurl".into(), "http://m/back".into());
        // unsigned extras must not affect the signature
        form.insert("pay_productname".into(), "gift".into());
        form.insert("pay_md5sign".into(), "IGNORED".into());

        let sign = sign_from_form(
            &form,
            &ORDER_SIGN_FIELDS,
            "32charapikey000000000000000000aa",
        );
        let link = "pay_amount=100.00&pay_applydate=2026-09-19 10:00:00&pay_bankcode=12&\
                    pay_callbackurl=http://m/back&pay_memberid=10001&pay_notifyurl=http://m/notify&\
                    pay_orderid=20260919001&key=32charapikey000000000000000000aa";
        assert_eq!(sign, md5_upper(link.as_bytes()));
    }

    #[test]
    fn sign_form_all_covers_every_key_but_the_signature() {
        // The payout `add` verify signs the WHOLE form (mchid + money + a
        // merchant-specific extra), skipping only pay_md5sign and empties.
        let mut form = BTreeMap::new();
        form.insert("mchid".into(), "10001".into());
        form.insert("money".into(), "100.00".into());
        form.insert("province".into(), "北京".into());
        form.insert("empty_field".into(), "".into()); // dropped (PHP empty)
        form.insert("pay_md5sign".into(), "SHOULD_SKIP".into());
        let got = sign_form_all("K", &form);
        let link = "mchid=10001&money=100.00&province=北京&key=K";
        assert_eq!(got, md5_upper(link.as_bytes()));
    }

    #[test]
    fn sign_query_request_only_covers_mchid_and_order_no() {
        // §7.5: the request verify rebuilds the sign over {mchid, out_trade_no}
        // alone — pay_md5sign / other request keys never participate.
        let got = sign_query_request("K", "10001", "DF-1");
        let link = "mchid=10001&out_trade_no=DF-1&key=K";
        assert_eq!(got, md5_upper(link.as_bytes()));
    }

    #[test]
    fn sign_pairs_matches_reply_layout_and_drops_sign_key() {
        // The query REPLY sign runs over the whole response map (already free
        // of pay_md5sign); a stray pay_md5sign must be filtered like PHP.
        let reply = [
            ("status", "success"),
            ("msg", "请求成功"),
            ("refCode", "1"),
            ("pay_md5sign", "IGNORED"),
        ];
        let got = sign_pairs("K", reply);
        let link = "msg=请求成功&refCode=1&status=success&key=K";
        assert_eq!(got, md5_upper(link.as_bytes()));
    }
}
