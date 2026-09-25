//! The merchant-panel handlers: the panel login (issues a Redis session),
//! logout, and the two §6.5 review batches. Every action past login is gated
//! by a live session token AND the merchant/agent portal capability, and each
//! review row is scoped to the acting merchant (`df_api_order where id,userid`
//! in the legacy `dfPass`) — a console can only ever move its OWN payout
//! applications, so a forged id list against another merchant folds to a
//! per-row failure rather than touching a foreign balance.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::Multipart;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::data::payout_orders;
use crate::gateway::dfpay::client_ip_of;
use crate::merchant::agent_rate::{self, RateRow};
use crate::merchant::apikey::{self, ApikeyOutcome};
use crate::merchant::article;
use crate::merchant::attachment::{self, uniqid};
use crate::merchant::bankcard::{self, BankcardForm};
use crate::merchant::charges;
use crate::merchant::console;
use crate::merchant::deposit::{self, DepositFilter};
use crate::merchant::downline::{self, DownlineFilter};
use crate::merchant::downline_order::{self, DownlineOrderFilter};
use crate::merchant::forgetpwd;
use crate::merchant::google::{self, BindResult, Initiate, PendingSecrets, UnbindResult};
use crate::merchant::invite;
use crate::merchant::login::{self, LoginOutcome};
use crate::merchant::loginrecord;
use crate::merchant::mobile::{self, BindOutcome, EditOutcome, SendOutcome};
use crate::merchant::password::{self, PwdOutcome};
use crate::merchant::profile;
use crate::merchant::profit_report::{self, DownlineReportFilter};
use crate::merchant::rbac::{has_permission, Permission};
use crate::merchant::register::{self, RegisterInput};
use crate::merchant::twofactor::{self, Factor, FactorGate};
use crate::merchant::{generate_apikey, MembersRepo, Role};
use crate::payout::state::Source;
use crate::payout::{parse_batch_ids, ReviewBatchReport, ReviewOutcome};
use crate::ratelimit::AuthLimiter;
use crate::sms::{self, SmsCodes};
use crate::state::{AppState, GatewayError};

use super::session::{session_is_live, PanelSession, SessionStore};

/// A panel failure carrying its HTTP status and the caller-facing message.
#[derive(Debug)]
pub struct PanelError {
    status: StatusCode,
    msg: String,
}

impl PanelError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            msg: msg.into(),
        }
    }
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, msg)
    }
    fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg)
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl From<GatewayError> for PanelError {
    fn from(e: GatewayError) -> Self {
        match e {
            GatewayError::BadRequest(m) => Self::new(StatusCode::BAD_REQUEST, m),
            GatewayError::Internal(m) => Self::internal(m),
        }
    }
}

impl From<sea_orm::DbErr> for PanelError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::internal(format!("db: {e}"))
    }
}

impl From<redis::RedisError> for PanelError {
    fn from(e: redis::RedisError) -> Self {
        Self::internal(format!("session store: {e}"))
    }
}

impl IntoResponse for PanelError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"status": "fail", "msg": self.msg})),
        )
            .into_response()
    }
}

// --- session plumbing -------------------------------------------------------

/// Pulls the bearer token from `Authorization: Bearer …` (or the
/// `X-Panel-Token` convenience header).
fn bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let rest = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "));
        if let Some(t) = rest {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    headers
        .get("x-panel-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolves the request's session, failing closed to a 401 when the token is
/// absent / expired / unknown.
async fn require_session(
    store: &SessionStore,
    headers: &HeaderMap,
) -> Result<(PanelSession, String), PanelError> {
    let token = bearer(headers).ok_or_else(|| PanelError::unauthorized("未登录"))?;
    let session = store
        .lookup(&token)
        .await
        .ok_or_else(|| PanelError::unauthorized("登录已失效，请重新登录"))?;
    Ok((session, token))
}

/// [`require_session`] plus the single-sign-on kick (§4.6): the session's
/// `version` must still equal the member's CURRENT `session_version`, so a
/// login elsewhere (which bumps it) revokes this token on its next request.
/// This is the merchant-panel analogue of the legacy `UserController`
/// constructor re-reading `session_random` on every request — the faithful,
/// non-typo version (see the module decision note).
pub async fn require_live_session(
    store: &SessionStore,
    db: &sea_orm::DatabaseConnection,
    headers: &HeaderMap,
) -> Result<PanelSession, PanelError> {
    let (session, _) = require_session(store, headers).await?;
    let member = MembersRepo::new(db)
        .by_id(session.user_id)
        .await?
        .ok_or_else(|| PanelError::unauthorized("登录已失效，请重新登录"))?;
    if !session_is_live(&session, member.session_version.as_deref().unwrap_or("")) {
        return Err(PanelError::unauthorized(
            "您的账号在别处登录，如非本人操作，请立即修改登录密码！",
        ));
    }
    Ok(session)
}

/// The panel is merchant/agent-only: a platform console (or any role without a
/// portal capability) is refused — the platform reviews through the back-office
/// surface, not here (§6.3「商户 API 面板，非平台管理员」).
fn ensure_portal(session: &PanelSession) -> Result<(), PanelError> {
    let role = Role::from_groupid(session.groupid);
    if has_permission(role, Permission::MerchantPortal)
        || has_permission(role, Permission::AgentPortal)
    {
        Ok(())
    } else {
        Err(PanelError::forbidden("无权访问代付审核"))
    }
}

// --- login / logout ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

fn login_failure(outcome: &LoginOutcome) -> String {
    match outcome {
        LoginOutcome::Banned { retry_after_secs } => {
            format!("登录失败次数过多，请 {retry_after_secs} 秒后重试")
        }
        LoginOutcome::UserNotFound | LoginOutcome::BadPassword => "用户名或密码错误".to_string(),
        LoginOutcome::IpNotAllowed => "IP不在授权列表".to_string(),
        LoginOutcome::Disabled => "账户已禁用".to_string(),
        LoginOutcome::Success { .. } => String::new(),
    }
}

/// `POST /panel/login` — runs the merchant login check (with the client IP for
/// the §4.2 whitelist) and, on success, bumps the member's single-sign-on
/// `session_version` (§4.6) and issues a session carrying it, so any older
/// session for this account is revoked on its next request.
pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Result<Response, PanelError> {
    let client_ip = client_ip_of(&headers);
    let outcome = login::login(&state, &req.username, &req.password, &client_ip).await?;
    let LoginOutcome::Success { user_id, .. } = outcome else {
        return Err(PanelError::unauthorized(login_failure(&outcome)));
    };
    let member = MembersRepo::new(&state.db)
        .by_id(user_id)
        .await?
        .ok_or_else(|| PanelError::unauthorized("账户不存在"))?;
    // Append the front-console login audit row (§5.4). Best-effort: an
    // append-log failure must not block an otherwise valid login, and the
    // `loginaddress` geolocation stays a seam (`None`) — no IP provider wired.
    if let Err(e) =
        loginrecord::record_login(&state.db, user_id, &client_ip, None, crate::data::now()).await
    {
        tracing::warn!(
            "loginrecord append failed for uid {user_id}: {}",
            e.message()
        );
    }
    // Mint a fresh single-sign-on version, persist it (revoking older
    // sessions), and bind this session to it.
    let version = generate_apikey();
    MembersRepo::new(&state.db)
        .bump_session_version(user_id, &version)
        .await?;
    let session = PanelSession {
        user_id,
        groupid: member.groupid,
        version,
    };
    ensure_portal(&session)?;
    let store = SessionStore::new(state.redis.clone());
    let token = store.issue(session).await?;
    Ok(Json(json!({
        "status": "success",
        "token": token,
        "user_id": user_id,
        "groupid": member.groupid,
    }))
    .into_response())
}

/// `POST /panel/logout` — revokes the caller's session.
pub async fn logout_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let (_, token) = require_session(&store, &headers).await?;
    store.revoke(&token).await;
    Ok(Json(json!({"status": "success"})).into_response())
}

// --- review batches (§6.5) --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BatchReq {
    /// The `_`-joined order-no list the panel JS submits
    /// (`ids.join('_')`, `dfPassBatch:2600`).
    #[serde(default)]
    pub ids: String,
    /// The reject reason; the batch form defaults it to empty (§6.5).
    #[serde(default)]
    pub reason: Option<String>,
}

fn outcome_label(o: &ReviewOutcome) -> &'static str {
    match o {
        ReviewOutcome::Approved(_) => "审核通过",
        ReviewOutcome::AlreadyApproved(_) => "已通过",
        ReviewOutcome::Rejected { .. } => "已驳回",
        ReviewOutcome::AlreadyRejected(_) => "已驳回",
        ReviewOutcome::NotRejectable(_) => "后台已处理代付，不能驳回",
    }
}

fn report_json(rep: &ReviewBatchReport) -> Value {
    let mut results: Vec<Value> = rep
        .succeeded
        .iter()
        .map(|(no, o)| json!({"order_no": no, "status": "ok", "message": outcome_label(o)}))
        .collect();
    for (no, m) in &rep.failures {
        results.push(json!({"order_no": no, "status": "fail", "message": m}));
    }
    json!({
        "status": "success",
        "summary": rep.summary(),
        "succeeded": rep.succeeded_count(),
        "failed": rep.failed_count(),
        "results": results,
    })
}

/// Splits a submitted id list into the ids the caller OWNS (present in
/// `owned`) and the rest, PRESERVING the submitted order and dropping
/// duplicates the ownership query cannot carry. Pure so the authorization
/// partition is pinned offline.
fn partition_owned(ids: &[String], owned: &HashSet<String>) -> (Vec<String>, Vec<String>) {
    let allowed: Vec<String> = ids.iter().filter(|i| owned.contains(*i)).cloned().collect();
    let denied: Vec<String> = ids
        .iter()
        .filter(|i| !owned.contains(*i))
        .cloned()
        .collect();
    (allowed, denied)
}

