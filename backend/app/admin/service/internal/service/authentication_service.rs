//! AuthenticationService — the full port of the reference
//! `internal/service/authentication_service.go` login chain: rate-limit
//! gate → captcha gate → tenant resolve → login policies → identifier
//! resolution → AES-decrypted bcrypt credential verify (dummy-hash
//! timing equalizer) → user/policy re-checks → authority resolution
//! (`system:access_backend`) → MFA gate → token pair + Redis whitelist
//! rows → refresh cookies.

use std::sync::Arc;

use sea_orm::sea_query::Condition;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use gen_rust::gen::services::AuthenticationServiceHandlers;
use gen_rust::proto::authentication::service::v1::{login_request, GrantType};
use gen_rust::proto::authentication::service::v1::{
    ForgotPasswordRequest, GenerateCaptchaResponse, LoginRequest, LoginResponse,
    ResetPasswordByCodeRequest, VerifyCaptchaRequest, VerifyCaptchaResponse,
};
use pbjson_types::Empty;
use rushwind_http_binding::envelope::StatusError;

use crate::state::{internal_error, status_error, AppState};
use crate::token::{new_jwt_id, SessionMeta, UserTokenPayload, CLIENT_TYPE_ADMIN};
use rushwind_authn::Authenticator as _;

use crate::data::sys_configs as configs;
use crate::data::sys_login_policies as login_policies;
use crate::data::sys_role_permissions as role_permissions;
use crate::data::sys_roles as roles;
use crate::data::sys_tenants as tenants;
use crate::data::sys_user_credentials as credentials;
use crate::data::sys_user_mfa_factors as mfa_factors;
use crate::data::sys_user_roles as user_roles;
use crate::data::sys_users as users;

pub struct AuthenticationService {
    pub state: Arc<AppState>,
}

/// The permission code every backend-capable user must hold
/// (`constants.SystemAccessBackendPermissionCode`).
const SYSTEM_ACCESS_BACKEND: &str = "system:access_backend";
const PLATFORM_ADMIN_ROLE: &str = "platform:admin";
const TENANT_ADMIN_ROLE: &str = "tenant:manager";

/// The MFA login-challenge window.
const MFA_CHALLENGE_TTL: u64 = 300;

fn invalid_password() -> StatusError {
    status_error("INVALID_PASSWORD", "invalid username or password")
}

impl AuthenticationService {
    async fn config_int(&self, key: &str, default: i64) -> i64 {
        let row = configs::Entity::find()
            .filter(configs::Column::Key.eq(key))
            .one(&self.state.db)
            .await
            .ok()
            .flatten();
        match row.and_then(|r| r.value) {
            Some(v) => v.parse().unwrap_or(default),
            None => default,
        }
    }

