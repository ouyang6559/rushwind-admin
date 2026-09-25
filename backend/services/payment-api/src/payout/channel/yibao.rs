//! The `Yibao` (易宝) live payout adapter — `Payment/YibaoController`. A JSON
//! POST whose sign is `md5(<json body> . '|' . signkey)` carried in an
//! `Api-Sign` header, with the endpoint the caller's `exec_gateway` /
//! `query_gateway` joined by `/withdraw/create` and `/withdraw/query`.
//!
//! The body is assembled with a tiny ORDERED writer (not `serde_json`'s
//! alphabetised map) because the sign is taken over the EXACT bytes PHP's
//! `json_encode` emits — insertion order, no spaces. Amounts ride 元 as JSON
//! numbers (`amount = realAmount + 3`, the fixed +3 元 channel surcharge).
//!
//! Byte-equivalence of the sign to the live upstream still needs a recorded
//! PHP golden (PHP's `json_encode` default-escapes non-ASCII to `\uXXXX` and
//! renders floats with shortest-roundtrip; this writer emits UTF-8 and a
//! trimmed decimal) — registered in the语义差异清单. The request wiring,
//! sign construction and response→[`ExecResp`] normalisation are exercised
//! here and by a loopback mock in the integration tests.

use async_trait::async_trait;
use serde_json::Value;

use crate::channel::sign::md5_hex_lower;
use crate::channel::ChannelError;
use crate::data::payout_orders;
use crate::money::units_to_fen;
use crate::payout::exec::{ExecResp, PayoutChannelCfg, PayoutExec};

use super::{http_client, join_gateway, out_trade_no};

pub struct Yibao {
    client: reqwest::Client,
}

impl Yibao {
    pub fn new() -> Self {
        Self {
            client: http_client(),
        }
    }

    /// The exec body JSON: `{merchantId, timestamp, body{…}}` in the exact
    /// `json_encode` field order of `YibaoController::PaymentExec`.
    pub fn exec_body(
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
        ts_secs: i64,
    ) -> String {
        let ext = order
            .additional
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);
        let ex = |k: &str| -> String {
            ext.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        // realAmount = arrival 元; amount = that + the fixed 3元 surcharge.
        let real_cents = units_to_fen(order.money);
        let real_amount = yuan_json(real_cents);
        let amount = yuan_json(real_cents + 300);

        let mut b = ObjBuilder::new();
        b.str("merchantId", chan.mch_id.as_deref().unwrap_or_default());
        b.str("timestamp", &format!("{ts_secs}000"));
        let mut inner = ObjBuilder::new();
        inner.str("advPasswordMd5", &chan.app_secret);
        inner.str("orderId", out_trade_no(order));
        inner.num("flag", "0");
        inner.str("bankProvinceName", &ex("bankProvinceName"));
        inner.str("bankProvinceCode", &ex("bankProvinceCode"));
        inner.str("bankCityName", &ex("bankCityName"));
        inner.str("bankCityCode", &ex("bankCityCode"));
        inner.str("bankAreaName", &ex("bankAreaName"));
        inner.str("bankAreaCode", &ex("bankAreaCode"));
        inner.str("bankId", &ex("bankId"));
        inner.str("bankName", order.bankname.as_deref().unwrap_or_default());
        inner.str(
            "bankBranchName",
            order.subbranch.as_deref().unwrap_or_default(),
        );
        inner.str("bankCode", order.cardnumber.as_deref().unwrap_or_default());
        inner.str("bankUser", order.accountname.as_deref().unwrap_or_default());
        inner.str("bankUserCert", &ex("bankUserCert"));
        inner.str("bankUserPhone", &ex("bankUserPhone"));
        inner.num("amount", &amount);
        inner.num("realAmount", &real_amount);
        b.obj("body", inner.finish());
        b.finish()
    }

    /// The query body: `{merchantId, timestamp, body{orderId}}`.
    pub fn query_body(
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
        ts_secs: i64,
    ) -> String {
        let mut b = ObjBuilder::new();
        b.str("merchantId", chan.mch_id.as_deref().unwrap_or_default());
        b.str("timestamp", &format!("{ts_secs}000"));
        let mut inner = ObjBuilder::new();
        inner.str("orderId", out_trade_no(order));
        b.obj("body", inner.finish());
        b.finish()
    }

