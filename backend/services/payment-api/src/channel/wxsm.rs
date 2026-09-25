//! The `WxSm` sample adapter — a 易支付 (`submit.php`) aggregate channel
//! (`juhepay/Application/Pay/Controller/WxSmController.class.php`). It shows
//! the contract carrying a pure redirect flow whose upstream request is
//! signed with the 易支付 MD5 style ([`crate::channel::sign::easy_pay_sign`])
//! and whose async notify is verified with the same rule.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::channel::sign::easy_pay_sign;
use crate::channel::{
    CallbackReq, Channel, ChannelCred, ChannelError, NotifyOk, OrderCtx, PayCtx, PayOut,
};
use crate::money::units_to_yuan_2dp;

/// The upstream submit endpoint used when the account sets no `gateway`.
const DEFAULT_GATEWAY: &str = "https://pay.pinyewang.com/submit.php";

pub struct WxSm;

impl WxSm {
    /// The ordered request params (URL order matches the PHP `$data` array;
    /// the signature is taken over the sorted subset).
    fn request_params(order: &OrderCtx, cred: &ChannelCred) -> Vec<(String, String)> {
        vec![
            ("pid".into(), cred.mch_id.clone()),
            ("type".into(), "wxpay".into()),
            ("out_trade_no".into(), order.order_id.clone()),
            ("notify_url".into(), order.notify_url.clone()),
            ("return_url".into(), order.callback_url.clone()),
            ("name".into(), order.subject.clone()),
            ("money".into(), units_to_yuan_2dp(order.amount_units)),
            ("sitename".into(), String::new()),
            ("sign".into(), String::new()),
            ("sign_type".into(), "MD5".into()),
        ]
    }
}

#[async_trait]
impl Channel for WxSm {
    fn code(&self) -> &'static str {
        "WxSm"
    }

    fn notify_order_id(&self, req: &CallbackReq) -> Option<String> {
        req.form
            .get("out_trade_no")
            .filter(|s| !s.is_empty())
            .cloned()
    }

    async fn pay(&self, ctx: &PayCtx<'_>) -> Result<PayOut, ChannelError> {
        let params = Self::request_params(ctx.order, ctx.cred);

        // Sign over the sorted, non-empty subset (skip sign/sign_type).
        let mut sign_map: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in &params {
            sign_map.insert(k.clone(), v.clone());
        }
        let sign = easy_pay_sign(&sign_map, &ctx.cred.sign_key);

        // Rebuild the ordered query with the signature filled in.
        let query = params
            .iter()
            .map(|(k, v)| {
                let value = if k == "sign" { &sign } else { v };
                format!("{k}={value}")
            })
            .collect::<Vec<_>>()
            .join("&");

        let base = if ctx.cred.gateway.is_empty() {
            DEFAULT_GATEWAY.to_string()
        } else {
            ctx.cred.gateway.clone()
        };
        Ok(PayOut::Redirect {
            url: format!("{base}?{query}"),
        })
    }

    fn verify_notify(
        &self,
        cred: &ChannelCred,
        req: &CallbackReq,
    ) -> Result<NotifyOk, ChannelError> {
        let form = &req.form;
        let get = |k: &str| form.get(k).cloned().unwrap_or_default();

        let expected = easy_pay_sign(form, &cred.sign_key);
        if !expected.eq_ignore_ascii_case(&get("sign")) {
            return Err(ChannelError::Signature("wxsm notify".into()));
        }

        let success = get("trade_status") == "TRADE_SUCCESS";
        Ok(NotifyOk {
            platform_order_id: get("out_trade_no"),
            upstream_txn: get("trade_no"),
            success,
            // The PHP writes `success` only on a verified trade; a verified
            // but non-success message is acknowledged plainly.
            ack: if success {
                "success".into()
            } else {
                "fail".into()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::sign::md5_hex_lower;

    fn ctx<'a>(order: &'a OrderCtx, cred: &'a ChannelCred) -> PayCtx<'a> {
        PayCtx { order, cred }
    }

    fn order() -> OrderCtx {
        OrderCtx {
            order_id: "P20260919001".into(),
            merchant_order_id: "M-1".into(),
            amount_units: 1_000_000, // 100.00元
            subject: "gift".into(),
            notify_url: "http://p/notify".into(),
            callback_url: "http://p/back".into(),
        }
    }

    fn cred() -> ChannelCred {
        ChannelCred {
            mch_id: "858580".into(),
            sign_key: "UzBGGG".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pay_redirect_signs_and_carries_all_params() {
        let o = order();
        let c = cred();
        let out = WxSm.pay(&ctx(&o, &c)).await.unwrap();
        let url = match out {
            PayOut::Redirect { url } => url,
            other => panic!("expected redirect, got {other:?}"),
        };
        assert!(url.starts_with(DEFAULT_GATEWAY));
        assert!(url.contains("out_trade_no=P20260919001"));
        assert!(url.contains("money=100.00"));
        assert!(url.contains("sign_type=MD5"));
        // The signature equals the recomputed easy-pay sign over the subset.
        let mut map = BTreeMap::new();
        for (k, v) in WxSm::request_params(&o, &c) {
            map.insert(k, v);
        }
        let sign = easy_pay_sign(&map, "UzBGGG");
        assert!(url.contains(&format!("sign={sign}")));
    }

    #[test]
    fn verify_notify_accepts_a_well_signed_success() {
        let mut form = BTreeMap::new();
        form.insert("pid".into(), "858580".into());
        form.insert("trade_no".into(), "UP123".into());
        form.insert("out_trade_no".into(), "P20260919001".into());
        form.insert("type".into(), "wxpay".into());
        form.insert("name".into(), "gift".into());
        form.insert("money".into(), "100.00".into());
        form.insert("trade_status".into(), "TRADE_SUCCESS".into());
        form.insert("sign_type".into(), "MD5".into());
        let sign = easy_pay_sign(&form, "UzBGGG");
        form.insert("sign".into(), sign);

        let ok = WxSm
            .verify_notify(
                &cred(),
                &CallbackReq {
                    form,
                    raw_body: String::new(),
                },
            )
            .unwrap();
        assert!(ok.success);
        assert_eq!(ok.platform_order_id, "P20260919001");
        assert_eq!(ok.upstream_txn, "UP123");
        assert_eq!(ok.ack, "success");
    }

    #[test]
    fn verify_notify_rejects_a_bad_signature() {
        let mut form = BTreeMap::new();
        form.insert("out_trade_no".into(), "P1".into());
        form.insert("trade_status".into(), "TRADE_SUCCESS".into());
        form.insert("sign".into(), md5_hex_lower(b"tampered"));
        let err = WxSm
            .verify_notify(
                &cred(),
                &CallbackReq {
                    form,
                    raw_body: String::new(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, ChannelError::Signature(_)));
    }
}
