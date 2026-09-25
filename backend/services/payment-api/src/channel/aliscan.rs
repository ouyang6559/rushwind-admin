//! The `Aliscan` sample adapter — 支付宝官方扫码 (`juhepay/Application/Pay/
//! Controller/AliscanController.class.php`). It exercises the third
//! [`PayOut`] shape (a QR-code page) and the simplest signing family: an
//! MD5 over `id . secret` (a fixed-key suffix hash), lowercase, verified on
//! the async notify by `respCode == 0000`.
//!
//! The live PHP performs an upstream JSON POST to obtain the `payUrl` before
//! rendering the QR. `pay` runs that POST for real (`merchantId` / `tranType`
//! / `merReqNo` / `pordInfo` / `amt`(分) / `notifyUrl` / `returnUrl` / `sign`)
//! and renders the QR page from the replied `payUrl`; `respCode != 0000`
//! renders the upstream `respDesc` verbatim (the legacy `exit($respDesc)`)
//! and leaves the order unpaid for reissue. An unconfigured `gateway` is an
//! upstream error — the legacy hard-coded a placeholder host, the rewrite
//! refuses to guess.

use std::time::Duration;

use async_trait::async_trait;

use crate::channel::sign::md5_hex_lower;
use crate::channel::{
    CallbackReq, Channel, ChannelCred, ChannelError, NotifyOk, OrderCtx, PayCtx, PayOut,
};
use crate::money::units_to_fen;

/// The per-request upstream timeout (same discipline as the payout adapters:
/// fail the drive rather than stall the dispatch).
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Aliscan;

impl Aliscan {
    fn scan_params(order: &OrderCtx, cred: &ChannelCred) -> Vec<(String, String)> {
        vec![
            ("merchantId".into(), cred.mch_id.clone()),
            ("tranType".into(), "1002".into()),
            ("merReqNo".into(), order.order_id.clone()),
            ("pordInfo".into(), order.subject.clone()),
            ("amt".into(), units_to_fen(order.amount_units).to_string()),
            ("notifyUrl".into(), order.notify_url.clone()),
            ("returnUrl".into(), order.callback_url.clone()),
        ]
    }

    /// `md5(<id> . <secret>)`, lowercase — the fixed-key hash the channel
    /// uses for both the request `sign` and the notify verification.
    fn sign(id: &str, secret: &str) -> String {
        md5_hex_lower(format!("{id}{secret}").as_bytes())
    }
}

