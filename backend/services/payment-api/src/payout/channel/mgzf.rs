//! The `MGZF` (蘑菇支付) live payout adapter — `Payment/MGZFController`.
//! A form-urlencoded POST whose sign is the sorted `k=v&…` link with the
//! secret appended DIRECTLY and a lowercase MD5 — byte-for-byte the
//! [`crate::channel::sign::easy_pay_sign`] family already proven against the
//! PHP layout on the收款 side, so the sign here is wire-exact without a fresh
//! golden.
//!
//! Amount rides 分 (`money × 100`), the arrival amount [`units_to_fen`]. The
//! exec answer NEVER reports final success (only `1` 处理中 on
//! `code==0000 && status==1`); the query is what settles an order (§9.2).

use async_trait::async_trait;
use serde_json::Value;

use crate::channel::sign::easy_pay_sign;
use crate::channel::ChannelError;
use crate::data::payout_orders;
use crate::money::units_to_fen;
use crate::payout::exec::{ExecResp, PayoutChannelCfg, PayoutExec};

use super::{bank_of, http_client, join_gateway, out_trade_no};

/// The channel-code key that gates the sign's empty-skip and the response.
const OK_CODE: &str = "0000";

pub struct Mgzf {
    client: reqwest::Client,
}

impl Mgzf {
    pub fn new() -> Self {
        Self {
            client: http_client(),
        }
    }

