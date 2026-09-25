//! The `Rzfkj` sample adapter — 睿支付银联快捷 bank quick-pay
//! (`juhepay/Application/Pay/Controller/RzfkjController.class.php`). It
//! proves the contract carries a channel with a DIFFERENT signing style from
//! `WxSm`: the secret is merged in as a normal sorted parameter
//! (`pubKey=<secret>`) and hashed over the sorted link string
//! ([`crate::channel::sign::pubkey_link_sign`]), while the URL query keeps
//! the original insertion order. Amount is 分-denominated (`exchange = 100`).

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::channel::sign::pubkey_link_sign;
use crate::channel::{
    CallbackReq, Channel, ChannelCred, ChannelError, NotifyOk, OrderCtx, PayCtx, PayOut,
};
use crate::money::units_to_fen;

/// The upstream card-pay endpoint used when the account sets no `gateway`.
const DEFAULT_GATEWAY: &str = "https://www.jrpay.net/Jrpay/tfb8Req/tfb8Req_doCardpayApplyApi";

pub struct Rzfkj;

impl Rzfkj {
    /// The ordered request params (URL order = PHP `$parameter` literal; the
    /// `signature` is appended after the query, not part of the signed set
    /// until the notify path).
    fn request_params(order: &OrderCtx, cred: &ChannelCred) -> Vec<(String, String)> {
        vec![
            ("spbillno".into(), order.order_id.clone()),
            ("sp_userid".into(), cred.mch_id.clone()),
            ("money".into(), units_to_fen(order.amount_units).to_string()),
            ("memo".into(), order.subject.clone()),
            ("productId".into(), "wapApply".into()),
            ("card_type".into(), "1".into()),
            ("user_type".into(), "1".into()),
            ("channel".into(), "2".into()),
            ("return_url".into(), order.callback_url.clone()),
            ("notify_url".into(), order.notify_url.clone()),
        ]
    }
}

#[async_trait]
impl Channel for Rzfkj {
    fn code(&self) -> &'static str {
        "Rzfkj"
    }

    fn notify_order_id(&self, req: &CallbackReq) -> Option<String> {
        req.form.get("spbillno").filter(|s| !s.is_empty()).cloned()
    }

    async fn pay(&self, ctx: &PayCtx<'_>) -> Result<PayOut, ChannelError> {
        let params = Self::request_params(ctx.order, ctx.cred);

        // URL query: insertion-order `k=v&…` of the non-empty params.
        let prestr = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");

        // Signature: sorted (filtered params + pubKey=<secret>) link, md5.
        let mut sign_map: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in &params {
            sign_map.insert(k.clone(), v.clone());
        }
        let signature = pubkey_link_sign(&sign_map, "pubKey", &ctx.cred.sign_key);

        let base = if ctx.cred.gateway.is_empty() {
            DEFAULT_GATEWAY.to_string()
        } else {
            ctx.cred.gateway.clone()
        };
        Ok(PayOut::Redirect {
            url: format!("{base}?{prestr}&signature={signature}"),
        })
    }

    fn verify_notify(
        &self,
        cred: &ChannelCred,
        req: &CallbackReq,
    ) -> Result<NotifyOk, ChannelError> {
        let form = &req.form;
        let get = |k: &str| form.get(k).cloned().unwrap_or_default();

        let expected = pubkey_link_sign(form, "pubKey", &cred.sign_key);
        if !expected.eq_ignore_ascii_case(&get("signature")) {
            return Err(ChannelError::Signature("rzfkj notify".into()));
        }

        let success = get("result") == "1";
        Ok(NotifyOk {
            platform_order_id: get("spbillno"),
            upstream_txn: get("orderno"),
            success,
            ack: if success {
                "SUCCESS".into()
            } else {
                "FAIL".into()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::channel::sign::md5_hex_lower;

    fn order() -> OrderCtx {
        OrderCtx {
            order_id: "P9".into(),
            merchant_order_id: "M9".into(),
            amount_units: 1_000_000, // 100元 → 10000 分
            subject: "order".into(),
            notify_url: "http://p/n".into(),
            callback_url: "http://p/c".into(),
        }
    }

    fn cred() -> ChannelCred {
        ChannelCred {
            mch_id: "10001".into(),
            sign_key: "PUB".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pay_redirect_uses_fen_and_pubkey_signature() {
        let o = order();
        let c = cred();
        let ctx = PayCtx {
            order: &o,
            cred: &c,
        };
        let out = Rzfkj.pay(&ctx).await.unwrap();
        let url = match out {
            PayOut::Redirect { url } => url,
            other => panic!("expected redirect, got {other:?}"),
        };
        assert!(url.contains("money=10000"));
        // signature is appended, and equals the recomputed pubkey-link sign.
        let mut map = BTreeMap::new();
        for (k, v) in Rzfkj::request_params(&o, &c) {
            map.insert(k, v);
        }
        let sig = pubkey_link_sign(&map, "pubKey", "PUB");
        assert!(url.ends_with(&format!("&signature={sig}")));
    }

    #[test]
    fn verify_notify_accepts_signed_success() {
        let mut form = BTreeMap::new();
        form.insert("spbillno".into(), "P9".into());
        form.insert("result".into(), "1".into());
        form.insert("orderno".into(), "UP-9".into());
        let sig = pubkey_link_sign(&form, "pubKey", "PUB");
        form.insert("signature".into(), sig);

        let req = CallbackReq {
            form,
            raw_body: String::new(),
        };
        let ok = Rzfkj.verify_notify(&cred(), &req).unwrap();
        assert!(ok.success);
        assert_eq!(ok.platform_order_id, "P9");
        assert_eq!(ok.ack, "SUCCESS");
    }

    #[test]
    fn verify_notify_rejects_bad_signature() {
        let mut form = BTreeMap::new();
        form.insert("spbillno".into(), "P9".into());
        form.insert("result".into(), "1".into());
        form.insert("signature".into(), md5_hex_lower(b"nope"));
        let req = CallbackReq {
            form,
            raw_body: String::new(),
        };
        assert!(matches!(
            Rzfkj.verify_notify(&cred(), &req),
            Err(ChannelError::Signature(_))
        ));
    }
}
