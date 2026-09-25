//! The downstream payout-API wire surface (`spec/04` §7): the HTTP edges of
//! `Payment/DfpayController::add` (§7.1–§7.4, a signed downstream merchant
//! filing a payout application) and `::query` (§7.5, the merchant polling its
//! own application). This is the sign / domain / IP / holiday / window edge the
//! [`crate::payout::review`] persistence core explicitly leaves to the handler.
//!
//! The filing runs the legacy guard ORDER (§7.1) and then hands the落库 to the
//! already-tested kernels: [`PayoutService::apply_payout_api`] files the
//! `source = 3, check_status = 0` row (no debit), and when the merchant carries
//! `df_auto_check` [`PayoutService::df_pass`] re-runs the full chain and debits
//! in one go (§7.4). `query` maps the row's `check_status` / `status` onto the
//! downstream `refCode` ladder (§7.5) and echoes a signed reply.
//!
//! Deviations from the legacy, registered as decision memories:
//! - the MD5 signature is verified at the handler boundary (identity/auth)
//!   rather than the legacy's last pre-file position — both reject the request,
//!   only the message differs when several guards breach at once;
//! - `Websiteconfig.df_api` (the platform-wide kill switch) is config-sourced
//!   ([`crate::config::Config::df_api`], default off per the DDL); the
//!   `pay_channel_extend_fields` required-field validation is NOT modelled,
//!   a documented seam — `extends` is base64-decoded and stored verbatim on
//!   the order's `additional` snapshot;
//! - `df_pass` failure after an auto-check filing rolls back the DEBIT in its
//!   own tx while the `apply_payout_api` row already committed (the legacy ran
//!   both under one outer tx) — the handler compensates by deleting the still
//!   `check_status = 0` row, reproducing the legacy's nothing-persists outcome;
//! - the query reply signature's byte-compat (the `amount` string form and the
//!   `success_time` representation) is implemented per the code but awaits a
//!   PHP golden to prove out — see [`units_to_yuan`].

use base64::Engine as _;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::{members, payout_orders};
use crate::gateway::sign::{sign_form_all, sign_pairs, sign_query_request, verify_sign};
use crate::merchant::{self, MembersRepo};
use crate::money::{parse_yuan_to_units, units_to_yuan};
use crate::payout::config::{check_holiday, check_time_window};
use crate::payout::{
    ApplyPayoutApi, BankSnapshot, HolidayRepo, PayoutConfigRepo, PayoutService, RequestTime,
};
use crate::state::{GatewayError, GatewayResult};

/// The `pay_*` style newline-separated base-domain whitelist check
/// (`checkDfDomain:933-951`): the request referer's registrable domain must be
/// one of the merchant's reported domains. A referer that resolves to no
/// domain never matches, so an unreported (empty) whitelist is short-circuited
/// by the caller before this is ever reached.
pub fn check_df_domain(referer: &str, recorded: &str) -> bool {
    if referer.is_empty() {
        return false;
    }
    let domain = get_base_domain(referer);
    recorded
        .split("\r\n")
        .filter(|v| !v.is_empty())
        .map(get_base_domain)
        .any(|base| base == domain)
}

/// The client-IP whitelist check (`checkDfIp:959-974`): the resolved client IP
/// must exactly equal one of the newline-separated reported IPs. The caller
/// supplies the client IP (see [`client_ip_of`]); an unreported (empty)
/// whitelist is short-circuited by the caller.
pub fn check_df_ip(client_ip: &str, recorded: &str) -> bool {
    !client_ip.is_empty()
        && recorded
            .split("\r\n")
            .map(|v| v.trim())
            .any(|v| !v.is_empty() && v == client_ip)
}

/// `getBaseDomain:884-926` — the registrable domain of a URL / host. The last
/// two labels unless the second-to-last is itself a known TLD / ccTLD (then
/// three), reproducing the legacy's `co.uk`-style special cases.
pub fn get_base_domain(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let lowered = url.to_lowercase();
    let owned;
    let with_scheme = if lowered.starts_with("http") {
        lowered.as_str()
    } else {
        // PHP prefixes `http://` when the string does not start with `http`.
        owned = format!("http://{lowered}");
        owned.as_str()
    };
    host_of(with_scheme)
        .and_then(base_of_host)
        .unwrap_or_default()
}