/// The ownership-scoped review: keep only the ids that belong to the acting
/// merchant's own `source = 3` applications, run the batch over those, and fold
/// every foreign / unknown id into a per-row 代付申请不存在 failure — the exact
/// legacy `df_api_order where id, userid` outcome.
async fn scoped_review(
    state: &AppState,
    session: PanelSession,
    ids: &[String],
    pass: bool,
    reason: &str,
    now_ts: i64,
) -> Result<ReviewBatchReport, PanelError> {
    let mut rep = if ids.is_empty() {
        ReviewBatchReport::default()
    } else {
        let owned: HashSet<String> = payout_orders::Entity::find()
            .filter(payout_orders::Column::OrderNo.is_in(ids.to_vec()))
            .filter(payout_orders::Column::UserId.eq(session.user_id))
            .filter(payout_orders::Column::Source.eq(Source::PayoutApi.code()))
            .all(&state.db)
            .await?
            .into_iter()
            .map(|m| m.order_no)
            .collect();
        let (allowed, denied) = partition_owned(ids, &owned);
        let mut rep = if pass {
            state.payout.df_pass_batch(&allowed, now_ts).await?
        } else {
            state
                .payout
                .df_reject_batch(&allowed, reason, now_ts)
                .await?
        };
        for d in denied {
            rep.failures.push((d, "代付申请不存在".to_string()));
        }
        rep
    };
    // Keep the summary stable regardless of push order above.
    rep.succeeded.sort_by(|a, b| a.0.cmp(&b.0));
    rep.failures.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(rep)
}

async fn run_batch(
    state: Arc<AppState>,
    headers: HeaderMap,
    req: BatchReq,
    pass: bool,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let ids = parse_batch_ids(&req.ids);
    let reason = req.reason.unwrap_or_default();
    let now_ts = chrono::Local::now().timestamp();
    let rep = scoped_review(&state, session, &ids, pass, &reason, now_ts).await?;
    Ok(Json(report_json(&rep)).into_response())
}

/// `POST /panel/payout/df_pass_batch` — §6.5 批量审核通过.
pub async fn df_pass_batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BatchReq>,
) -> Result<Response, PanelError> {
    run_batch(state, headers, req, true).await
}

/// `POST /panel/payout/df_reject_batch` — §6.5 批量审核驳回.
pub async fn df_reject_batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BatchReq>,
) -> Result<Response, PanelError> {
    run_batch(state, headers, req, false).await
}

// --- agent downline rate config (§6.2) --------------------------------------

/// One product's rate as the agent console submits it: a decimal rate
/// FRACTION string (`"0.0060"` == 0.6%) and a 元 cap string, mirroring the
/// legacy form fields (`feilv` / `fengding` / `t0feilv` / `t0fengding`). An
/// empty string means unset → `0`.
#[derive(Debug, Deserialize)]
pub struct RateForm {
    /// The product id (legacy `payapiid`).
    pub payapiid: i64,
    #[serde(default)]
    pub feilv: String,
    #[serde(default)]
    pub fengding: String,
    #[serde(default)]
    pub t0feilv: String,
    #[serde(default)]
    pub t0fengding: String,
}

#[derive(Debug, Deserialize)]
pub struct SaveUserRateReq {
    /// The downline merchant being priced (legacy `post.userid`).
    pub userid: i64,
    /// The per-product rate rows (legacy `post.u/a`, keyed by `payapiid`).
    #[serde(default)]
    pub rates: Vec<RateForm>,
}

/// The agent console may set a downline rate; a plain merchant (or the
/// platform, already rejected by [`ensure_portal`]) may not (§6.2, the
/// `AgentController` constructor + `Permission::SetSubRate`).
fn ensure_sub_rate(session: &PanelSession) -> Result<(), PanelError> {
    let role = Role::from_groupid(session.groupid);
    if has_permission(role, Permission::SetSubRate) {
        Ok(())
    } else {
        Err(PanelError::forbidden("无权配置下级费率"))
    }
}

/// Parses a decimal rate FRACTION field to `RATE_SCALE` units; empty → `0`.
fn parse_rate_field(raw: &str) -> Result<i64, PanelError> {
    if raw.trim().is_empty() {
        return Ok(0);
    }
    crate::money::parse_rate_to_scaled(raw)
        .ok_or_else(|| PanelError::new(StatusCode::BAD_REQUEST, "费率格式错误"))
}

/// Parses a 元 cap field to money units; empty → `0`.
fn parse_cap_field(raw: &str) -> Result<i64, PanelError> {
    if raw.trim().is_empty() {
        return Ok(0);
    }
    crate::money::parse_yuan_to_units(raw)
        .ok_or_else(|| PanelError::new(StatusCode::BAD_REQUEST, "封顶金额格式错误"))
}

/// `POST /panel/agent/save_user_rate` — §6.2 代理给下级配费率. Session- and
/// agent-gated, then ownership-checked (the target must be this agent's direct
/// child) before [`agent_rate::apply_agent_rates`] runs the cost-floor
/// validation + upsert. A cost-floor breach replies `{status:"0",msg}` (HTTP
/// 200, the legacy `ajaxReturn` shape); a clean write replies `{status:"1"}`.
pub async fn save_user_rate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SaveUserRateReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_sub_rate(&session)?;

    // Ownership (§6.2 L341-347): the downline must exist and report to this agent.
    let member = MembersRepo::new(&state.db)
        .by_id(req.userid)
        .await?
        .ok_or_else(|| PanelError::new(StatusCode::BAD_REQUEST, "用户不存在！"))?;
    if member.parentid != session.user_id {
        return Err(PanelError::forbidden("您没有权限查对该用户进行费率设置！"));
    }

    let mut rows = Vec::with_capacity(req.rates.len());
    for r in &req.rates {
        rows.push(RateRow {
            product_id: r.payapiid,
            rate: parse_rate_field(&r.feilv)?,
            fengding: parse_cap_field(&r.fengding)?,
            t0_rate: parse_rate_field(&r.t0feilv)?,
            t0_fengding: parse_cap_field(&r.t0fengding)?,
        });
    }

    match agent_rate::apply_agent_rates(&state.db, session.user_id, req.userid, &rows).await? {
        Ok(()) => Ok(Json(json!({ "status": "1" })).into_response()),
        Err(violation) => Ok(Json(json!({ "status": "0", "msg": violation.msg })).into_response()),
    }
}

/// The §6.2 read form (`userRateEdit` GET `uid`): the downline child whose
/// opened products + current rates should be loaded for editing.
#[derive(Debug, Deserialize)]
pub struct UserRateEditReq {
    pub userid: i64,
}

/// `POST /panel/agent/user_rate_edit` — §6.2 下级费率编辑页读侧. Session-,
/// agent- and ownership-gated exactly like [`save_user_rate`]; replies
/// `{status:1,data:{list:[{product_id,name,feilv,fengding,t0feilv,t0fengding}]}}`
/// for the child's opened products (integer representation: rates
/// `RATE_SCALE`-scaled, caps money units; a not-yet-priced product is all-`0`,
/// the legacy `'0.000'` placeholder).
pub async fn user_rate_edit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<UserRateEditReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_sub_rate(&session)?;

    // Ownership (§6.2 L301-307): the target must exist and report to this agent.
    let member = MembersRepo::new(&state.db)
        .by_id(req.userid)
        .await?
        .ok_or_else(|| PanelError::new(StatusCode::BAD_REQUEST, "用户不存在！"))?;
    if member.parentid != session.user_id {
        return Err(PanelError::forbidden("您没有权限查对该用户进行费率设置！"));
    }

    let rows = agent_rate::downline_rate_edit(&state.db, req.userid).await?;
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "product_id": r.product_id,
                "name": r.name,
                "feilv": r.rate,
                "fengding": r.fengding,
                "t0feilv": r.t0_rate,
                "t0fengding": r.t0_fengding,
            })
        })
        .collect();
    Ok(Json(json!({ "status": 1, "data": { "list": list } })).into_response())
}

// --- agent invite codes (§6.3) ----------------------------------------------

/// The mint form (§6.3 `addInvitecode`): the group the code admits and an
/// optional `Y-m-d H:i:s` expiry (the `addInvite` pre-generation fills it with
/// `now + 86400`; empty defaults to that same day window).
#[derive(Debug, Deserialize)]
pub struct CreateInviteReq {
    /// `post.regtype` — the downline group this invite admits.
    pub regtype: i32,
    #[serde(default)]
    pub yxdatetime: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteInviteReq {
    /// `post.id` — the code row to remove.
    pub id: i64,
}

/// Only an agent mints / deletes invite codes; a plain merchant (and the
/// platform, already rejected by [`ensure_portal`]) is refused — the legacy
/// `AgentController` constructor denies groupid 4.
fn ensure_agent(session: &PanelSession) -> Result<(), PanelError> {
    let role = Role::from_groupid(session.groupid);
    if has_permission(role, Permission::ManageSubMerchant) {
        Ok(())
    } else {
        Err(PanelError::forbidden("无权管理下级"))
    }
}

/// `POST /panel/agent/create_invite` — §6.3 生成邀请码. Agent-gated, then
/// [`invite::create_invite`] runs the tier gate and mints a unique code. A
/// non-lower `regtype` replies `{status:"0",msg:"没有权限"}` (HTTP 200); a
/// clean mint replies `{status:"1", invitecode, yxdatetime}`.
pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateInviteReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let now = chrono::Local::now().timestamp();
    let yxdatetime = invite::parse_yxdatetime(&req.yxdatetime, now);
    match invite::create_invite(
        &state.db,
        session.user_id,
        session.groupid,
        req.regtype,
        yxdatetime,
    )
    .await?
    {
        Ok(m) => Ok(Json(json!({
            "status": "1",
            "invitecode": m.invitecode,
            "yxdatetime": m.yxdatetime,
        }))
        .into_response()),
        Err(msg) => Ok(Json(json!({ "status": "0", "msg": msg })).into_response()),
    }
}