    /// `md5(<body> . '|' . signkey)`, lowercase — `YibaoController::_createSign`.
    pub fn sign(body_json: &str, secret: &str) -> String {
        md5_hex_lower(format!("{body_json}|{secret}").as_bytes())
    }

    /// Exec: `status === 0` → 处理中; else → 失败; empty → 服务不可用 (§9.2).
    pub fn parse_exec(body: &Value) -> ExecResp {
        if body.is_null() || body.get("status").is_none() {
            return ExecResp::failed("错误：服务不可用");
        }
        if field(body, "status") == "0" {
            ExecResp::processing("提交成功")
        } else {
            ExecResp::failed(format!(
                "错误：{}：{}",
                field(body, "status"),
                field(body, "message")
            ))
        }
    }

    /// Query: `status === 0 && body.status == 1` → the legacy reports a paid
    /// payout as 处理中 (`1`, msg 付款成功), never `2` — a faithful quirk that
    /// keeps paid Yibao orders cycling until a manual re-confirm (§9.2).
    pub fn parse_query(body: &Value) -> ExecResp {
        if body.is_null() || body.get("status").is_none() {
            return ExecResp::failed("错误：服务不可用");
        }
        if field(body, "status") != "0" {
            return ExecResp::failed(format!(
                "错误：{}：{}",
                field(body, "status"),
                field(body, "message")
            ));
        }
        let inner = body.get("body").cloned().unwrap_or(Value::Null);
        if field(&inner, "status") == "1" {
            ExecResp::processing("付款成功")
        } else {
            ExecResp::failed(format!(
                "{}：{}",
                field(&inner, "status"),
                field(&inner, "message")
            ))
        }
    }