/// Extracts the host component (no userinfo / port / path) like `parse_url`.
fn host_of(url: &str) -> Option<String> {
    let after = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let authority = after
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = authority.split(':').next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Reduces a bare host to its registrable domain per the legacy's label pops.
fn base_of_host(host: String) -> Option<String> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return Some(host);
    }
    let n = parts.len();
    let (last, last_1, last_2) = (parts[n - 1], parts[n - 2], parts[n - 3]);
    let base = if is_state_domain(last_1) {
        format!("{last_2}.{last_1}.{last}")
    } else {
        format!("{last_1}.{last}")
    };
    Some(base)
}

/// The legacy `$state_domain` TLD / ccTLD table (`function.php:889-890`); the
/// second-to-last label matching it widens the base domain to three labels.
fn is_state_domain(label: &str) -> bool {
    STATE_DOMAIN.contains(&label)
}

/// §7.5 — the downstream `refCode` / `refMsg` the query reply reports, off the
/// unified row's `check_status` (review sub-status) and `status` (execution).
/// `check_status = 0` → 待审核, `2` → 审核驳回, and an approved (`1`) row folds
/// its execution `status` (0→待处理, 1→处理中, 2→成功, 3→失败, 4→待确认) onto
/// the legacy's `wttklist.status` ladder; anything unrecognised is 未知状态.
pub fn ref_code_for(check_status: Option<i16>, status: i16) -> (&'static str, &'static str) {
    match check_status {
        Some(2) => ("5", "审核驳回"),
        Some(1) => match status {
            0 => ("4", "待处理"),
            1 => ("3", "处理中"),
            2 => ("1", "成功"),
            3 => ("2", "失败"),
            4 => ("3", "待确认"),
            _ => ("8", "未知状态"),
        },
        // `check_status = 0` (or a never-reviewed application) → 待审核.
        _ => ("6", "待审核"),
    }
}

/// Best-effort client IP off the proxy headers (`get_client_ip`): the first
/// `X-Forwarded-For` hop, then `X-Real-IP`. Empty when none is present — the
/// IP whitelist check then simply fails to match (an empty IP never equals a
/// reported IP), so an unreported whitelist (guarded by the caller) is the
/// only way a blank IP passes.
fn header_ip<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