/// `POST /panel/agent/delete_invite` — §6.3 删除邀请码. Agent-gated; only the
/// caller's OWN, non-admin codes are removed (the legacy
/// `where(id, fmusernameid, is_admin = 0)`), so a foreign / admin id deletes
/// nothing. Replies `{status:<rows-affected>}` like the legacy `ajaxReturn`.
pub async fn delete_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DeleteInviteReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let affected = invite::delete_invite(&state.db, req.id, session.user_id).await?;
    Ok(Json(json!({ "status": affected.to_string() })).into_response())
}

/// The §6.1 代理开商户 form (`User/AgentController::saveUser` → `u/a`): the
/// username / email / (optional) login password. An empty password makes the
/// kernel mint a `random_str(6)` the Noop email would deliver.
#[derive(Debug, Deserialize)]
pub struct AgentSaveUserReq {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub password: String,
}

/// `POST /panel/agent/save_user` — §6.1 代理开商户. Agent-gated, then
/// [`register::open_downline_merchant`] runs the username / email duplicate
/// check and files a `groupid = 4` merchant parented to the caller. A
/// duplicate replies `{status:"0",msg}` (HTTP 200); a clean open replies
/// `{status:"1",uid}`. Deviation: the legacy replies `{status:<insert-id>}`;
/// the id moves to a dedicated `uid` and `status` is the usual flag.
pub async fn agent_save_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AgentSaveUserReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let username = req.username.trim();
    let email = req.email.trim();
    if username.is_empty() || email.is_empty() {
        return Ok(Json(json!({ "status": "0", "msg": "用户名和邮箱不能为空" })).into_response());
    }
    // §6.1 inherits the same site switches for the child's status/authorized;
    // the invite gate never applies to an agent-opened merchant.
    let flags = register::SiteFlags {
        invitecode: false,
        authorized: state.cfg.register_authorized,
        register_need_activate: state.cfg.register_need_activate,
        data_auth_key: state.cfg.data_auth_key.clone(),
    };
    let input = register::OpenChildInput {
        username,
        email,
        password: req.password.trim(),
    };
    match register::open_downline_merchant(&state.db, session.user_id, &input, &flags).await? {
        Ok(uid) => Ok(Json(json!({ "status": "1", "uid": uid })).into_response()),
        Err(e) => Ok(Json(json!({ "status": "0", "msg": e.message() })).into_response()),
    }
}

/// The §6.4 下级会员 list form (`member()` GET filters): the username / 商户号
/// search box, the optional `status` / `authorized` matches and the page/rows.
#[derive(Debug, Deserialize)]
pub struct DownlineListReq {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub authorized: Option<i32>,
    #[serde(default)]
    pub page: Option<u64>,
    #[serde(default)]
    pub rows: Option<u64>,
}

/// `POST /panel/agent/downline_list` — §6.4 下级会员分页列表. Agent-gated;
/// [`downline`] scopes to `parentid = caller` (`groupid != 1`). Replies
/// `{status:1,data:{total,page,rows,list:[{id,mch_id,username,groupid,status,
/// authorized,balance,blocked_balance,email}]}}`. Faithful quirk: the legacy
/// `!empty($status)`/`!empty($authorized)` guards ignore a posted `0`, so `0`
/// is collapsed to "no filter" (you cannot list only-disabled rows).
pub async fn downline_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DownlineListReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let page = req.page.unwrap_or(1).max(1);
    let rows = req.rows.unwrap_or(downline::PAGE_SIZE).max(1);
    let filter = DownlineFilter {
        username: req.username,
        status: req.status.filter(|s| *s != 0),
        authorized: req.authorized.filter(|a| *a != 0),
    };
    let total = downline::count_filtered(&state.db, session.user_id, &filter).await?;
    let page_rows = downline::list_page(&state.db, session.user_id, &filter, page, rows).await?;
    let list: Vec<Value> = page_rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "mch_id": crate::merchant::mch_id_of(r.id),
                "username": r.username,
                "groupid": r.groupid,
                "status": r.status,
                "authorized": r.authorized,
                "balance": r.balance,
                "blocked_balance": r.blocked_balance,
                "email": r.email,
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": { "total": total, "page": page, "rows": rows, "list": list },
    }))
    .into_response())
}

/// The §6.4 启停 form (`editStatus()` POST): the target child `uid` and the
/// `isopen` flag (legacy `isopen ? isopen : 0` — the raw value, `0` disables).
#[derive(Debug, Deserialize)]
pub struct DownlineStatusReq {
    #[serde(default)]
    pub uid: i64,
    #[serde(default)]
    pub isopen: i32,
}

/// `POST /panel/agent/downline_set_status` — §6.4 下级启停. Agent-gated;
/// [`downline::set_child_status`] refuses a non-direct-child. A missing / foreign
/// target replies `{status:0,msg}`; a clean toggle replies `{status:1}`.
/// Deviation: the legacy replies `{status:<affected-rows>}` (a same-value write
/// is `0` on MySQL); here any successful persist is `1`.
pub async fn downline_set_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DownlineStatusReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let outcome =
        downline::set_child_status(&state.db, session.user_id, req.uid, req.isopen).await?;
    Ok(match outcome {
        downline::StatusOutcome::Updated => Json(json!({ "status": 1 })).into_response(),
        downline::StatusOutcome::NotFound => ajax_err("用户不存在！"),
        downline::StatusOutcome::NotOwned => ajax_err("您没有权限查切换该用户状态！"),
    })
}

/// The §6.4 / §7 downline earnings report form. `userid` selects a single
/// DIRECT child (the legacy `childord` detail); omitted = the whole tree. The
/// optional windows are unix-second bounds on the order's `apply_date` /
/// `success_date`.
#[derive(Debug, Deserialize)]
pub struct DownlineReportReq {
    #[serde(default)]
    pub userid: Option<i64>,
    #[serde(default)]
    pub apply_start: Option<i64>,
    #[serde(default)]
    pub apply_end: Option<i64>,
    #[serde(default)]
    pub success_start: Option<i64>,
    #[serde(default)]
    pub success_end: Option<i64>,
}

/// `POST /panel/agent/downline_report` — §7 三级分润树读面 / §6.4 `childord`
/// 成交+分润聚合. Agent-gated. Replies
/// `{status:1,data:{list:[{child_id,mch_id,username,trade_amount,poundage,
/// actual_amount,order_count,agent_profit}]}}` (money in units). A `userid`
/// that is not the caller's direct child yields an empty scope →
/// `{status:0,msg:'您没有权限查看该用户信息！'}` (the rewrite cannot cheaply
/// distinguish 不存在 vs 非直属, so both map to the legacy permission message).
pub async fn downline_report(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DownlineReportReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let filter = DownlineReportFilter {
        child: req.userid,
        apply_start: req.apply_start,
        apply_end: req.apply_end,
        success_start: req.success_start,
        success_end: req.success_end,
    };
    let rows = profit_report::downline_profit_report(&state.db, session.user_id, &filter).await?;
    if req.userid.is_some() && rows.is_empty() {
        return Ok(ajax_err("您没有权限查看该用户信息！"));
    }
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "child_id": r.child_id,
                "mch_id": crate::merchant::mch_id_of(r.child_id),
                "username": r.username,
                "trade_amount": r.trade_amount,
                "poundage": r.poundage,
                "actual_amount": r.actual_amount,
                "order_count": r.order_count,
                "agent_profit": r.agent_profit,
            })
        })
        .collect();
    Ok(Json(json!({ "status": 1, "data": { "list": list } })).into_response())
}

/// The §6.5 export form (legacy `exportuser` GET): the same username / status /
/// authorized filter matrix as §6.4 (posted as JSON), no pagination — the whole
/// (capped) downline is rendered at once.
#[derive(Debug, Deserialize)]
pub struct DownlineExportReq {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub authorized: Option<i32>,
}

/// `POST /panel/agent/export_user` — §6.5 下级会员导出. Agent-gated; reuses the
/// §6.4 [`downline`] filter + list read and renders a UTF-8 (BOM) CSV with a
/// `text/csv` + attachment disposition (a byte-stream stand-in for the legacy
/// PHPExcel `.xls`). Errors fold to the panel JSON, but a success is raw bytes.
pub async fn agent_export_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DownlineExportReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let filter = DownlineFilter {
        username: req.username,
        // Legacy `!empty($status)` / `!empty($authorized)` ignore a posted 0.
        status: req.status.filter(|s| *s != 0),
        authorized: req.authorized.filter(|a| *a != 0),
    };
    // Every exported row's 上级用户名 is the acting agent (the scope is its own
    // direct children), so a single lookup fills the whole column.
    let parent_name = MembersRepo::new(&state.db)
        .by_id(session.user_id)
        .await?
        .map(|m| m.username)
        .unwrap_or_default();
    let rows =
        downline::list_page(&state.db, session.user_id, &filter, 1, downline::EXPORT_CAP).await?;
    let bytes = downline::render_export_csv(&parent_name, &rows);

    let mut resp = axum::body::Body::from(bytes).into_response();
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=\"downline_export.csv\""),
    );
    Ok(resp)
}