    /// resolveUserAuthority + authorizeAndEnrich (OneToOne relation): the
    /// user's roles → permission codes must contain
    /// `system:access_backend`; roles/admin-flags/data-scope/hidden-field
    /// claims aggregate from the valid roles.
    async fn resolve_authority(&self, payload: &mut UserTokenPayload) -> Result<(), StatusError> {
        let uid = payload.user_id;
        let _tid = payload.tenant_id;

        let role_rows = user_roles::Entity::find()
            .filter(
                Condition::all()
                    .add(user_roles::Column::UserId.eq(uid))
                    .add(user_roles::Column::Status.eq("ACTIVE")),
            )
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?;
        let role_ids: Vec<u32> = role_rows.iter().filter_map(|r| r.role_id).collect();

        let perm_rows = if role_ids.is_empty() {
            Vec::new()
        } else {
            role_permissions::Entity::find()
                .filter(role_permissions::Column::RoleId.is_in(role_ids.clone()))
                .all(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?
        };
        let perm_ids: Vec<u32> = perm_rows.iter().filter_map(|r| r.permission_id).collect();

        let codes: Vec<String> = if perm_ids.is_empty() {
            Vec::new()
        } else {
            crate::data::sys_permissions::Entity::find()
                .filter(crate::data::sys_permissions::Column::Id.is_in(perm_ids))
                .all(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?
                .into_iter()
                .map(|p| p.code)
                .collect()
        };

        if !codes.iter().any(|c| c == SYSTEM_ACCESS_BACKEND) {
            return Err(status_error("FORBIDDEN", "insufficient authority"));
        }

        let role_rows = roles::Entity::find()
            .filter(roles::Column::Id.is_in(role_ids))
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?;

        payload.roles = role_rows.iter().map(|r| r.code.clone()).collect();
        for code in &payload.roles {
            if code == PLATFORM_ADMIN_ROLE {
                payload.is_platform_admin = Some(true);
            }
            if code == TENANT_ADMIN_ROLE {
                payload.is_tenant_admin = Some(true);
            }
        }

        // dss: the roles' data-scope enum names (unique, order stable).
        let mut dss = Vec::new();
        let mut dsu_units: Vec<u32> = Vec::new();
        for role in &role_rows {
            if let Some(scope) = &role.data_scope {
                if !dss.iter().any(|s| s == scope) {
                    dss.push(scope.clone());
                }
            }
        }
        payload.data_scopes = dss;
        // dsu: the unit targets of UNIT_* scopes (union).
        let unit_roles: Vec<u32> = role_rows
            .iter()
            .filter(|r| {
                matches!(
                    r.data_scope.as_deref(),
                    Some("UNIT_ONLY") | Some("UNIT_AND_CHILD") | Some("SELECTED_UNITS")
                )
            })
            .map(|r| r.id)
            .collect();
        if !unit_roles.is_empty() {
            let units = crate::data::sys_role_org_units::Entity::find()
                .filter(crate::data::sys_role_org_units::Column::RoleId.is_in(unit_roles))
                .all(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?;
            for u in units {
                if let Some(oid) = u.org_unit_id {
                    if !dsu_units.contains(&oid) {
                        dsu_units.push(oid);
                    }
                }
            }
            payload.data_scope_unit_ids = Some(
                dsu_units
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }

        // hfs: "resource.field" hidden-field entries of the valid roles.
        let hfs = crate::data::sys_role_field_permissions::Entity::find()
            .filter(
                crate::data::sys_role_field_permissions::Column::RoleId
                    .is_in(role_rows.iter().map(|r| r.id).collect::<Vec<_>>()),
            )
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?;
        payload.hidden_fields = hfs
            .iter()
            .filter_map(|r| {
                let resource = r.resource.as_ref()?;
                let field = r.field_name.as_ref()?;
                Some(format!("{resource}.{field}"))
            })
            .collect();

        Ok(())
    }

    /// FindUsernameByIdentifier: `@` → email lookup, all-digits → mobile
    /// lookup (ambiguous → 500), miss → input unchanged.
    async fn resolve_identifier(&self, tenant_id: u32, input: &str) -> Result<String, StatusError> {
        if input.is_empty() {
            return Ok(input.to_string());
        }
        let column = if input.contains('@') {
            users::Column::Email
        } else if input.bytes().all(|b| b.is_ascii_digit()) {
            let rows = users::Entity::find()
                .filter(
                    Condition::all()
                        .add(users::Column::TenantId.eq(tenant_id))
                        .add(users::Column::Mobile.eq(input)),
                )
                .all(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?;
            if rows.len() > 1 {
                return Err(internal_error("ambiguous account identifier"));
            }
            return Ok(rows
                .first()
                .map(|u| u.username.clone())
                .unwrap_or_else(|| input.to_string()));
        } else {
            return Ok(input.to_string());
        };
        let row = users::Entity::find()
            .filter(
                Condition::all()
                    .add(users::Column::TenantId.eq(tenant_id))
                    .add(column.eq(input)),
            )
            .one(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?;
        Ok(row.map(|u| u.username).unwrap_or_else(|| input.to_string()))
    }

    /// FindUserCredential: decrypt → lookup → dummy-verify paths →
    /// bcrypt → password-age policy. Returns the matched user id.
    async fn verify_credential(
        &self,
        tenant_id: u32,
        identifier: &str,
        encrypted_password: &str,
    ) -> Result<u32, StatusError> {
        use base64::Engine as _;
        let plain =
            match base64::engine::general_purpose::STANDARD.decode(encrypted_password.trim()) {
                Ok(bytes) => match crate::crypto::decrypt_aes_cbc(&bytes) {
                    Some(text) => text,
                    None => {
                        return Err(status_error("BAD_REQUEST", "decrypt credential failed"));
                    }
                },
                Err(_) => {
                    return Err(status_error("BAD_REQUEST", "invalid credential format"));
                }
            };

        let row = credentials::Entity::find()
            .filter(
                Condition::all()
                    .add(credentials::Column::TenantId.eq(tenant_id))
                    .add(credentials::Column::IdentityType.eq("USERNAME"))
                    .add(credentials::Column::Identifier.eq(identifier)),
            )
            .one(&self.state.db)
            .await;
        let row = match row {
            Ok(Some(row)) => row,
            Ok(None) => {
                crate::crypto::dummy_verify();
                return Err(status_error("USER_NOT_FOUND", "user not found"));
            }
            Err(_) => {
                crate::crypto::dummy_verify();
                return Err(internal_error("db error"));
            }
        };
        let (cred, cred_user_id) = (row.credential.clone(), row.user_id);
        let (Some(cred_user_id), Some(status)) = (cred_user_id, row.status.clone()) else {
            crate::crypto::dummy_verify();
            return Err(status_error("USER_NOT_FOUND", "user not found"));
        };
        if status != "ENABLED" {
            crate::crypto::dummy_verify();
            return Err(status_error("USER_NOT_FOUND", "user not found"));
        }
        if !crate::crypto::verify_password(&plain, &cred) {
            return Err(status_error("INVALID_PASSWORD", "incorrect password"));
        }
        // Password-age policy (sys.password.maxAgeDays, ≤0 disables).
        let max_age = self.config_int("sys.password.maxAgeDays", 90).await;
        if max_age > 0 && row.credential_type.as_deref() == Some("PASSWORD_HASH") {
            if let Some(updated_at) = row.updated_at {
                let age = chrono::Local::now().naive_local() - updated_at;
                if age > chrono::Duration::days(max_age) {
                    return Err(status_error(
                        "BAD_REQUEST",
                        "password expired, please reset your password",
                    ));
                }
            }
        }
        Ok(cred_user_id)
    }

    /// checkLoginPolicies — black-hit blocks, whitelist presence requires
    /// a hit, per IP/TIME/DEVICE in the checker's order. Fail-open.
    async fn check_login_policies(
        &self,
        tenant_id: u32,
        user_id: u32,
        ip: &str,
        device_id: &str,
    ) -> bool {
        let rows = match login_policies::Entity::find()
            .filter(login_policies::Column::TenantId.eq(tenant_id))
            .all(&self.state.db)
            .await
        {
            Ok(rows) => rows,
            Err(_) => return false,
        };
        for method in ["IP", "TIME", "DEVICE"] {
            let mut blacks = Vec::new();
            let mut whites = Vec::new();
            for p in &rows {
                if p.method.as_deref() != Some(method) {
                    continue;
                }
                let target = p.target_id.as_deref().and_then(|t| t.parse::<u32>().ok());
                if let Some(target) = target {
                    if target != 0 && target != user_id {
                        continue;
                    }
                }
                if p.type_column.as_deref() == Some("WHITELIST") {
                    whites.push(p);
                } else {
                    blacks.push(p);
                }
            }
            let matched = |value: &str| -> bool {
                match method {
                    "IP" => crate::policy::ip_matches(ip, value),
                    "TIME" => crate::policy::time_window_matches(value),
                    _ => !device_id.is_empty() && device_id == value,
                }
            };
            for p in &blacks {
                if p.value.as_deref().map(&matched).unwrap_or(false) {
                    return true;
                }
            }
            if !whites.is_empty()
                && !whites
                    .iter()
                    .any(|p| p.value.as_deref().map(&matched).unwrap_or(false))
            {
                return true;
            }
        }
        false
    }

    /// Mint the access+refresh pair and register the Redis rows.
    async fn issue_token_pair(
        &self,
        payload: &UserTokenPayload,
    ) -> Result<(String, String), StatusError> {
        let now = chrono::Utc::now().timestamp();
        let access_exp = now + self.state.tokens.access_expires_secs;
        let refresh_exp = now + self.state.tokens.refresh_expires_secs;

        let access = self
            .state
            .jwt
            .create_identity(&rushwind_authn::AuthClaims(
                payload.to_access_claims(access_exp),
            ))
            .map_err(|_| internal_error("create access token failed"))?;
        let refresh = self
            .state
            .jwt
            .create_identity(&rushwind_authn::AuthClaims(
                payload.to_refresh_claims(refresh_exp),
            ))
            .map_err(|_| internal_error("create refresh token failed"))?;
        self.state
            .tokens
            .add_token_pair(payload.user_id, &payload.jti, &access, &refresh)
            .await
            .map_err(internal_error)?;
        Ok((access, refresh))
    }

    fn session_meta(&self, payload: &UserTokenPayload) -> SessionMeta {
        SessionMeta {
            username: payload.username.clone(),
            tenant_id: payload.tenant_id,
            ip: String::new(),
            user_agent: String::new(),
            device: payload.device_id.clone(),
            login_at: chrono::Local::now().naive_local().to_string(),
        }
    }

    async fn set_cookies(&self, ctx: &Ctx, refresh_token: &str, secure: bool) {
        let (rt, exp) = self
            .state
            .tokens
            .refresh_cookie_values(refresh_token, secure);
        ctx.add_reply_header("Set-Cookie", rt);
        ctx.add_reply_header("Set-Cookie", exp);
    }

    fn clear_cookies(&self, ctx: &Ctx, secure: bool) {
        let (rt, exp) = crate::token::TokenStore::clear_cookie_values(secure);
        ctx.add_reply_header("Set-Cookie", rt);
        ctx.add_reply_header("Set-Cookie", exp);
    }

    /// The MFA login challenge row (`mfa:login:{opId}`, 5 min).
    async fn set_mfa_challenge(
        &self,
        op_id: &str,
        payload: &UserTokenPayload,
    ) -> Result<(), String> {
        let mut conn = self.state.redis.clone();
        let body = serde_json::json!({ "payload": payload, "clientType": CLIENT_TYPE_ADMIN });
        let _: Result<(), _> = redis::AsyncCommands::set_ex(
            &mut conn,
            format!("mfa:login:{op_id}"),
            body.to_string(),
            MFA_CHALLENGE_TTL,
        )
        .await;
        Ok(())
    }

    /// Wired with the MFA login branch (storage phase).
    #[allow(dead_code)]
    async fn take_mfa_challenge(&self, op_id: &str) -> Option<UserTokenPayload> {
        let mut conn = self.state.redis.clone();
        let raw: Option<String> =
            redis::AsyncCommands::get(&mut conn, format!("mfa:login:{op_id}"))
                .await
                .ok()?;
        let value: serde_json::Value = serde_json::from_str(raw.as_deref()?).ok()?;
        serde_json::from_value(value.get("payload").cloned()?).ok()
    }

    /// Builds the token payload for an authenticated user (fresh roles,
    /// scopes and hidden fields) — shared by login and refresh.
    async fn payload_for_user(
        &self,
        user: users::Model,
        client_id: Option<String>,
        device_id: Option<String>,
    ) -> Result<UserTokenPayload, StatusError> {
        let mut payload = UserTokenPayload {
            user_id: user.id,
            tenant_id: user.tenant_id.unwrap_or(0),
            username: user.username.clone(),
            client_id,
            device_id,
            ..Default::default()
        };
        self.resolve_authority(&mut payload).await?;
        Ok(payload)
    }
}

type Ctx = rushwind_http_binding::ctx::RequestContext;

#[async_trait::async_trait]
impl AuthenticationServiceHandlers for AuthenticationService {
    async fn login(&self, ctx: Ctx, req: LoginRequest) -> Result<LoginResponse, StatusError> {
        match GrantType::try_from(req.grant_type) {
            Ok(GrantType::Password) => self.do_password(ctx, req).await,
            Ok(GrantType::RefreshToken) => Err(status_error(
                "INVALID_GRANT_TYPE",
                "use /admin/v1/refresh-token for token refresh",
            )),
            _ => Err(status_error("INVALID_GRANT_TYPE", "invalid grant type")),
        }
    }

    async fn logout(&self, ctx: Ctx, _req: Empty) -> Result<Empty, StatusError> {
        if let Some(payload) = ctx.claims.as_ref().and_then(UserTokenPayload::from_claims) {
            self.state.tokens.revoke_user_token(payload.user_id).await;
        }
        let secure = ctx
            .headers
            .get("x-forwarded-proto")
            .map(|v| v.eq_ignore_ascii_case("https"))
            .unwrap_or(false);
        self.clear_cookies(&ctx, secure);
        Ok(Empty {})
    }

    async fn forgot_password(
        &self,
        _ctx: Ctx,
        req: ForgotPasswordRequest,
    ) -> Result<Empty, StatusError> {
        let identifier = req.identifier.unwrap_or_default();
        if !identifier.is_empty() {
            // The anti-enumeration contract: unknown identifiers answer
            // success silently (authentication_forgot_password.go).
            let row = credentials::Entity::find()
                .filter(
                    Condition::all()
                        .add(credentials::Column::IdentityType.eq("EMAIL"))
                        .add(credentials::Column::Identifier.eq(identifier.clone())),
                )
                .one(&self.state.db)
                .await
                .ok()
                .flatten();
            if row.is_some() {
                let code = format!(
                    "{:06}",
                    chrono::Utc::now().timestamp_micros().rem_euclid(1_000_000)
                );
                let mut conn = self.state.redis.clone();
                let _: Result<(), _> = redis::AsyncCommands::set_ex(
                    &mut conn,
                    format!("gowind:vcode:reset_password:{identifier}"),
                    code.clone(),
                    600u64,
                )
                .await;
                // Delivery rides the SMTP notification channel; without a
                // configured relay the code stays retrievable server-side
                // (same fail mode as the reference without channels).
                eprintln!("[forgot-password] reset code for {identifier}: {code}");
            }
        }
        Ok(Empty {})
    }

    async fn reset_password_by_code(
        &self,
        ctx: Ctx,
        req: ResetPasswordByCodeRequest,
    ) -> Result<Empty, StatusError> {
        let identifier = req.identifier.unwrap_or_default();
        let code = req.code.unwrap_or_default();
        if identifier.is_empty() || code.is_empty() {
            return Err(status_error("BAD_REQUEST", "invalid identifier or code"));
        }
        let key = format!("gowind:vcode:reset_password:{identifier}");
        let mut conn = self.state.redis.clone();
        let stored: Option<String> = redis::AsyncCommands::get(&mut conn, &key)
            .await
            .unwrap_or(None);
        match stored {
            Some(stored) if stored == code => {
                let _: Result<i64, _> = redis::AsyncCommands::del(&mut conn, &key).await;
            }
            _ => return Err(status_error("BAD_REQUEST", "invalid verification code")),
        }

        // The user behind the identifier (email or username).
        let user = if identifier.contains('@') {
            users::Entity::find()
                .filter(users::Column::Email.eq(identifier.clone()))
                .one(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?
        } else {
            users::Entity::find()
                .filter(
                    Condition::all()
                        .add(users::Column::TenantId.eq(0))
                        .add(users::Column::Username.eq(identifier.clone())),
                )
                .one(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?
        };
        let Some(user) = user else {
            return Ok(Empty {}); // silent like the send path
        };

        use base64::Engine as _;
        let new_plain = req
            .new_password
            .as_deref()
            .and_then(|v| {
                base64::engine::general_purpose::STANDARD
                    .decode(v.trim())
                    .ok()
            })
            .and_then(|bytes| crate::crypto::decrypt_aes_cbc(&bytes))
            .ok_or_else(|| status_error("BAD_REQUEST", "invalid credential format"))?;

        let min_len = self.config_int("sys.password.minLen", 8).await;
        if (new_plain.len() as i64) < min_len || crate::policy::char_classes(&new_plain) < 3 {
            return Err(status_error(
                "BAD_REQUEST",
                "password does not meet complexity requirements",
            ));
        }

        let cred = credentials::Entity::find()
            .filter(
                Condition::all()
                    .add(credentials::Column::TenantId.eq(user.tenant_id.unwrap_or(0)))
                    .add(credentials::Column::IdentityType.eq("USERNAME"))
                    .add(credentials::Column::Identifier.eq(user.username.clone())),
            )
            .one(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .ok_or_else(|| internal_error("credential row missing"))?;

        // History: verify against the last N hashes, then append.
        let history_count = self.config_int("sys.password.historyCount", 3).await;
        let mut history: Vec<String> = cred
            .extra_info
            .as_ref()
            .and_then(|v| v.get("password_history"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if history
            .iter()
            .any(|h| crate::crypto::verify_password(&new_plain, h))
        {
            return Err(status_error("BAD_REQUEST", "password was used recently"));
        }
        history.push(cred.credential.clone());
        let keep = history.len().saturating_sub(history_count.max(0) as usize);
        history.drain(..keep);

        let mut active: credentials::ActiveModel = cred.into();
        active.credential =
            sea_orm::Set(crate::crypto::hash_password(&new_plain).map_err(internal_error)?);
        active.updated_at = sea_orm::Set(Some(crate::data::now()));
        active.extra_info = sea_orm::Set(Some(serde_json::json!({ "password_history": history })));
        use sea_orm::ActiveModelTrait;
        active
            .update(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?;

        self.state.tokens.revoke_user_token(user.id).await;
        let _ = ctx;
        Ok(Empty {})
    }

    async fn refresh_token(
        &self,
        ctx: Ctx,
        req: LoginRequest,
    ) -> Result<LoginResponse, StatusError> {
        if GrantType::try_from(req.grant_type) != Ok(GrantType::RefreshToken) {
            return Err(status_error("INVALID_GRANT_TYPE", "invalid grant type"));
        }
        let Some(token) = ctx.cookies.get("refresh_token").cloned() else {
            return Err(status_error(
                "INCORRECT_REFRESH_TOKEN",
                "refresh token cookie is missing",
            ));
        };

        // Signature + expiry via the verification engine; then the Lua
        // verify-and-revoke makes the row single-use.
        let claims = self
            .state
            .authenticator
            .authenticate_token(&token)
            .map_err(|_| status_error("INCORRECT_REFRESH_TOKEN", "invalid refresh token"))?;
        if claims
            .get_expiration_time()
            .unwrap_or(Some(0))
            .map(|exp| exp < chrono::Utc::now().timestamp())
            .unwrap_or(false)
        {
            return Err(status_error(
                "INCORRECT_REFRESH_TOKEN",
                "refresh token is expired",
            ));
        }
        let uid = claims
            .0
            .get("uid")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .ok_or_else(|| status_error("INCORRECT_REFRESH_TOKEN", "invalid refresh token"))?;
        let jti = claims
            .get_jwt_id()
            .map_err(|_| status_error("INCORRECT_REFRESH_TOKEN", "invalid refresh token"))?;
        if !self
            .state
            .tokens
            .verify_and_revoke_refresh_token(uid, &jti, &token)
            .await
        {
            return Err(status_error(
                "INCORRECT_REFRESH_TOKEN",
                "invalid refresh token",
            ));
        }

        // Fresh authority resolution from the DB (roles may have changed).
        let user = users::Entity::find_by_id(uid)
            .one(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .ok_or_else(|| status_error("INCORRECT_REFRESH_TOKEN", "invalid refresh token"))?;
        let payload = self
            .payload_for_user(user, req.client_id.clone(), req.device_id.clone())
            .await?;
        let payload = UserTokenPayload {
            jti: new_jwt_id(),
            ..payload
        };
        let (access, refresh) = self.issue_token_pair(&payload).await?;

        let meta = SessionMeta {
            ip: ctx.ip.clone(),
            user_agent: ctx.user_agent.clone(),
            ..self.session_meta(&payload)
        };
        let _ = self
            .state
            .tokens
            .set_session_meta(payload.user_id, &payload.jti, &meta)
            .await;

        let secure = ctx
            .headers
            .get("x-forwarded-proto")
            .map(|v| v.eq_ignore_ascii_case("https"))
            .unwrap_or(false);
        self.set_cookies(&ctx, &refresh, secure).await;

        Ok(LoginResponse {
            token_type: 0, // bearer
            access_token: access,
            expires_in: self.state.tokens.access_expires_secs,
            refresh_token: None,
            scope: None,
            refresh_expires_in: Some(self.state.tokens.refresh_expires_secs),
            id_token: None,
            mfa_operation_id: None,
        })
    }

    async fn generate_captcha(
        &self,
        _ctx: Ctx,
        _req: Empty,
    ) -> Result<GenerateCaptchaResponse, StatusError> {
        let (id, image, _answer) = crate::captcha::generate(&self.state.redis)
            .await
            .map_err(internal_error)?;
        Ok(GenerateCaptchaResponse {
            captcha_id: id,
            image_base64: image,
        })
    }

    async fn verify_captcha(
        &self,
        _ctx: Ctx,
        req: VerifyCaptchaRequest,
    ) -> Result<VerifyCaptchaResponse, StatusError> {
        let valid =
            crate::captcha::verify(&self.state.redis, &req.captcha_id, &req.user_input).await;
        Ok(VerifyCaptchaResponse { valid })
    }
}

impl AuthenticationService {
    async fn do_password(&self, ctx: Ctx, req: LoginRequest) -> Result<LoginResponse, StatusError> {
        let client_ip = ctx.ip.clone();
        // The reference reads GetUsername() — only the Username oneof
        // variant carries; email/mobile identifiers answer the uniform
        // failure path.
        let raw_identifier = match &req.identifier {
            Some(login_request::Identifier::Username(name)) => name.clone(),
            _ => String::new(),
        };
        let username: String = raw_identifier.replace(['\r', '\n'], "");

        // Gate 1: the rate limiter pre-check.
        if crate::ratelimit::is_locked(&self.state.redis, &client_ip, &username).await {
            return Err(status_error(
                "BAD_REQUEST",
                "too many login failures, please try again later",
            ));
        }

        // Gate 2: the mandatory captcha (headers X-Captcha-Id/X-Captcha-Value).
        let captcha_ok = {
            let id = ctx.headers.get("x-captcha-id").cloned().unwrap_or_default();
            let value = ctx
                .headers
                .get("x-captcha-value")
                .cloned()
                .unwrap_or_default();
            crate::captcha::verify(&self.state.redis, &id, &value).await
        };
        if !captcha_ok {
            return Err(status_error("BAD_REQUEST", "invalid or missing captcha"));
        }

        // Tenant resolution: empty code = platform (0).
        let mut tenant_id: u32 = 0;
        if let Some(code) = req
            .tenant_code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            let tenant = tenants::Entity::find()
                .filter(tenants::Column::Code.eq(code))
                .one(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?;
            match tenant {
                Some(t) if t.status.as_deref() == Some("ON") => tenant_id = t.id,
                _ => return Err(status_error("BAD_REQUEST", "invalid tenant")),
            }
        }

        // Login-policy gate (global pass, user pass after credential).
        if self
            .check_login_policies(
                tenant_id,
                0,
                &client_ip,
                req.device_id.as_deref().unwrap_or(""),
            )
            .await
        {
            return Err(status_error(
                "FORBIDDEN",
                "login blocked by security policy",
            ));
        }

        // Identifier resolution (email / mobile / username).
        let username = self.resolve_identifier(tenant_id, &username).await?;

        // Credential verify; failures feed the rate limiter then normalize.
        let matched_user_id = match self
            .verify_credential(
                tenant_id,
                &username,
                &req.password.clone().unwrap_or_default(),
            )
            .await
        {
            Ok(id) => id,
            Err(err) => {
                crate::ratelimit::check_and_incr(&self.state.redis, &client_ip, &username).await;
                let reason = err.reason;
                if matches!(
                    reason,
                    "USER_NOT_FOUND" | "USER_FREEZE" | "INVALID_PASSWORD"
                ) {
                    return Err(invalid_password());
                }
                return Err(err);
            }
        };

        let user = users::Entity::find_by_id(matched_user_id)
            .one(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .ok_or_else(|| status_error("USER_NOT_FOUND", "user not found"))?;

        if user.tenant_id.unwrap_or(0) != tenant_id {
            return Err(status_error("BAD_REQUEST", "invalid tenant"));
        }

        // User-targeted policy pass.
        if self
            .check_login_policies(
                tenant_id,
                user.id,
                &client_ip,
                req.device_id.as_deref().unwrap_or(""),
            )
            .await
        {
            return Err(status_error(
                "FORBIDDEN",
                "login blocked by security policy",
            ));
        }

        let payload = self
            .payload_for_user(user.clone(), req.client_id.clone(), req.device_id.clone())
            .await?;

        // MFA gate: enabled TOTP factor → challenge instead of tokens.
        let has_totp = mfa_factors::Entity::find()
            .filter(
                Condition::all()
                    .add(mfa_factors::Column::TenantId.eq(payload.tenant_id))
                    .add(mfa_factors::Column::UserId.eq(payload.user_id))
                    .add(mfa_factors::Column::Method.eq("TOTP"))
                    .add(mfa_factors::Column::Status.eq("ENABLED")),
            )
            .one(&self.state.db)
            .await
            .map_err(|_| internal_error("mfa check failed"))?
            .is_some();
        if has_totp {
            let op_id = new_jwt_id();
            self.set_mfa_challenge(&op_id, &payload)
                .await
                .map_err(|_| internal_error("mfa challenge failed"))?;
            return Ok(LoginResponse {
                token_type: 0,
                access_token: String::new(),
                expires_in: 0,
                refresh_token: None,
                scope: None,
                refresh_expires_in: None,
                id_token: None,
                mfa_operation_id: Some(op_id),
            });
        }

        let payload = UserTokenPayload {
            jti: new_jwt_id(),
            ..payload
        };
        let (access, refresh) = self.issue_token_pair(&payload).await?;

        // Session meta + last-login bookkeeping (non-blocking).
        let meta = SessionMeta {
            ip: ctx.ip.clone(),
            user_agent: ctx.user_agent.clone(),
            ..self.session_meta(&payload)
        };
        let _ = self
            .state
            .tokens
            .set_session_meta(payload.user_id, &payload.jti, &meta)
            .await;
        {
            use sea_orm::ActiveModelTrait;
            use sea_orm::Set;
            let mut active: users::ActiveModel = user.into();
            active.last_login_at = Set(Some(crate::data::now()));
            active.last_login_ip = Set(Some(client_ip.clone()));
            let _ = active.update(&self.state.db).await;
        }
        crate::ratelimit::reset(&self.state.redis, &client_ip, &username).await;

        let secure = ctx
            .headers
            .get("x-forwarded-proto")
            .map(|v| v.eq_ignore_ascii_case("https"))
            .unwrap_or(false);
        self.set_cookies(&ctx, &refresh, secure).await;

        Ok(LoginResponse {
            token_type: 0,
            access_token: access,
            expires_in: self.state.tokens.access_expires_secs,
            refresh_token: None,
            scope: None,
            refresh_expires_in: None,
            id_token: None,
            mfa_operation_id: None,
        })
    }
}