    async fn post_json(
        &self,
        url: &str,
        body_json: &str,
        sign: &str,
    ) -> Result<Value, ChannelError> {
        let resp = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Api-Sign", sign)
            .body(body_json.to_string())
            .send()
            .await
            .map_err(|e| ChannelError::Upstream(format!("yibao transport: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ChannelError::Upstream(format!("yibao http {status}")));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ChannelError::Upstream(format!("yibao read: {e}")))?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

impl Default for Yibao {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads a JSON field as a string, coercing numbers (the gateway mixes int and
/// string codes across environments).
fn field(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Renders a cent count as a PHP-style JSON number: at least one decimal,
/// trailing fractional zeros trimmed (`100.0`, `100.5`, `100.25`).
fn yuan_json(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let ac = cents.unsigned_abs();
    let yuan = ac / 100;
    let frac = (ac % 100) as i64;
    if frac == 0 {
        format!("{sign}{yuan}.0")
    } else if frac % 10 == 0 {
        format!("{sign}{yuan}.{}", frac / 10)
    } else {
        format!("{sign}{yuan}.{frac:02}")
    }
}

/// A minimal ordered JSON-object writer (insertion order, compact, no spaces)
/// so the signed bytes match what PHP's `json_encode` assembles.
struct ObjBuilder {
    out: String,
    first: bool,
}

impl ObjBuilder {
    fn new() -> Self {
        Self {
            out: String::from("{"),
            first: true,
        }
    }
    fn key(&mut self, k: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        self.out.push('"');
        self.out.push_str(k);
        self.out.push_str("\":");
    }
    fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        self.out.push('"');
        for c in v.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                c if (c as u32) < 0x20 => self.out.push_str(&format!("\\u{:04x}", c as u32)),
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }
    fn num(&mut self, k: &str, raw: &str) {
        self.key(k);
        self.out.push_str(raw);
    }
    fn obj(&mut self, k: &str, inner: String) {
        self.key(k);
        self.out.push_str(&inner);
    }
    fn finish(self) -> String {
        let mut s = self.out;
        s.push('}');
        s
    }
}

#[async_trait]
impl PayoutExec for Yibao {
    fn code(&self) -> &str {
        "Yibao"
    }

    async fn exec(
        &self,
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        let url = join_gateway(&chan.exec_gateway, "/withdraw/create");
        let body = Self::exec_body(order, chan, crate::data::now_ts());
        let sign = Self::sign(&body, &chan.sign_key);
        let resp = self.post_json(&url, &body, &sign).await?;
        Ok(Self::parse_exec(&resp))
    }

    async fn query(
        &self,
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        let url = join_gateway(&chan.query_gateway, "/withdraw/query");
        let body = Self::query_body(order, chan, crate::data::now_ts());
        let sign = Self::sign(&body, &chan.sign_key);
        let resp = self.post_json(&url, &body, &sign).await?;
        Ok(Self::parse_query(&resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn order() -> payout_orders::Model {
        payout_orders::Model {
            order_no: "P0922001230000101".into(),
            user_id: 7,
            money: 100 * 10_000, // 100元
            cardnumber: Some("622202000000001".into()),
            accountname: Some("张三".into()),
            bankname: Some("工商银行".into()),
            subbranch: Some("测试支行".into()),
            additional: Some(json!({"bankProvinceCode": "110000"}).to_string()),
            ..Default::default()
        }
    }

    fn chan() -> PayoutChannelCfg {
        PayoutChannelCfg {
            id: 21,
            code: "Yibao".into(),
            name: "易宝代付".into(),
            mch_id: Some("YB777".into()),
            sign_key: "SECRET".into(),
            app_secret: "PWD".into(),
            exec_gateway: "http://up/exec".into(),
            query_gateway: "http://up/query".into(),
            ..Default::default()
        }
    }

    #[test]
    fn exec_body_is_ordered_with_the_surcharge() {
        let body = Yibao::exec_body(&order(), &chan(), 1_700_000_000);
        // insertion order, compact, amount = realAmount + 3 (103.0), no ASCII
        // sort applied.
        assert!(body
            .starts_with("{\"merchantId\":\"YB777\",\"timestamp\":\"1700000000000\",\"body\":{"));
        assert!(body.contains("\"flag\":0"));
        assert!(body.contains("\"amount\":103.0"));
        assert!(body.contains("\"realAmount\":100.0"));
        assert!(body.contains("\"bankProvinceCode\":\"110000\""));
        // merchantId precedes timestamp precedes body (which is last).
        assert!(body.find("merchantId").unwrap() < body.find("timestamp").unwrap());
        assert!(body.find("\"body\":{").unwrap() < body.find("advPasswordMd5").unwrap());
    }

    #[test]
    fn query_body_is_minimal() {
        let body = Yibao::query_body(&order(), &chan(), 42);
        assert_eq!(
            body,
            "{\"merchantId\":\"YB777\",\"timestamp\":\"42000\",\"body\":{\"orderId\":\"P0922001230000101\"}}"
        );
    }

    #[test]
    fn sign_is_body_pipe_key_md5() {
        let s = Yibao::sign("{\"a\":1}", "K");
        assert_eq!(s, md5_hex_lower(b"{\"a\":1}|K"));
    }

    #[test]
    fn yuan_number_renders_php_style() {
        assert_eq!(yuan_json(10000), "100.0");
        assert_eq!(yuan_json(10050), "100.5");
        assert_eq!(yuan_json(10025), "100.25");
        assert_eq!(yuan_json(10020), "100.2");
    }

    #[test]
    fn exec_maps_zero_to_processing() {
        assert_eq!(
            Yibao::parse_exec(&json!({"status":0,"message":"ok"})).status,
            1
        );
        assert_eq!(
            Yibao::parse_exec(&json!({"status":1,"message":"bad"})).status,
            3
        );
        assert_eq!(Yibao::parse_exec(&json!(null)).status, 3);
        assert_eq!(Yibao::parse_exec(&json!({})).msg, "错误：服务不可用");
    }

    #[test]
    fn query_reports_paid_as_processing_faithfully() {
        // §9.2 quirk: a body.status==1 success answer maps to `1` (处理中),
        // never terminal `2`.
        let ok = json!({"status":0,"body":{"status":1,"message":"paid"}});
        let r = Yibao::parse_query(&ok);
        assert_eq!(r.status, 1);
        assert_eq!(r.msg, "付款成功");
        assert_eq!(
            Yibao::parse_query(&json!({"status":0,"body":{"status":2}})).status,
            3
        );
        assert_eq!(
            Yibao::parse_query(&json!({"status":5,"message":"e"})).status,
            3
        );
    }
}