// --- agent downline order detail (§6.4 `order` / `exportorder`) -------------

/// The §6.4 order-detail form (`order` GET matrix): an optional single-child
/// `memberid` (wire 商户号), order-number / product-name text legs, the two
/// epoch-seconds time windows, and paging.
#[derive(Debug, Deserialize)]
pub struct DownlineOrderReq {
    #[serde(default)]
    pub memberid: Option<i64>,
    #[serde(default)]
    pub orderid: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub apply_start: Option<i64>,
    #[serde(default)]
    pub apply_end: Option<i64>,
    #[serde(default)]
    pub success_start: Option<i64>,
    #[serde(default)]
    pub success_end: Option<i64>,
    #[serde(default)]
    pub page: u64,
    #[serde(default)]
    pub rows: Option<u64>,
}

impl DownlineOrderReq {
    fn filter(&self) -> DownlineOrderFilter {
        DownlineOrderFilter {
            memberid: self.memberid,
            order_id: self.orderid.clone().filter(|s| !s.is_empty()),
            product_name: self.body.clone().filter(|s| !s.is_empty()),
            apply_start: self.apply_start,
            apply_end: self.apply_end,
            success_start: self.success_start,
            success_end: self.success_end,
        }
    }
}

/// `POST /panel/agent/downline_order_list` — §6.4 下级订单明细分页. Agent-gated.
/// Replies `{status:1,data:{total,page,rows,stats,list:[{…order…}]}}` (money in
/// units). `stats` carries either the 今日/累计 roll-up (no window posted,
/// `{mode:"summary",…}`) or the filtered window total (`{mode:"windowed",…}`),
/// mirroring the legacy branch.
pub async fn downline_order_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DownlineOrderReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let page_no = req.page.max(1);
    let rows = req.rows.unwrap_or(downline_order::PAGE_SIZE).max(1);
    let page = downline_order::downline_order_page(
        &state.db,
        session.user_id,
        &req.filter(),
        page_no,
        rows,
    )
    .await?;
    let stats = match page.stats {
        downline_order::OrderStats::Summary {
            today_amount,
            today_count,
            total_amount,
            total_count,
        } => json!({
            "mode": "summary",
            "today_amount": today_amount,
            "today_count": today_count,
            "total_amount": total_amount,
            "total_count": total_count,
        }),
        downline_order::OrderStats::Windowed {
            amount,
            actual_amount,
            count,
        } => json!({
            "mode": "windowed",
            "amount": amount,
            "actual_amount": actual_amount,
            "count": count,
        }),
    };
    let list: Vec<Value> = page
        .orders
        .iter()
        .map(|o| {
            json!({
                "id": o.id,
                "user_id": o.user_id,
                "mch_id": o.mch_id,
                "order_id": o.order_id,
                "out_trade_id": o.out_trade_id,
                "amount": o.amount,
                "poundage": o.poundage,
                "actual_amount": o.actual_amount,
                "apply_date": o.apply_date,
                "success_date": o.success_date,
                "channel_code": o.channel_code,
                "product_name": o.product_name,
                "status": o.status,
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": { "total": page.total, "page": page_no, "rows": rows, "stats": stats, "list": list },
    }))
    .into_response())
}

/// `POST /panel/agent/export_order` — §6.4 下级订单导出. Agent-gated; reuses the
/// same filter matrix but narrows to successes (`status IN (1,2)`) and renders
/// the whole (capped) set as a UTF-8 (BOM) CSV byte-stream (the legacy
/// `exportorder` `.xls` stand-in), mirroring [`agent_export_user`].
pub async fn agent_export_order(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DownlineOrderReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    ensure_agent(&session)?;

    let rows =
        downline_order::downline_order_export(&state.db, session.user_id, &req.filter()).await?;
    let bytes = downline_order::render_order_csv(&rows);

    let mut resp = axum::body::Body::from(bytes).into_response();
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=\"downline_order_export.csv\""),
    );
    Ok(resp)
}

// --- API-key reveal (§9) ----------------------------------------------------

/// The reveal form (§9 `apikey`): the payment-password second factor
/// (`request.code`).
#[derive(Debug, Deserialize)]
pub struct ApikeyViewReq {
    #[serde(default)]
    pub code: String,
}

/// `POST /panel/apikey/view` — §9 查看 APIKEY. Session- and portal-gated, then
/// [`apikey::view_apikey`] runs the auth_type=6 lockout gate and the
/// payment-password second factor. Every branch replies HTTP 200 in the legacy
/// `ajaxReturn` shape: a lock / wrong password is `{status:"0",msg}`, a
/// verified reveal is `{status:"1",apikey}`.
pub async fn apikey_view(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ApikeyViewReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;

    let limiter = AuthLimiter::new(state.redis.clone());
    let outcome = apikey::view_apikey(&state.db, &limiter, session.user_id, &req.code).await?;
    let body = match outcome {
        ApikeyOutcome::Locked { msg } => json!({ "status": "0", "msg": msg }),
        ApikeyOutcome::BadPassword { msg } => json!({ "status": "0", "msg": msg }),
        ApikeyOutcome::Revealed { apikey } => json!({ "status": "1", "apikey": apikey }),
    };
    Ok(Json(body).into_response())
}

// --- merchant profile write (§10) -------------------------------------------

/// The `saveProfile` form (§10): the `auth_type` selector (`0` = SMS, `1` =
/// Google), the Google `google_code`, and every whitelisted profile column as
/// an `Option<String>` — a `Some` (present key) is a write (empty string
/// included, matching legacy `save($p)`), a `None` leaves the column untouched.
#[derive(Debug, Deserialize)]
pub struct ProfileSaveReq {
    #[serde(default)]
    pub auth_type: i32,
    #[serde(default)]
    pub google_code: Option<String>,
    #[serde(default)]
    pub agentname: Option<String>,
    #[serde(default)]
    pub realname: Option<String>,
    #[serde(default)]
    pub sfznumber: Option<String>,
    #[serde(default)]
    pub mobile: Option<String>,
    #[serde(default)]
    pub qq: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub login_ip: Option<String>,
    #[serde(default)]
    pub df_domain: Option<String>,
    #[serde(default)]
    pub df_ip: Option<String>,
    #[serde(default)]
    pub sex: Option<String>,
    #[serde(default)]
    pub df_api: Option<String>,
    #[serde(default)]
    pub df_auto_check: Option<String>,
    #[serde(default)]
    pub birthday: Option<String>,
}

/// `POST /panel/profile/save` — §10 编辑资料. Session- and portal-gated, then
/// the conditional second factor (`required_factor` matrix over the member's
/// Google secret and the [`twofactor::sms_status`] seam): a Google factor must
/// pass [`twofactor::verify_google`], an SMS factor is unwired (unreachable) so
/// it folds to the legacy param error, and with neither configured the write
/// proceeds. A passed gate plans the whitelist via [`profile::plan_profile`]
/// (an unknown `agentname` still rejects the whole save) and applies it. Every
/// branch replies HTTP 200 in the `ajaxReturn` shape.
pub async fn profile_save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ProfileSaveReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;

    let member = MembersRepo::new(&state.db)
        .by_id(session.user_id)
        .await?
        .ok_or_else(|| PanelError::unauthorized("账户不存在"))?;
    let secret = member.google_secret_key.clone().unwrap_or_default();
    let has_google = !secret.trim().is_empty();

    let factor =
        match twofactor::required_factor(has_google, twofactor::sms_status(), req.auth_type) {
            Ok(f) => f,
            Err(msg) => return Ok(Json(json!({ "status": "0", "msg": msg })).into_response()),
        };
    match factor {
        Factor::Google => {
            let code = req.google_code.clone().unwrap_or_default();
            let now = chrono::Local::now().timestamp();
            let limiter = AuthLimiter::new(state.redis.clone());
            if let FactorGate::Rejected { msg } =
                twofactor::verify_google(&limiter, session.user_id, &secret, &code, now).await
            {
                return Ok(Json(json!({ "status": "0", "msg": msg })).into_response());
            }
        }
        // The SMS factor is not wired (`sms_status() == false` makes it
        // unreachable); if ever reached, reject as the legacy param error.
        Factor::Sms => {
            return Ok(
                Json(json!({ "status": "0", "msg": twofactor::MSG_PARAM_ERR })).into_response(),
            );
        }
        Factor::None => {}
    }

    // Build the posted-field map (present = write), then resolve the agent-name
    // lookup once so `plan_profile`'s gate can run DB-free.
    let mut posted: Vec<(&str, String)> = Vec::new();
    for (key, value) in [
        ("agentname", req.agentname.clone()),
        ("realname", req.realname.clone()),
        ("sfznumber", req.sfznumber.clone()),
        ("mobile", req.mobile.clone()),
        ("qq", req.qq.clone()),
        ("address", req.address.clone()),
        ("login_ip", req.login_ip.clone()),
        ("df_domain", req.df_domain.clone()),
        ("df_ip", req.df_ip.clone()),
        ("sex", req.sex.clone()),
        ("df_api", req.df_api.clone()),
        ("df_auto_check", req.df_auto_check.clone()),
        ("birthday", req.birthday.clone()),
    ] {
        if let Some(v) = value {
            posted.push((key, v));
        }
    }
    let agent_exists = match req.agentname.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => MembersRepo::new(&state.db)
            .by_username(name)
            .await?
            .is_some(),
        _ => false,
    };
    let update = match profile::plan_profile(&posted, |_| agent_exists) {
        Ok(u) => u,
        Err(msg) => return Ok(Json(json!({ "status": "0", "msg": msg })).into_response()),
    };
    profile::apply_profile(&state.db, session.user_id, &update).await?;
    Ok(Json(json!({ "status": "1", "msg": "编辑成功" })).into_response())
}