#[async_trait]
impl Channel for Aliscan {
    fn code(&self) -> &'static str {
        "Aliscan"
    }

    fn notify_order_id(&self, req: &CallbackReq) -> Option<String> {
        req.form.get("merReqNo").filter(|s| !s.is_empty()).cloned()
    }

    async fn pay(&self, ctx: &PayCtx<'_>) -> Result<PayOut, ChannelError> {
        if ctx.cred.gateway.is_empty() {
            return Err(ChannelError::Upstream(
                "aliscan gateway not configured".into(),
            ));
        }
        let params = Self::scan_params(ctx.order, ctx.cred);
        let sign = Self::sign(&ctx.order.order_id, &ctx.cred.sign_key);
        let mut body: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for (k, v) in &params {
            body.insert(k, v);
        }
        body.insert("sign", &sign);

        let client = reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .map_err(|e| ChannelError::Upstream(format!("aliscan client: {e}")))?;
        let reply = client
            .post(&ctx.cred.gateway)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                serde_json::to_string(&body)
                    .map_err(|e| ChannelError::Upstream(format!("aliscan encode: {e}")))?,
            )
            .send()
            .await
            .map_err(|e| ChannelError::Upstream(format!("aliscan post: {e}")))?;
        let text = reply
            .text()
            .await
            .map_err(|e| ChannelError::Upstream(format!("aliscan read: {e}")))?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ChannelError::Upstream(format!("aliscan decode: {e}")))?;

        if json.get("respCode").and_then(|v| v.as_str()) != Some("0000") {
            // The legacy rendered the upstream's own failure description.
            let desc = json
                .get("respDesc")
                .and_then(|v| v.as_str())
                .unwrap_or("respCode error")
                .to_string();
            return Ok(PayOut::Raw(desc));
        }
        let pay_url = json
            .get("payUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChannelError::Upstream("aliscan reply missing payUrl".into()))?;
        Ok(PayOut::QrCode {
            url: pay_url.to_string(),
        })
    }

    fn verify_notify(
        &self,
        cred: &ChannelCred,
        req: &CallbackReq,
    ) -> Result<NotifyOk, ChannelError> {
        let form = &req.form;
        let get = |k: &str| form.get(k).cloned().unwrap_or_default();

        let expected = Self::sign(&get("serverRspNo"), &cred.sign_key);
        if !expected.eq_ignore_ascii_case(&get("sign")) {
            return Err(ChannelError::Signature("aliscan notify".into()));
        }

        let success = get("respCode") == "0000";
        Ok(NotifyOk {
            platform_order_id: get("merReqNo"),
            upstream_txn: get("serverRspNo"),
            success,
            ack: if success {
                "success".into()
            } else {
                "trade fail".into()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};

    use super::*;

    fn order() -> OrderCtx {
        OrderCtx {
            order_id: "P5".into(),
            merchant_order_id: "M5".into(),
            amount_units: 1_000_000, // 100元 → 10000 分
            subject: "团购商品".into(),
            notify_url: "http://p/n".into(),
            callback_url: "http://p/c".into(),
        }
    }

    fn cred() -> ChannelCred {
        ChannelCred {
            mch_id: "554444".into(),
            sign_key: "123456".into(),
            ..Default::default()
        }
    }

    /// A one-shot loopback upstream: answers one POST with `reply` (as a
    /// JSON 200) and hands back the raw request bytes for assertions.
    fn mock_upstream(reply: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock bind");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("mock accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let headers = String::from_utf8_lossy(&buf).to_lowercase();
            let want = headers
                .split("content-length:")
                .nth(1)
                .and_then(|s| {
                    s.trim_start()
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse::<usize>()
                        .ok()
                })
                .unwrap_or(0);
            let have = buf.len().saturating_sub(
                buf.windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .unwrap_or(buf.len()),
            );
            let mut missing = want.saturating_sub(have);
            while missing > 0 {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        missing = missing.saturating_sub(n);
                    }
                }
            }
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                reply.len(),
                reply
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
            buf
        });
        (format!("http://127.0.0.1:{port}/pay"), handle)
    }

    #[tokio::test]
    async fn pay_posts_json_and_renders_the_replied_qr_url() {
        let (url, handle) =
            mock_upstream(r#"{"respCode":"0000","payUrl":"https://qr.example/abc"}"#);
        let o = order();
        let mut c = cred();
        c.gateway = url;
        let ctx = PayCtx {
            order: &o,
            cred: &c,
        };

        let out = Aliscan.pay(&ctx).await.unwrap();
        match out {
            PayOut::QrCode { url } => assert_eq!(url, "https://qr.example/abc"),
            other => panic!("expected qrcode, got {other:?}"),
        }

        // The wire request is the legacy JSON curl set, sign included.
        let raw = handle.join().unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["merchantId"], "554444");
        assert_eq!(json["tranType"], "1002");
        assert_eq!(json["merReqNo"], "P5");
        assert_eq!(json["amt"], "10000");
        assert_eq!(json["sign"], Aliscan::sign("P5", "123456"));
    }

    #[tokio::test]
    async fn pay_renders_the_upstream_failure_description() {
        let (url, handle) = mock_upstream(r#"{"respCode":"9999","respDesc":"余额不足"}"#);
        let o = order();
        let mut c = cred();
        c.gateway = url;
        let ctx = PayCtx {
            order: &o,
            cred: &c,
        };

        let out = Aliscan.pay(&ctx).await.unwrap();
        match out {
            PayOut::Raw(desc) => assert_eq!(desc, "余额不足"),
            other => panic!("expected raw, got {other:?}"),
        }
        drop(handle);
    }

    #[tokio::test]
    async fn pay_without_a_gateway_is_an_upstream_error() {
        let o = order();
        let c = cred(); // gateway left empty
        assert!(matches!(
            Aliscan
                .pay(&PayCtx {
                    order: &o,
                    cred: &c
                })
                .await,
            Err(ChannelError::Upstream(_))
        ));
    }

    #[test]
    fn verify_notify_accepts_resp0000() {
        let mut form = BTreeMap::new();
        form.insert("merReqNo".into(), "P5".into());
        form.insert("serverRspNo".into(), "SRV-9".into());
        form.insert("respCode".into(), "0000".into());
        form.insert("sign".into(), Aliscan::sign("SRV-9", "123456"));

        let req = CallbackReq {
            form,
            raw_body: String::new(),
        };
        let ok = Aliscan.verify_notify(&cred(), &req).unwrap();
        assert!(ok.success);
        assert_eq!(ok.platform_order_id, "P5");
        assert_eq!(ok.upstream_txn, "SRV-9");
    }

    #[test]
    fn verify_notify_reads_failed_trade_without_error() {
        // A correctly-signed `respCode != 0000` is a valid message reporting a
        // failed trade: verified (no error) but `success == false`.
        let mut form = BTreeMap::new();
        form.insert("merReqNo".into(), "P5".into());
        form.insert("serverRspNo".into(), "SRV-9".into());
        form.insert("respCode".into(), "9999".into());
        form.insert("sign".into(), Aliscan::sign("SRV-9", "123456"));

        let req = CallbackReq {
            form,
            raw_body: String::new(),
        };
        let ok = Aliscan.verify_notify(&cred(), &req).unwrap();
        assert!(!ok.success);
        assert_eq!(ok.ack, "trade fail");
    }

    #[test]
    fn verify_notify_rejects_bad_signature() {
        let mut form = BTreeMap::new();
        form.insert("merReqNo".into(), "P5".into());
        form.insert("serverRspNo".into(), "SRV-9".into());
        form.insert("respCode".into(), "0000".into());
        form.insert("sign".into(), md5_hex_lower(b"wrong"));
        let req = CallbackReq {
            form,
            raw_body: String::new(),
        };
        assert!(matches!(
            Aliscan.verify_notify(&cred(), &req),
            Err(ChannelError::Signature(_))
        ));
    }
}