    /// The submit form params (pre-sign), arrival amount in 分 (§9.2).
    pub fn exec_params(
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Vec<(String, String)> {
        let (card, user, bank_name) = bank_of(order);
        vec![
            (
                "merchant_no".into(),
                chan.mch_id.clone().unwrap_or_default(),
            ),
            ("method".into(), "settle".into()),
            ("bank_card".into(), card.to_string()),
            ("bank_name".into(), bank_name.to_string()),
            ("bank_user".into(), user.to_string()),
            (
                "bank_province".into(),
                order.province.clone().unwrap_or_default(),
            ),
            ("bank_city".into(), order.city.clone().unwrap_or_default()),
            ("bank_card_type".into(), "1".into()),
            ("amount".into(), units_to_fen(order.money).to_string()),
            ("out_trade_no".into(), out_trade_no(order).to_string()),
        ]
    }

    /// The query form params (pre-sign).
    pub fn query_params(
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Vec<(String, String)> {
        vec![
            (
                "merchant_no".into(),
                chan.mch_id.clone().unwrap_or_default(),
            ),
            ("method".into(), "settlequery".into()),
            ("out_trade_no".into(), out_trade_no(order).to_string()),
        ]
    }

    /// Signs a param set: the sorted `k=v&…` (skip `sign`/empty) + secret,
    /// lowercase MD5 — the exact `md5Sign` of `MGZFController:94-106`.
    pub fn sign(params: &[(String, String)], secret: &str) -> String {
        let map = params
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        easy_pay_sign(&map, secret)
    }

    /// `code == 0000 && status == 1` → 处理中; everything else → 失败 (§9.2).
    pub fn parse_exec(body: &Value) -> ExecResp {
        let msg = field(body, "msg");
        if field(body, "code") == OK_CODE && field(body, "status") == "1" {
            ExecResp::processing(msg)
        } else {
            ExecResp::failed(msg)
        }
    }

    /// §9.2 意图映射: `code==0000` 时 `status` 1→成功 / 2→失败 / 0→处理中，
    /// 其余→失败；`code!=0000` → 失败。（MGZFController 的 `switch($result &&
    /// code==0000)` 把被 switch 的表达式误写成了布尔量，任何 code 正常都会命中
    /// 首个 `case '1'` 恒返回成功——此为登记在语义差异清单的 legacy 缺陷，此处按
    /// 表 §9.2 的正确语义实现。）
    pub fn parse_query(body: &Value) -> ExecResp {
        let msg = field(body, "msg");
        if field(body, "code") != OK_CODE {
            return ExecResp::failed(msg);
        }
        match field(body, "status").as_str() {
            "1" => ExecResp::success(msg),
            "0" => ExecResp::processing(msg),
            _ => ExecResp::failed(msg),
        }
    }

    async fn post_form(
        &self,
        url: &str,
        params: &[(String, String)],
        secret: &str,
    ) -> Result<Value, ChannelError> {
        let sign = Self::sign(params, secret);
        let mut form: Vec<(String, String)> = params.to_vec();
        form.push(("sign".into(), sign));
        let resp = self
            .client
            .post(url)
            .form(&form)
            .send()
            .await
            .map_err(|e| ChannelError::Upstream(format!("mgzf transport: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ChannelError::Upstream(format!("mgzf http {status}")));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ChannelError::Upstream(format!("mgzf read: {e}")))?;
        serde_json::from_str(&text).map_err(|e| ChannelError::Upstream(format!("mgzf decode: {e}")))
    }
}

impl Default for Mgzf {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads a JSON field as a trimmed string, coercing numbers (the gateways mix
/// `"0000"` strings and bare ints across the sample responses).
fn field(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

#[async_trait]
impl PayoutExec for Mgzf {
    fn code(&self) -> &str {
        "MGZF"
    }

    async fn exec(
        &self,
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        let url = join_gateway(&chan.exec_gateway, "");
        let body = self
            .post_form(&url, &Self::exec_params(order, chan), &chan.sign_key)
            .await?;
        Ok(Self::parse_exec(&body))
    }

    async fn query(
        &self,
        order: &payout_orders::Model,
        chan: &PayoutChannelCfg,
    ) -> Result<ExecResp, ChannelError> {
        let url = join_gateway(&chan.query_gateway, "");
        let body = self
            .post_form(&url, &Self::query_params(order, chan), &chan.sign_key)
            .await?;
        Ok(Self::parse_query(&body))
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
            money: 100 * 10_000, // 100元 → 10000 分
            cardnumber: Some("622202000000001".into()),
            accountname: Some("张三".into()),
            bankname: Some("工商银行".into()),
            province: Some("北京".into()),
            city: Some("北京".into()),
            ..payout_orders::Model::default()
        }
    }

    fn chan() -> PayoutChannelCfg {
        PayoutChannelCfg {
            id: 3,
            code: "MGZF".into(),
            name: "蘑菇代付".into(),
            mch_id: Some("MG888".into()),
            sign_key: "SECRET".into(),
            exec_gateway: "http://up/settle".into(),
            query_gateway: "http://up/query".into(),
            ..Default::default()
        }
    }

    #[test]
    fn sign_is_the_sorted_link_plus_secret_md5() {
        // Reproduce MGZF md5Sign: ksort, skip sign/empty, join k=v&, append the
        // secret with no separator, lowercase md5.
        let params = Mgzf::exec_params(&order(), &chan());
        let sign = Mgzf::sign(&params, "SECRET");
        // The link is exactly what easy_pay_sign produces → equals its md5.
        let map = params
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(sign, easy_pay_sign(&map, "SECRET"));
        assert_eq!(sign.len(), 32);
        assert!(sign.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn amount_rides_cents() {
        let params = Mgzf::exec_params(&order(), &chan());
        let amount = params.iter().find(|(k, _)| k == "amount").unwrap();
        assert_eq!(amount.1, "10000"); // 100元 → 10000 分
    }

    #[test]
    fn exec_maps_only_status1_to_processing() {
        assert_eq!(
            Mgzf::parse_exec(&json!({"code":"0000","status":"1","msg":"ok"})).status,
            1
        );
        assert_eq!(
            Mgzf::parse_exec(&json!({"code":"0000","status":"0","msg":"no"})).status,
            3
        );
        assert_eq!(
            Mgzf::parse_exec(&json!({"code":"5000","status":"1","msg":"x"})).status,
            3
        );
        // a numeric `status` is coerced to a string and still matches (the
        // gateway mixes "1" and 1 across environments); the ok `code` is the
        // "0000" string — a bare numeric 0 does NOT equal it and stays failed.
        assert_eq!(
            Mgzf::parse_exec(&json!({"code":"0000","status":1,"msg":"x"})).status,
            1
        );
        assert_eq!(
            Mgzf::parse_exec(&json!({"code":0,"status":1,"msg":"x"})).status,
            3
        );
    }

    #[test]
    fn query_maps_status_by_the_spec_table() {
        assert_eq!(
            Mgzf::parse_query(&json!({"code":"0000","status":"1","msg":"ok"})).status,
            2
        );
        assert_eq!(
            Mgzf::parse_query(&json!({"code":"0000","status":"2","msg":"no"})).status,
            3
        );
        assert_eq!(
            Mgzf::parse_query(&json!({"code":"0000","status":"0","msg":"wait"})).status,
            1
        );
        assert_eq!(
            Mgzf::parse_query(&json!({"code":"9999","status":"1","msg":"x"})).status,
            3
        );
    }
}