// --- settlement bank cards (§10) --------------------------------------------

/// The `addBankcard` form (§10): an optional `id` (present = update an OWNED
/// card, absent = insert), the card text fields, and the reserved SMS-factor
/// fields (`auth_type` / `code`) the legacy form posts but the unwired SMS
/// seam ignores.
#[derive(Debug, Deserialize)]
pub struct BankcardSaveReq {
    #[serde(default)]
    pub id: Option<i64>,
    /// Reserved for the (unwired) `addBankcard` SMS factor.
    #[serde(default)]
    #[allow(dead_code)]
    pub auth_type: i32,
    /// Reserved for the (unwired) `addBankcard` SMS code.
    #[serde(default)]
    #[allow(dead_code)]
    pub code: String,
    #[serde(default)]
    pub bankname: String,
    #[serde(default)]
    pub subbranch: String,
    #[serde(default)]
    pub accountname: String,
    #[serde(default)]
    pub cardnumber: String,
    #[serde(default)]
    pub province: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub alias: String,
}

/// `POST /panel/bankcard/save` — §10 新增/编辑银行卡. Session- and portal-gated.
/// Legacy gates this on the SMS factor alone; with the SMS seam closed
/// (`sms_status() == false`) nothing gates the write, so it goes straight to
/// [`bankcard::upsert_card`]. Replies `{status:<rows-affected>}`.
pub async fn bankcard_save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BankcardSaveReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;

    let form = BankcardForm {
        bankname: &req.bankname,
        subbranch: &req.subbranch,
        accountname: &req.accountname,
        cardnumber: &req.cardnumber,
        province: &req.province,
        city: &req.city,
        alias: &req.alias,
    };
    let now = chrono::Local::now().timestamp();
    let rows = bankcard::upsert_card(&state.db, req.id, session.user_id, &form, now).await?;
    Ok(Json(json!({ "status": rows })).into_response())
}

#[derive(Debug, Deserialize)]
pub struct BankcardSetDefaultReq {
    pub id: i64,
    /// The target `isdefault` flag (legacy `editBankStatus`).
    pub isopen: i32,
}

/// `POST /panel/bankcard/set_default` — §10 设为默认卡. No second factor (only
/// the login session); [`bankcard::set_default`] enforces at-most-one default
/// and ownership (a foreign id affects 0 rows). Replies `{status:<rows>}`.
pub async fn bankcard_set_default(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BankcardSetDefaultReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;

    let now = chrono::Local::now().timestamp();
    let rows = bankcard::set_default(&state.db, req.id, session.user_id, req.isopen, now).await?;
    Ok(Json(json!({ "status": rows })).into_response())
}

#[derive(Debug, Deserialize)]
pub struct BankcardDeleteReq {
    pub id: i64,
}

/// `POST /panel/bankcard/delete` — §10 删除银行卡. No second factor; ownership-
/// scoped [`bankcard::delete_card`] (a foreign id deletes nothing). Replies
/// `{status:<rows>}`.
pub async fn bankcard_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BankcardDeleteReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;

    let rows = bankcard::delete_card(&state.db, req.id, session.user_id).await?;
    Ok(Json(json!({ "status": rows })).into_response())
}

/// `POST /panel/bankcard/list` — §10 银行卡列表. No second factor; returns the
/// caller's own cards (ownership-isolated by [`bankcard::list_for_user`]) in
/// the legacy panel ordering.
pub async fn bankcard_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;

    let cards = bankcard::list_for_user(&state.db, session.user_id).await?;
    let list: Vec<Value> = cards
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "bankname": c.bankname,
                "subbranch": c.subbranch,
                "accountname": c.accountname,
                "cardnumber": c.cardnumber,
                "province": c.province,
                "city": c.city,
                "alias": c.alias,
                "isdefault": c.isdefault,
                "updatetime": c.updatetime,
            })
        })
        .collect();
    Ok(Json(json!({ "status": "success", "list": list })).into_response())
}

// --- password changes (§10) -------------------------------------------------

/// Renders a [`PwdOutcome`] as the legacy `ajaxReturn` `{status, msg?}` body
/// (the `msg` key is omitted when the outcome carries none — the pay-password
/// success / reuse branches).
fn pwd_json(out: PwdOutcome) -> Value {
    match out {
        PwdOutcome::Rejected { status, msg } => match msg {
            Some(m) => json!({ "status": status, "msg": m }),
            None => json!({ "status": status }),
        },
        PwdOutcome::Success { msg } => match msg {
            Some(m) => json!({ "status": 1, "msg": m }),
            None => json!({ "status": 1 }),
        },
    }
}

/// The `editPaypassword` form (§10): the reserved SMS `code` (the unwired
/// `sms_status()` seam ignores it) and the old / new / confirm passwords.
#[derive(Debug, Deserialize)]
pub struct PayPwdReq {
    #[serde(default)]
    #[allow(dead_code)]
    pub code: String,
    pub oldpwd: String,
    pub newpwd: String,
    pub secondpwd: String,
}

/// The `editPassword` (login) form — same shape.
#[derive(Debug, Deserialize)]
pub struct LoginPwdReq {
    #[serde(default)]
    #[allow(dead_code)]
    pub code: String,
    pub oldpwd: String,
    pub newpwd: String,
    pub secondpwd: String,
}

/// `POST /panel/password/pay/edit` — §10 修改支付密码. Session- and
/// portal-gated. The only upstream factor is SMS; with the seam closed
/// (`sms_status() == false`) nothing gates the write, so it runs the
/// verify-then-write of [`password::change_pay_password`]. Replies the legacy
/// `{status, msg?}` at HTTP 200.
pub async fn pay_password_edit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<PayPwdReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    if twofactor::sms_status() {
        // SMS configured but the send / check path is unwired → the legacy
        // verify-fail reject (unreachable while `sms_status()` is false).
        return Ok(Json(json!({ "status": 0, "msg": "验证码错误" })).into_response());
    }
    let out = password::change_pay_password(
        &state.db,
        session.user_id,
        &req.oldpwd,
        &req.newpwd,
        &req.secondpwd,
    )
    .await?;
    Ok(Json(pwd_json(out)).into_response())
}

/// `POST /panel/password/login/edit` — §10 修改登录密码. Same SMS seam as
/// [`pay_password_edit`]; runs [`password::change_login_password`] (which
/// reproduces the legacy `请勿使用旧密码` new-equals-old branch). Does NOT touch
/// the single-sign-on `session_version` (legacy `editPassword` does not).
pub async fn login_password_edit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LoginPwdReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    if twofactor::sms_status() {
        return Ok(Json(json!({ "status": 0, "msg": "验证码错误" })).into_response());
    }
    let out = password::change_login_password(
        &state.db,
        session.user_id,
        &req.oldpwd,
        &req.newpwd,
        &req.secondpwd,
    )
    .await?;
    Ok(Json(pwd_json(out)).into_response())
}

// --- merchant mobile bind / change (§10) ------------------------------------

/// `bindMobile` / `bindMobileShow` send form: the (new) number to bind.
#[derive(Debug, Deserialize)]
pub struct BindSendReq {
    pub mobile: String,
}

/// `bindMobileShow` confirm form: the delivered code + the number to write.
#[derive(Debug, Deserialize)]
pub struct BindConfirmReq {
    pub code: String,
    pub mobile: String,
}

/// `editMobile` send form: the NEW number, only read on step two (the old-phone
/// step targets the member's bound number instead).
#[derive(Debug, Deserialize)]
pub struct EditSendReq {
    #[serde(default)]
    pub mobile: String,
}

/// `editMobileShow` confirm form: the delivered code + (step two) the new number.
#[derive(Debug, Deserialize)]
pub struct EditConfirmReq {
    pub code: String,
    #[serde(default)]
    pub mobile: String,
}

/// `POST /panel/mobile/bind/send` — `bindMobile` (§10): issue + dispatch a
/// `bindMobile` code to `mobile`. Session- and portal-gated; the code is never
/// surfaced to the client. Replies the legacy `{status: 1}`.
pub async fn mobile_bind_send(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BindSendReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let sms = SmsCodes::new(state.redis.clone());
    mobile::bind_send(&sms, session.user_id, &req.mobile).await?;
    Ok(Json(json!({ "status": 1 })).into_response())
}

/// `POST /panel/mobile/bind/confirm` — `bindMobileShow` (§10): verify the code,
/// then write the mobile. A wrong / expired code rejects with `{status:0,
/// msg:'验证码错误'}` (legacy returns an empty body here — made explicit, a
/// registered deviation); a success replies `{status: <rows>}`.
pub async fn mobile_bind_confirm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BindConfirmReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let sms = SmsCodes::new(state.redis.clone());
    let out =
        mobile::bind_confirm(&state.db, &sms, session.user_id, &req.code, &req.mobile).await?;
    let body = match out {
        BindOutcome::Saved { status } => json!({ "status": status }),
        BindOutcome::BadCode => json!({ "status": 0, "msg": sms::MSG_SMS_CODE_WRONG }),
    };
    Ok(Json(body).into_response())
}

/// `POST /panel/mobile/edit/send` — `editMobile` (§10): issue + dispatch the
/// old- or new-phone code according to the phase. Empty-target branches carry
/// the exact legacy messages.
pub async fn mobile_edit_send(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<EditSendReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let sms = SmsCodes::new(state.redis.clone());
    let out = mobile::edit_send(&state.db, &sms, session.user_id, &req.mobile).await?;
    let body = match out {
        SendOutcome::Sent => json!({ "status": 1 }),
        SendOutcome::EmptyOld => json!({ "status": 0, "msg": mobile::MSG_NO_BOUND_MOBILE }),
        SendOutcome::EmptyNew => json!({ "status": 0, "msg": mobile::MSG_MOBILE_EMPTY }),
    };
    Ok(Json(body).into_response())
}