pub fn client_ip_of(headers: &axum::http::HeaderMap) -> String {
    if let Some(fwd) = header_ip(headers, "x-forwarded-for") {
        if let Some(first) = fwd.split(',').next().map(str::trim) {
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    header_ip(headers, "x-real-ip").unwrap_or("").to_string()
}

/// The `Dfpay::add` request, every field already trimmed off the form. Money
/// arrives as a 元 decimal string (`post.money`); `extends_raw` is the base64
/// JSON blob the merchant sends.
#[derive(Debug, Clone)]
pub struct DfApplyInput {
    pub money_yuan: String,
    pub out_trade_no: String,
    pub bankname: String,
    pub subbranch: String,
    pub accountname: String,
    pub cardnumber: String,
    pub province: String,
    pub city: String,
    pub extends_raw: String,
    pub client_ip: String,
    pub referer: String,
}

/// The outcome of a §7.2 filing (+ optional §7.4 auto-review), ready to shape
/// into the legacy JSON reply.
#[derive(Debug, Clone)]
pub enum ApplyResult {
    /// Filed (and auto-reviewed when configured): `transaction_id` is the
    /// platform order no.
    Success { transaction_id: String },
    /// A §7.1 / §7.4 guard refused the request — the exact legacy message.
    Rejected { msg: String },
}

/// Runs the §7.1 business-guard chain in the legacy order and, on a clean
/// pass, files the application (§7.2) and — for an `df_auto_check` merchant —
/// immediately reviews it (§7.4). Identity / `df_api` / signature are the
/// handler's job and already cleared before this is called; `member` supplies
/// the `df_domain` / `df_ip` / `df_auto_check` gates.
pub async fn apply_payout(
    db: &DatabaseConnection,
    payout: &PayoutService,
    member: &members::Model,
    input: &DfApplyInput,
    now_ts: i64,
) -> GatewayResult<ApplyResult> {
    // 报备域名 / 报备 IP (§7.1 :55-63) — only when the merchant configured one.
    let df_domain = member.df_domain.clone().unwrap_or_default();
    if !df_domain.is_empty() && !check_df_domain(&input.referer, &df_domain) {
        return Ok(rejected("请求来源域名与报备域名不一致！"));
    }
    let df_ip = member.df_ip.clone().unwrap_or_default();
    if !df_ip.is_empty() && !check_df_ip(&input.client_ip, &df_ip) {
        return Ok(rejected("IP地址与报备IP不一致！"));
    }

    // 节假日 (:64-73).
    let now = RequestTime::from_ts(now_ts);
    let holidays = HolidayRepo::new(db).load().await?;
    if let Some(msg) = check_holiday(&now, &holidays) {
        return Ok(rejected(&msg));
    }

    // 结算方式 / 提款设置 (:74-100) — the effective config (system row
    // required, personal override, window forced from system) + the trading
    // window. `None` = withdrawal globally closed.
    let cfg = PayoutConfigRepo::new(db).resolve(member.id).await?;
    let Some(cfg) = cfg else {
        return Ok(rejected("提款已关闭！"));
    };
    if check_time_window(cfg.allow_start, cfg.allow_end, now.hour).is_some() {
        return Ok(rejected("不在提现时间，请换个时间再来!"));
    }

    // 金额 (:102-113) — money > 0, then the single-txn min / max bounds.
    let amount_units = match parse_yuan_to_units(&input.money_yuan) {
        Some(u) if u > 0 => u,
        _ => return Ok(rejected("金额错误！")),
    };
    if cfg.tkzx_money > 0 && amount_units < cfg.tkzx_money {
        return Ok(rejected(&format!(
            "单笔最低提款额度：{}",
            units_to_yuan(cfg.tkzx_money)
        )));
    }
    if cfg.tkzd_money > 0 && amount_units > cfg.tkzd_money {
        return Ok(rejected(&format!(
            "单笔最大提款额度：{}",
            units_to_yuan(cfg.tkzd_money)
        )));
    }

    // 卡四要素 + 省市区 + 订单号 (:114-141).
    for (field, msg) in [
        (&input.bankname, "银行名称不能为空！"),
        (&input.subbranch, "支行名称不能为空"),
        (&input.accountname, "开户名不能为空！"),
        (&input.cardnumber, "银行卡号不能为空！"),
        (&input.province, "省份不能为空！"),
        (&input.city, "城市不能为空！"),
        (&input.out_trade_no, "订单号不能为空！"),
    ] {
        if field.is_empty() {
            return Ok(rejected(msg));
        }
    }

    // 商户内唯一订单号查重 (:138-146). The partial unique index
    // `(user_id, out_trade_no)` stays the net for a raced twin.
    if find_application(db, member.id, &input.out_trade_no)
        .await?
        .is_some()
    {
        return Ok(rejected("存在重复订单号！"));
    }

    // File the pending application (§7.2), then auto-review when configured.
    let order = payout
        .apply_payout_api(&ApplyPayoutApi {
            user_id: member.id,
            amount: amount_units,
            out_trade_no: &input.out_trade_no,
            bank: BankSnapshot {
                bankname: Some(input.bankname.clone()),
                subbranch: Some(input.subbranch.clone()),
                accountname: Some(input.accountname.clone()),
                cardnumber: Some(input.cardnumber.clone()),
                province: Some(input.province.clone()),
                city: Some(input.city.clone()),
            },
            extends: decode_extends(&input.extends_raw),
        })
        .await?;

    if member.df_auto_check != 0 {
        if let Err(e) = payout.df_pass(&order.order_no, now_ts).await {
            // Legacy rolled the file + dfPass back together; our apply already
            // committed, so drop the still-pending row we just filed.
            if matches!(e, GatewayError::BadRequest(_)) {
                let _ = payout_orders::Entity::delete_many()
                    .filter(payout_orders::Column::OrderNo.eq(&order.order_no))
                    .filter(payout_orders::Column::CheckStatus.eq(0))
                    .exec(db)
                    .await;
                return Ok(rejected(e.message()));
            }
            return Err(e);
        }
    }
    Ok(ApplyResult::Success {
        transaction_id: order.order_no,
    })
}

/// Loads the merchant's payout-API application by its downstream order no.
/// The query reply mapping is pure ([`ref_code_for`]); the handler shapes it.
pub async fn find_application(
    db: &DatabaseConnection,
    user_id: i64,
    out_trade_no: &str,
) -> GatewayResult<Option<payout_orders::Model>> {
    Ok(payout_orders::Entity::find()
        .filter(payout_orders::Column::UserId.eq(user_id))
        .filter(payout_orders::Column::OutTradeNo.eq(out_trade_no))
        .one(db)
        .await?)
}

/// Decodes the base64 `extends` blob to the JSON string the legacy stores on
/// the order (the `additional` snapshot). An undecodable / empty value stores
/// `None`; the channel-required-field validation stays a seam.
fn decode_extends(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn rejected(msg: &str) -> ApplyResult {
    ApplyResult::Rejected {
        msg: msg.to_string(),
    }
}

// --- axum wire handlers ----------------------------------------------------

use crate::gateway::handlers::{internal_error, legacy_error, legacy_json};
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::state::AppState;

/// `Payment/Dfpay/add` — the signed downstream payout-application filing.
pub async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    if form.is_empty() {
        return legacy_error("no data!");
    }
    // The platform-wide代付 kill switch (`websiteconfig.df_api`, the first
    // guard the legacy Dfpay::add ran); config-sourced — see
    // [`crate::config::Config::df_api`].
    if !state.cfg.df_api {
        return legacy_error("代付API未开启！");
    }
    let get = |k: &str| {
        form.get(k)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    let sign = get("pay_md5sign");
    if sign.is_empty() {
        return legacy_error("缺少签名参数");
    }
    let mchid = get("mchid");
    if mchid.is_empty() {
        return legacy_error("商户ID不能为空！");
    }
    let user_id = match mchid.parse::<i64>().ok().and_then(merchant::user_id_of_mch) {
        Some(u) => u,
        None => return legacy_error("商户不存在！"),
    };
    let member = match MembersRepo::new(&state.db).by_id(user_id).await {
        Ok(Some(m)) => m,
        Ok(None) => return legacy_error("商户不存在！"),
        Err(e) => return internal_error(format!("db: {e}")),
    };
    if member.df_api == 0 {
        return legacy_error("商户未开启此功能！");
    }
    let apikey = member.apikey.clone().unwrap_or_default();
    if !verify_sign(&sign, &sign_form_all(&apikey, &form)) {
        return legacy_error("签名验证失败");
    }

    let input = DfApplyInput {
        money_yuan: get("money"),
        out_trade_no: get("out_trade_no"),
        bankname: get("bankname"),
        subbranch: get("subbranch"),
        accountname: get("accountname"),
        cardnumber: get("cardnumber"),
        province: get("province"),
        city: get("city"),
        extends_raw: form.get("extends").cloned().unwrap_or_default(),
        client_ip: client_ip_of(&headers),
        referer: headers
            .get(axum::http::header::REFERER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    };
    let now_ts = chrono::Local::now().timestamp();
    match apply_payout(&state.db, &state.payout, &member, &input, now_ts).await {
        Ok(ApplyResult::Success { transaction_id }) => legacy_json(
            StatusCode::OK,
            &serde_json::json!({
                "status": "success", "msg": "代付申请成功", "transaction_id": transaction_id,
            }),
        ),
        Ok(ApplyResult::Rejected { msg }) => legacy_error(&msg),
        Err(e) => internal_error(e.message()),
    }
}

/// `Payment/Dfpay/query` — the downstream merchant's own-application poll,
/// GET / POST both accepted (the legacy read `$_REQUEST`).
pub async fn query_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    query_reply(&state, &headers, &query).await
}

pub async fn query_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    query_reply(&state, &headers, &form).await
}

async fn query_reply(
    state: &AppState,
    headers: &HeaderMap,
    params: &BTreeMap<String, String>,
) -> Response {
    let get = |k: &str| {
        params
            .get(k)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let sign = get("pay_md5sign");
    if sign.is_empty() {
        return legacy_error("缺少签名参数");
    }
    let out_trade_no = get("out_trade_no");
    if out_trade_no.is_empty() {
        return legacy_error("缺少订单号");
    }
    let mchid = get("mchid");
    if mchid.is_empty() {
        return legacy_error("缺少商户号");
    }
    let user_id = match mchid.parse::<i64>().ok().and_then(merchant::user_id_of_mch) {
        Some(u) => u,
        None => return legacy_error("商户不存在！"),
    };
    let member = match MembersRepo::new(&state.db).by_id(user_id).await {
        Ok(Some(m)) => m,
        Ok(None) => return legacy_error("商户不存在！"),
        Err(e) => return internal_error(format!("db: {e}")),
    };
    if member.df_api == 0 {
        return legacy_error("商户未开启此功能！");
    }
    let df_domain = member.df_domain.clone().unwrap_or_default();
    if !df_domain.is_empty() {
        let referer = headers
            .get(axum::http::header::REFERER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !check_df_domain(referer, &df_domain) {
            return legacy_error("请求来源域名与报备域名不一致！");
        }
    }
    let df_ip = member.df_ip.clone().unwrap_or_default();
    if !df_ip.is_empty() && !check_df_ip(&client_ip_of(headers), &df_ip) {
        return legacy_error("IP地址与报备IP不一致！");
    }
    let apikey = member.apikey.clone().unwrap_or_default();
    if !verify_sign(&sign, &sign_query_request(&apikey, &mchid, &out_trade_no)) {
        return legacy_error("验签失败!");
    }

    let order = match find_application(&state.db, user_id, &out_trade_no).await {
        Ok(Some(o)) => o,
        Ok(None) => {
            return legacy_json(
                StatusCode::OK,
                &serde_json::json!({
                    "status": "error", "msg": "请求成功", "refCode": "7", "refMsg": "交易不存在",
                }),
            );
        }
        Err(e) => return internal_error(e.message()),
    };

    let (code, msg) = ref_code_for(order.check_status, order.status);
    let mut fields: Vec<(String, String)> = vec![
        ("status".into(), "success".into()),
        ("msg".into(), "请求成功".into()),
        ("mchid".into(), mchid),
        (
            "out_trade_no".into(),
            order.out_trade_no.clone().unwrap_or_default(),
        ),
        ("amount".into(), units_to_yuan(order.tkmoney)),
        ("transaction_id".into(), order.order_no.clone()),
        ("refCode".into(), code.into()),
        ("refMsg".into(), msg.into()),
    ];
    if code == "1" {
        fields.push((
            "success_time".into(),
            order.settled_at.map(|t| t.to_string()).unwrap_or_default(),
        ));
    }
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let reply_sign = sign_pairs(&apikey, pairs.clone());
    let mut body = serde_json::Map::new();
    for (k, v) in &fields {
        body.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    body.insert("sign".into(), serde_json::Value::String(reply_sign));
    legacy_json(StatusCode::OK, &serde_json::Value::Object(body))
}

/// The legacy `$state_domain` table (`function.php:889-890`).
const STATE_DOMAIN: &[&str] = &[
    "al", "dz", "af", "ar", "ae", "aw", "om", "az", "eg", "et", "ie", "ee", "ad", "ao", "ai", "ag",
    "at", "au", "mo", "bb", "pg", "bs", "pk", "py", "ps", "bh", "pa", "br", "by", "bm", "bg", "mp",
    "bj", "be", "is", "pr", "ba", "pl", "bo", "bz", "bw", "bt", "bf", "bi", "bv", "kp", "gq", "dk",
    "de", "tl", "tp", "tg", "dm", "do", "ru", "ec", "er", "fr", "fo", "pf", "gf", "tf", "va", "ph",
    "fj", "fi", "cv", "fk", "gm", "cg", "cd", "co", "cr", "gg", "gd", "gl", "ge", "cu", "gp", "gu",
    "gy", "kz", "ht", "kr", "nl", "an", "hm", "hn", "ki", "dj", "kg", "gn", "gw", "ca", "gh", "ga",
    "kh", "cz", "zw", "cm", "qa", "ky", "km", "ci", "kw", "cc", "hr", "ke", "ck", "lv", "ls", "la",
    "lb", "lt", "lr", "ly", "li", "re", "lu", "rw", "ro", "mg", "im", "mv", "mt", "mw", "my", "ml",
    "mk", "mh", "mq", "yt", "mu", "mr", "us", "um", "as", "vi", "mn", "ms", "bd", "pe", "fm", "mm",
    "md", "ma", "mc", "mz", "mx", "nr", "np", "ni", "ne", "ng", "nu", "no", "nf", "na", "za", "aq",
    "gs", "eu", "pw", "pn", "pt", "jp", "se", "ch", "sv", "ws", "yu", "sl", "sn", "cy", "sc", "sa",
    "cx", "st", "sh", "kn", "lc", "sm", "pm", "vc", "lk", "sk", "si", "sj", "sz", "sd", "sr", "sb",
    "so", "tj", "tw", "th", "tz", "to", "tc", "tt", "tn", "tv", "tr", "tm", "tk", "wf", "vu", "gt",
    "ve", "bn", "ug", "ua", "uy", "uz", "es", "eh", "gr", "hk", "sg", "nc", "nz", "hu", "sy", "jm",
    "am", "ac", "ye", "iq", "ir", "il", "it", "in", "id", "uk", "vg", "io", "jo", "vn", "zm", "je",
    "td", "gi", "cl", "cf", "cn", "yr", "com", "arpa", "edu", "gov", "int", "mil", "net", "org",
    "biz", "info", "pro", "name", "museum", "coop", "aero", "xxx", "idv", "me", "mobi", "asia",
    "ax", "bl", "bq", "cat", "cw", "gb", "jobs", "mf", "rs", "su", "sx", "tel", "travel",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_domain_widens_only_for_a_known_penultimate_tld() {
        // two labels → the host itself
        assert_eq!(get_base_domain("example.com"), "example.com");
        assert_eq!(get_base_domain("localhost"), "localhost");
        // generic: drop the sub-domain
        assert_eq!(
            get_base_domain("http://a.b.example.com/x?y=1"),
            "example.com"
        );
        // co.uk: the penultimate label is itself a TLD → three labels
        assert_eq!(
            get_base_domain("http://sub.example.co.uk/path"),
            "example.co.uk"
        );
        // bare host without a scheme is treated the same
        assert_eq!(get_base_domain("www.shop.com.cn"), "shop.com.cn");
    }

    #[test]
    fn domain_whitelist_matches_the_referer_base_domain() {
        let recorded = "example.com\r\nshop.cn";
        assert!(check_df_domain("http://pay.example.com/x", recorded));
        assert!(check_df_domain("http://www.shop.cn", recorded));
        assert!(!check_df_domain("http://evil.com", recorded));
        assert!(
            !check_df_domain("", recorded),
            "empty referer never matches"
        );
    }

    #[test]
    fn ip_whitelist_is_exact_line_match() {
        let recorded = "10.0.0.1\r\n 10.0.0.2 \r\n";
        assert!(check_df_ip("10.0.0.1", recorded));
        assert!(check_df_ip("10.0.0.2", recorded));
        assert!(!check_df_ip("10.0.0.3", recorded));
        assert!(!check_df_ip("", recorded));
    }

    #[test]
    fn ref_code_ladder_matches_the_legacy_table() {
        // check_status: 0 待审核, 2 驳回, 1 folds the execution status.
        assert_eq!(ref_code_for(Some(0), 0), ("6", "待审核"));
        assert_eq!(ref_code_for(Some(2), 3), ("5", "审核驳回"));
        assert_eq!(ref_code_for(Some(1), 0), ("4", "待处理"));
        assert_eq!(ref_code_for(Some(1), 1), ("3", "处理中"));
        assert_eq!(ref_code_for(Some(1), 2), ("1", "成功"));
        assert_eq!(ref_code_for(Some(1), 3), ("2", "失败"));
        assert_eq!(ref_code_for(Some(1), 4), ("3", "待确认"));
        assert_eq!(ref_code_for(Some(1), 9), ("8", "未知状态"));
    }

    #[test]
    fn extends_decodes_standard_base64() {
        let json = "{\"k\":\"v\"}";
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        assert_eq!(decode_extends(&encoded).as_deref(), Some(json));
        assert_eq!(decode_extends(""), None);
        assert_eq!(decode_extends("   "), None);
    }

    #[test]
    fn client_ip_prefers_the_first_forwarded_hop() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.7, 70.41.30.2".parse().unwrap(),
        );
        assert_eq!(client_ip_of(&headers), "203.0.113.7");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-real-ip", "198.51.100.9".parse().unwrap());
        assert_eq!(client_ip_of(&headers), "198.51.100.9");
    }
}