/// `POST /panel/mobile/edit/confirm` — `editMobileShow` (§10): the limiter-wrapped
/// two-step machine. `Locked` / `BadCode` reject with their message; step one
/// replies `{status:1, data:'editOldMobile'}` (advance), step two writes the new
/// mobile and replies `{status:<rows>, data:'editNewMobile'}`.
pub async fn mobile_edit_confirm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<EditConfirmReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let sms = SmsCodes::new(state.redis.clone());
    let limiter = AuthLimiter::new(state.redis.clone());
    let out = mobile::edit_confirm(
        &state.db,
        &sms,
        &limiter,
        session.user_id,
        &req.code,
        &req.mobile,
    )
    .await?;
    let body = match out {
        EditOutcome::Locked { msg } => json!({ "status": 0, "msg": msg }),
        EditOutcome::BadCode => json!({ "status": 0, "msg": sms::MSG_SMS_CODE_WRONG }),
        EditOutcome::OldVerified => json!({ "status": 1, "data": "editOldMobile" }),
        EditOutcome::Saved { status } => json!({ "status": status, "data": "editNewMobile" }),
    };
    Ok(Json(body).into_response())
}

// --- merchant google-authenticator bind / unbind (§10) ----------------------

/// The `google()` bind-confirm form (§10): the 6-digit TOTP code posted after
/// scanning the pending secret.
#[derive(Debug, Deserialize)]
pub struct GoogleBindReq {
    pub code: String,
}

/// `POST /panel/google/initiate` — `google()` GET (§10): mint or reuse the
/// pending secret for an unbound merchant (the DB stays untouched until
/// confirm). Session- and portal-gated. Replies `{status:1,bound:false,secret,
/// otpauth}` or, when already bound, `{status:1,bound:true}` (the active secret
/// is not re-served).
pub async fn google_initiate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let pending = PendingSecrets::new(state.redis.clone());
    let out = google::bind_initiate(&state.db, &pending, session.user_id).await?;
    let body = match out {
        Initiate::Secret { secret, otpauth } => json!({
            "status": 1, "bound": false, "secret": secret, "otpauth": otpauth
        }),
        Initiate::AlreadyBound => json!({ "status": 1, "bound": true }),
    };
    Ok(Json(body).into_response())
}

/// `POST /panel/google/bind` — `google()` POST (§10): confirm the pending
/// secret with a valid TOTP code (the SMS branch is skipped at the `false`
/// seam), then persist only while still unbound. Every branch replies the
/// legacy `{status, msg}` at HTTP 200.
pub async fn google_bind(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<GoogleBindReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let pending = PendingSecrets::new(state.redis.clone());
    let limiter = AuthLimiter::new(state.redis.clone());
    let now = chrono::Local::now().timestamp();
    let out = google::bind_confirm(
        &state.db,
        &pending,
        &limiter,
        session.user_id,
        &req.code,
        now,
    )
    .await?;
    let body = match out {
        BindResult::Locked { msg } => json!({ "status": 0, "msg": msg }),
        BindResult::EmptyCode => json!({ "status": 0, "msg": google::MSG_CODE_EMPTY }),
        BindResult::NoPending => json!({ "status": 0, "msg": google::MSG_NO_PENDING }),
        BindResult::BadCode => json!({ "status": 0, "msg": google::MSG_CODE_WRONG }),
        BindResult::Bound { .. } => json!({ "status": 1, "msg": google::MSG_BIND_OK }),
    };
    Ok(Json(body).into_response())
}

/// `POST /panel/google/unbind` — `unbindGoogle()` POST (§10): clear the secret
/// column. The legacy guards it only behind the (unwired) SMS factor, so with a
/// live portal session the write proceeds; replies `{status:1,msg:'解绑成功'}`.
pub async fn google_unbind(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let out = google::unbind(&state.db, session.user_id).await?;
    let body = match out {
        UnbindResult::Unbound => json!({ "status": 1, "msg": google::MSG_UNBIND_OK }),
    };
    Ok(Json(body).into_response())
}

// --- merchant KYC attachment upload / certification (§8.5) -----------------

/// `POST /panel/attachment/list` — the `authorized()` page (§8.5): the acting
/// merchant's current认证 state plus their evidence rows. Session- and
/// portal-gated; replies `{status:1,data:{authorized,list:[{id,filename,path}]}}`.
pub async fn attachment_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let authorized = attachment::get_authorized(&state.db, session.user_id).await?;
    let rows = attachment::list_for_user(&state.db, session.user_id).await?;
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "filename": r.filename,
                "path": r.path,
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": { "authorized": authorized, "list": list },
    }))
    .into_response())
}

/// `POST /panel/attachment/upload` — the `upload()` write (§8.5): a
/// multipart/form-data POST whose `auth` file field is stored under the
/// uploads root and filed as an attachment row. The legacy `Upload` guard is
/// reproduced: a jpg/gif/png extension and a 2 MiB byte ceiling. Replies
/// `{status:1,data:<row id>}` (the legacy `ajaxReturn($res)`); every rejection
/// is a 400 with the caller-facing message.
pub async fn attachment_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    // Take the first `auth` field that actually carries a file name.
    let mut chosen: Option<(String, Vec<u8>)> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| PanelError::new(StatusCode::BAD_REQUEST, format!("读取上传失败: {e}")))?
    {
        if chosen.is_some() || field.name() != Some("auth") {
            continue;
        }
        let Some(fname) = field.file_name().map(str::to_string) else {
            continue;
        };
        let bytes = field
            .bytes()
            .await
            .map_err(|e| PanelError::new(StatusCode::BAD_REQUEST, format!("读取上传失败: {e}")))?;
        chosen = Some((fname, bytes.to_vec()));
    }
    let (original, bytes) = match chosen {
        Some(pair) => pair,
        None => return Err(PanelError::new(StatusCode::BAD_REQUEST, "未选择文件")),
    };
    let ext = original.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if !attachment::is_allowed_ext(ext) {
        return Err(PanelError::new(
            StatusCode::BAD_REQUEST,
            "上传文件格式不允许（仅 jpg/gif/png）",
        ));
    }
    if bytes.len() as u64 > attachment::MAX_BYTES {
        return Err(PanelError::new(
            StatusCode::BAD_REQUEST,
            "上传文件大小超出限制（最大 2MB）",
        ));
    }
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);
    let name = uniqid(micros);
    let (_stored, record) = state
        .uploads
        .store(&name, &ext.to_ascii_lowercase(), &bytes)
        .await?;
    let id = attachment::add(&state.db, session.user_id, &original, &record).await?;
    Ok(Json(json!({ "status": 1, "data": id })).into_response())
}

/// `POST /panel/certification/submit` — the `certification()` write (§8.5):
/// files the merchant's认证 request by setting `authorized = 2` (待审核). The
/// legacy applies it unconditionally (no current-state gate), so this does too.
/// Replies `{status:1,msg:'已申请认证，请等待审核！'}`.
pub async fn certification_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    attachment::submit_certification(&state.db, session.user_id).await?;
    Ok(Json(json!({
        "status": 1,
        "msg": "已申请认证，请等待审核！",
    }))
    .into_response())
}

// --- merchant login audit records (§5.4 / §10) ------------------------------

/// The `loginrecord()` page query string (§10): 1-based `page` + `rows` per
/// page (legacy `I('get.rows', 15)`); both optional, defaulted server-side.
#[derive(Debug, Deserialize)]
pub struct LoginRecordReq {
    #[serde(default)]
    pub page: Option<u64>,
    #[serde(default)]
    pub rows: Option<u64>,
}

/// The legacy page widget's default rows-per-page (`$size = 15`).
const LOGINRECORD_DEFAULT_ROWS: u64 = 15;

/// `POST /panel/loginrecord/list` — the `loginrecord()` page (§5.4 / §10): the
/// acting merchant's OWN front-console (`type = 0`) login rows, newest first,
/// paginated. Session- and portal-gated; replies
/// `{status:1,data:{total,list:[{id,loginip,loginaddress,logindatetime}]}}`.
pub async fn loginrecord_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LoginRecordReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let page = req.page.unwrap_or(1).max(1);
    let rows = req.rows.unwrap_or(LOGINRECORD_DEFAULT_ROWS).max(1);
    let total = loginrecord::count_for_user(&state.db, session.user_id).await?;
    let page_rows = loginrecord::list_page(&state.db, session.user_id, page, rows).await?;
    let list: Vec<Value> = page_rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "loginip": r.loginip,
                "loginaddress": r.loginaddress,
                "logindatetime": r.logindatetime.format("%Y-%m-%d %H:%M:%S").to_string(),
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": { "total": total, "page": page, "rows": rows, "list": list },
    }))
    .into_response())
}

// --- merchant台卡 / 收款码 (§10) --------------------------------------------

/// The `saveReceiver` form (§10): the台卡 payee line. Legacy posted an
/// arbitrary `p` array; this port edits only `receiver`.
#[derive(Debug, Deserialize)]
pub struct SaveReceiverReq {
    pub receiver: String,
}

/// `POST /panel/charges/link` — the `link()` page (§10): the merchant's收款码
/// URL (cash-page pointer, `mid = uid + 10000`). Session- and portal-gated;
/// replies `{status:1,data:{url}}`.
pub async fn charges_link(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let url = charges::charges_url(&state.cfg.site_url, session.user_id);
    Ok(Json(json!({ "status": 1, "data": { "url": url } })).into_response())
}

/// `POST /panel/charges/qrcode` — the `qrcode()` page (§10) minus the raster
/// step: reports the收款码 URL, the current台卡 payee line, and the site-
/// relative path a rendered card image would live at. QR-IMAGE rendering and
/// `downQrcode` download are a documented seam (no raster pipeline here).
/// Replies `{status:1,data:{url,receiver,qr_path}}`.
pub async fn charges_qrcode(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let url = charges::charges_url(&state.cfg.site_url, session.user_id);
    let qr_path = charges::qr_target_path(session.user_id);
    let receiver = charges::get_receiver(&state.db, session.user_id).await?;
    Ok(Json(json!({
        "status": 1,
        "data": { "url": url, "receiver": receiver, "qr_path": qr_path },
    }))
    .into_response())
}

/// `POST /panel/charges/receiver` — the `saveReceiver()` write (§10): persist
/// the台卡 payee line (narrowed to `receiver`). Replies `{status:1}` (the
/// legacy redirect target is the `qrcode` page, which the client re-fetches).
pub async fn charges_save_receiver(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SaveReceiverReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    charges::save_receiver(&state.db, session.user_id, &req.receiver).await?;
    Ok(Json(json!({ "status": 1 })).into_response())
}

// --- merchant控制台首页 aggregation (§10) -----------------------------------

/// The `gonggao()` page widget's rows-per-page (legacy `new Page($count, 5)`).
const GONGGAO_SIZE: u64 = 5;

/// The `gonggao()` page query string: 1-based `page` (optional, defaulted).
#[derive(Debug, Deserialize)]
pub struct GonggaoReq {
    #[serde(default)]
    pub page: Option<u64>,
}

/// True when the acting account is a merchant (`groupid == 4`, the legacy
/// `main` / `gonggao` branch); an agent (the only other portal holder) is
/// `false` and sees the agent-targeted notices.
fn is_merchant_group(groupid: i32) -> bool {
    Role::from_groupid(groupid) == Role::Merchant
}

/// `POST /panel/console/main` — the `main()` block of the console首页 (§10,
/// read-side): today's headline `stat` map, the latest-2 visible公告 (`gglist`),
/// and the latest-2 login rows (`loginlog`). Session- and portal-gated. Money
/// values are units (1/10000 元); `createtime` is unix seconds.
pub async fn console_main(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let is_merchant = is_merchant_group(session.groupid);
    let today = chrono::Local::now().naive_local().date();
    let stat = console::today_stats(&state.db, session.user_id, today).await?;
    let gglist = article::latest_visible(&state.db, is_merchant, 2).await?;
    let loginlog = loginrecord::list_page(&state.db, session.user_id, 1, 2).await?;
    let gg: Vec<Value> = gglist
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "title": a.title,
                "description": a.description,
                "createtime": a.createtime,
            })
        })
        .collect();
    let logins: Vec<Value> = loginlog
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "loginip": r.loginip,
                "loginaddress": r.loginaddress,
                "logindatetime": r.logindatetime.format("%Y-%m-%d %H:%M:%S").to_string(),
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": {
            "stat": {
                "todayordercount": stat.today_order_count,
                "todayorderpaidcount": stat.today_order_paid_count,
                "todayordernopaidcount": stat.today_order_unpaid_count,
                "todayorderactualsum": stat.today_order_actual_sum,
                "complaints_deposit": stat.complaints_deposit,
                "today_income": stat.today_income,
            },
            "gglist": gg,
            "loginlog": logins,
        },
    }))
    .into_response())
}

/// `POST /panel/console/gonggao` — the `gonggao()` page (§10): the caller's
/// visible公告, newest first, paged 5-per-page. Replies
/// `{status:1,data:{total,page,rows,list}}`.
pub async fn console_gonggao(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<GonggaoReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let is_merchant = is_merchant_group(session.groupid);
    let page = req.page.unwrap_or(1).max(1);
    let offset = (page - 1) * GONGGAO_SIZE;
    let total = article::count_visible(&state.db, is_merchant).await?;
    let rows = article::list_visible(&state.db, is_merchant, offset, GONGGAO_SIZE).await?;
    let list: Vec<Value> = rows
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "title": a.title,
                "description": a.description,
                "createtime": a.createtime,
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": { "total": total, "page": page, "rows": GONGGAO_SIZE, "list": list },
    }))
    .into_response())
}

// --- 保证金明细 (complaintsDeposit, read-side; §10) -------------------------

/// The `complaintsDeposit()` page query string (§10): the legacy form's
/// filters (`orderid`, `status`, `createtime` "start|end") plus the 1-based
/// `page` / `rows` widget. All optional; defaulted server-side.
#[derive(Debug, Deserialize)]
pub struct ComplaintsDepositReq {
    #[serde(default)]
    pub page: Option<u64>,
    #[serde(default)]
    pub rows: Option<u64>,
    /// Exact `out_trade_id` match when non-empty (legacy GET `orderid`).
    #[serde(default)]
    pub orderid: Option<String>,
    /// `""` (all) / `"0"` (待解冻) / `"1"` (已解冻).
    #[serde(default)]
    pub status: Option<String>,
    /// A `Y-m-d H:i:s|Y-m-d H:i:s` create-time range (the laydate picker).
    #[serde(default)]
    pub createtime: Option<String>,
}

/// The legacy page widget's default rows-per-page (`$size = 15`).
const DEPOSIT_DEFAULT_ROWS: u64 = 15;

/// Parses one legacy laydate `Y-m-d H:i:s` stamp to local unix seconds.
fn parse_deposit_ts(raw: &str) -> Option<i64> {
    let ndt = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let zoned = ndt
        .and_local_timezone(chrono::Local)
        .single()
        .or_else(|| ndt.and_local_timezone(chrono::Local).earliest())?;
    Some(zoned.timestamp())
}

/// Splits the legacy `createtime` into an inclusive `(start, end)` unix pair.
/// A missing / unparseable end bound falls back to `now` (the legacy
/// `strtotime($cetime) ?: time()`); an unparseable start drops the whole range.
fn parse_deposit_range(raw: Option<&str>) -> Option<(i64, i64)> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    let (a, b) = match s.split_once('|') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let start = parse_deposit_ts(a)?;
    let end = b
        .and_then(parse_deposit_ts)
        .unwrap_or_else(crate::data::now_ts);
    Some((start, end))
}

/// Renders a unix-seconds column to the legacy `Y-m-d H:i:s` wall clock.
fn fmt_deposit_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// `POST /panel/deposit/list` — the `complaintsDeposit()` page (§10, read-
/// side): the acting merchant's OWN deposit rows (newest id first, paged) with
/// the three-way amount summary. Session- and portal-gated; filters mirror the
/// legacy `$where` (an optional `orderid` / `status` / `createtime` range),
/// while `stats` ignores the list-only `orderid` / `status` legs (the legacy
/// `$map`). Money values are units (1/10000 元). Replies
/// `{status:1,data:{total,page,rows,stats,list}}`.
pub async fn complaints_deposit_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ComplaintsDepositReq>,
) -> Result<Response, PanelError> {
    let store = SessionStore::new(state.redis.clone());
    let session = require_live_session(&store, &state.db, &headers).await?;
    ensure_portal(&session)?;
    let page = req.page.unwrap_or(1).max(1);
    let rows = req.rows.unwrap_or(DEPOSIT_DEFAULT_ROWS).max(1);
    let (cstart, cend) = match parse_deposit_range(req.createtime.as_deref()) {
        Some((s, e)) => (Some(s), Some(e)),
        None => (None, None),
    };
    let filter = DepositFilter {
        out_trade_id: req
            .orderid
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        status: req
            .status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i32>().ok()),
        create_start: cstart,
        create_end: cend,
    };
    let total = deposit::count_filtered(&state.db, session.user_id, &filter).await?;
    let page_rows = deposit::list_filtered(&state.db, session.user_id, &filter, page, rows).await?;
    let stats = deposit::stats(&state.db, session.user_id, cstart, cend).await?;
    let list: Vec<Value> = page_rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "pay_orderid": r.pay_orderid,
                "out_trade_id": r.out_trade_id,
                "freeze_money": r.freeze_money,
                "status": r.status,
                "unfreeze_time": r.unfreeze_time,
                "unfreeze_time_text": fmt_deposit_ts(r.unfreeze_time),
                // The template only shows the actual unfreeze once it happens.
                "real_unfreeze_time": if r.real_unfreeze_time > 0 {
                    Value::from(r.real_unfreeze_time)
                } else {
                    Value::Null
                },
                "real_unfreeze_time_text": if r.real_unfreeze_time > 0 {
                    Value::from(fmt_deposit_ts(r.real_unfreeze_time))
                } else {
                    Value::Null
                },
                "create_at": r.create_at,
                "create_at_text": fmt_deposit_ts(r.create_at),
            })
        })
        .collect();
    Ok(Json(json!({
        "status": 1,
        "data": {
            "total": total,
            "page": page,
            "rows": rows,
            "stats": {
                "all": stats.all,
                "freezed": stats.freezed,
                "unfreezed": stats.unfreezed,
            },
            "list": list,
        },
    }))
    .into_response())
}

// --- 注册开户 (pre-login, self-service; §3) ---------------------------------

/// The `checkRegister()` form (§3): the fields the legacy register form posts.
/// `invitecode` is only enforced when the site's invite switch is on.
#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub confirmpassword: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub invitecode: String,
}

/// The legacy `ajaxReturn(['errorno'=>.., 'msg'=>..])` register envelope, with
/// `need_activate` present only on the two success branches.
fn register_json(errorno: i64, need_activate: Option<i64>, msg: Value) -> Response {
    let mut obj = json!({ "errorno": errorno, "msg": msg });
    if let Some(n) = need_activate {
        obj["need_activate"] = json!(n);
    }
    Json(obj).into_response()
}

/// `POST /panel/register` — the `checkRegister()` leg (§3): validate the form,
/// run the invite gate, create the merchant/agent, consume the code. PRE-LOGIN
/// (no session). The site switches ride static config (the wide `websiteconfig`
/// table is unmodeled — the same config seam as `site_url`). Every branch
/// replies HTTP 200 in the legacy `{errorno, need_activate?, msg}` shape; the
/// one deviation is keying every rejection `errorno` (legacy typo'd the
/// password-mismatch one as `errono`) and a non-empty username/password guard
/// the legacy `checkRegister` lacked.
pub async fn register_submit(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterReq>,
) -> Result<Response, PanelError> {
    // Harden the §12 risk #2 gap: never create a blank-credential account.
    if req.username.trim().is_empty() || req.password.is_empty() {
        return Ok(register_json(10004, None, json!("注册失败")));
    }
    let flags = register::SiteFlags {
        invitecode: state.cfg.register_invitecode,
        authorized: state.cfg.register_authorized,
        register_need_activate: state.cfg.register_need_activate,
        data_auth_key: state.cfg.data_auth_key.clone(),
    };
    let input = RegisterInput {
        username: req.username.trim(),
        password: req.password.trim(),
        confirm_password: req.confirmpassword.trim(),
        email: req.email.trim(),
        invite_code: req.invitecode.trim(),
    };
    match register::register_member(&state.db, &input, &flags).await? {
        Ok(_uid) => {
            if flags.register_need_activate {
                // The activation email is a seam (no SMTP wired, §3.4): the
                // account is filed status=0 pending activation. The `msg` is
                // narrowed from the legacy contact-info object.
                Ok(register_json(0, Some(1), json!("注册成功，请查收激活邮件")))
            } else {
                Ok(register_json(0, Some(0), json!("注册成功！")))
            }
        }
        Err(e) => Ok(register_json(e.errorno(), None, json!(e.message()))),
    }
}

// --- 邮箱激活 (pre-login, activation link; §3.4) ----------------------------

/// The activation-link form: the `activate` token from the emailed link.
#[derive(Debug, Deserialize)]
pub struct ActivateReq {
    #[serde(default)]
    pub token: String,
}

/// `POST /panel/activate` — the `Activate` link (§3.4, legacy
/// `Home/EmptyController::_empty`). PRE-LOGIN (no session). Looks the member up
/// by its `activate` token and, while pending, flips `status 0 → 1`. Deviation:
/// the legacy renders a browser success/error page from a `GET` path link; the
/// JSON panel面 exposes it as a `POST {token}` replying the `{status,msg}`
/// envelope. Both `激活成功!` and the idempotent `您已激活！` are `status:1`.
pub async fn activate_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ActivateReq>,
) -> Result<Response, PanelError> {
    let outcome = register::activate_member(&state.db, req.token.trim()).await?;
    Ok(match outcome {
        register::ActivateOutcome::Activated => ajax_ok("激活成功!"),
        register::ActivateOutcome::AlreadyActive => ajax_ok("您已激活！"),
        register::ActivateOutcome::InvalidToken => ajax_err("账号有误，激活失败！"),
    })
}

// --- 找回密码 (pre-login, EMAIL code; §5 / §10) -----------------------------

/// `sendUserCode` form: username + registered email.
#[derive(Debug, Deserialize)]
pub struct SendUserCodeReq {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
}

/// `forgetpwd` form. The legacy posts the code as `varification` (sic); both
/// that and `code` are accepted.
#[derive(Debug, Deserialize)]
pub struct ForgetPwdReq {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, alias = "varification")]
    pub code: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub confirmpassword: String,
}

/// The legacy `ajaxReturn(['status'=>1,'msg'=>…])` success envelope.
fn ajax_ok(msg: &str) -> Response {
    Json(json!({ "status": 1, "msg": msg })).into_response()
}

/// The legacy `ajaxReturn(['status'=>0,'msg'=>…])` reject envelope (still 200).
fn ajax_err(msg: &str) -> Response {
    Json(json!({ "status": 0, "msg": msg })).into_response()
}

/// `POST /panel/forgetpwd/send_code` — the `sendUserCode()` leg (§5): mail a
/// 找回密码 code to the account's email and file it. PRE-LOGIN (no session);
/// replies the legacy `{status,msg}` envelope.
pub async fn forgetpwd_send_code(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendUserCodeReq>,
) -> Result<Response, PanelError> {
    if req.username.is_empty() {
        return Ok(ajax_err("用户名不能为空"));
    }
    if req.email.is_empty() {
        return Ok(ajax_err("邮箱不能为空"));
    }
    let outcome = forgetpwd::send_user_code(
        &state.db,
        &forgetpwd::NoopEmailProvider,
        &req.username,
        &req.email,
        crate::data::now_ts(),
    )
    .await?;
    Ok(match outcome {
        forgetpwd::SendOutcome::Sent => ajax_ok("发送邮件成功"),
        forgetpwd::SendOutcome::UserNotFound => ajax_err("用户或邮箱不正确"),
        forgetpwd::SendOutcome::SendFailed => ajax_err("发送邮件失败"),
    })
}

/// `POST /panel/forgetpwd/reset` — the `forgetpwd()` leg (§5): verify the code
/// and rewrite the login password. PRE-LOGIN; replies `{status,msg}`.
pub async fn forgetpwd_reset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ForgetPwdReq>,
) -> Result<Response, PanelError> {
    if req.username.is_empty() {
        return Ok(ajax_err("用户名不能为空"));
    }
    if req.email.is_empty() {
        return Ok(ajax_err("邮箱不能为空"));
    }
    if req.code.is_empty() {
        return Ok(ajax_err("验证码不能为空"));
    }
    if req.password.is_empty() || req.confirmpassword.is_empty() {
        return Ok(ajax_err("密码不能为空"));
    }
    if req.password != req.confirmpassword {
        return Ok(ajax_err("密码输入不一致!"));
    }
    let outcome = forgetpwd::reset_password(
        &state.db,
        &req.username,
        &req.email,
        &req.code,
        &req.password,
        crate::data::now_ts(),
    )
    .await?;
    Ok(match outcome {
        forgetpwd::ResetOutcome::Success => ajax_ok("修改成功!"),
        forgetpwd::ResetOutcome::CodeInvalid => ajax_err("验证码不正确或过期"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers_with(key: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let name = HeaderName::from_bytes(key.as_bytes()).unwrap();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn bearer_reads_authorization_and_fallback_header() {
        let auth = headers_with("authorization", "Bearer  abc123 ");
        assert_eq!(bearer(&auth).as_deref(), Some("abc123"));
        let custom = headers_with("x-panel-token", "tok9");
        assert_eq!(bearer(&custom).as_deref(), Some("tok9"));
        assert!(bearer(&HeaderMap::new()).is_none());
        let empty = headers_with("authorization", "Bearer ");
        assert!(bearer(&empty).is_none(), "a blank bearer is no token");
    }

    #[test]
    fn partition_preserves_order_and_splits_ownership() {
        let ids: Vec<String> = ["A", "B", "C", "A"].iter().map(|s| s.to_string()).collect();
        let owned: HashSet<String> = ["A".to_string(), "C".to_string()].into_iter().collect();
        let (allowed, denied) = partition_owned(&ids, &owned);
        // submitted order kept, duplicates preserved on the allowed side,
        // B alone is denied.
        assert_eq!(
            allowed,
            vec!["A".to_string(), "C".to_string(), "A".to_string()]
        );
        assert_eq!(denied, vec!["B".to_string()]);
    }

    fn sess(groupid: i32) -> PanelSession {
        PanelSession {
            user_id: 1,
            groupid,
            version: "v".to_string(),
        }
    }

    #[test]
    fn only_merchant_and_agent_may_review() {
        assert!(ensure_portal(&sess(4)).is_ok());
        assert!(ensure_portal(&sess(6)).is_ok());
        // the platform (groupid 1) reviews via the back-office, not the panel.
        assert!(ensure_portal(&sess(1)).is_err());
    }

    #[test]
    fn only_an_agent_may_set_a_sub_rate() {
        // agents carry SetSubRate; a plain merchant does not.
        assert!(ensure_sub_rate(&sess(6)).is_ok());
        assert!(ensure_sub_rate(&sess(4)).is_err());
    }

    #[test]
    fn only_an_agent_may_manage_invite_codes() {
        // agents carry ManageSubMerchant; a plain merchant does not (§6.3).
        assert!(ensure_agent(&sess(5)).is_ok());
        assert!(ensure_agent(&sess(4)).is_err());
    }

    #[test]
    fn report_json_carries_summary_and_per_row_status() {
        let mut rep = ReviewBatchReport::default();
        rep.succeeded.push((
            "A".into(),
            ReviewOutcome::Approved(payout_orders::Model::default()),
        ));
        rep.failures.push(("B".into(), "代付申请不存在".into()));
        let v = report_json(&rep);
        assert_eq!(v["summary"], "成功 1 失败 1");
        assert_eq!(v["succeeded"], 1);
        assert_eq!(v["failed"], 1);
        assert_eq!(v["results"][0]["status"], "ok");
        assert_eq!(v["results"][0]["message"], "审核通过");
        assert_eq!(v["results"][1]["status"], "fail");
        assert_eq!(v["results"][1]["message"], "代付申请不存在");
    }
}
