//! LoginAuditLogService — List/Get over `sys_login_audit_logs`
//! .

use std::sync::Arc;

use sea_orm::EntityTrait;

use crate::state::{db_err, not_found, AppState, StatusError};
use gen_rust::proto::audit::service::v1::{
    GetLoginAuditLogRequest, ListLoginAuditLogResponse, LoginAuditLog,
};
use gen_rust::proto::pagination::PagingRequest;

fn log_proto(r: crate::data::audit::sys_login_audit_logs::Model) -> LoginAuditLog {
    LoginAuditLog {
        id: Some(r.id),
        tenant_id: r.tenant_id,
        tenant_name: None,
        user_id: r.user_id,
        username: r.username,
        ip_address: r.ip_address,
        geo_location: None,
        session_id: r.session_id,
        device_info: None,
        request_id: r.request_id,
        trace_id: r.trace_id,
        action_type: r.action_type.as_deref().map(|s| match s {
            "LOGOUT" => 1,
            "SESSION_EXPIRED" => 2,
            "KICKED_OUT" => 3,
            "PASSWORD_RESET" => 4,
            _ => 0,
        }),
        status: r.status.as_deref().map(|s| match s {
            "FAILED" => 1,
            "PARTIAL" => 2,
            "LOCKED" => 3,
            _ => 0,
        }),
        failure_reason: r.failure_reason,
        mfa_status: r.mfa_status,
        login_method: r.login_method.as_deref().map(|s| match s {
            "SMS_CODE" => 1,
            "QR_CODE" => 2,
            "OIDC_SOCIAL" => 3,
            "BIOMETRIC" => 4,
            "FIDO2" => 5,
            _ => 0,
        }),
        risk_score: r.risk_score,
        risk_level: r.risk_level.as_deref().map(|s| match s {
            "MEDIUM" => 1,
            "HIGH" => 2,
            _ => 0,
        }),
        risk_factors: r
            .risk_factors
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        log_hash: r.log_hash,
        signature: r.signature,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct LoginAuditLogService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl gen_rust::gen::services::LoginAuditLogServiceHandlers for LoginAuditLogService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListLoginAuditLogResponse, StatusError> {
        let repo = crate::data::repos::AuditRepo::new(&self.state.db);
        let (rows, total) = repo.paged_login(&req).await?;
        Ok(ListLoginAuditLogResponse {
            items: rows.into_iter().map(log_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetLoginAuditLogRequest,
    ) -> Result<LoginAuditLog, StatusError> {
        let Some(gen_rust::proto::audit::service::v1::get_login_audit_log_request::QueryBy::Id(
            id,
        )) = req.query_by
        else {
            return Err(crate::state::status_error(
                "BAD_REQUEST",
                "query_by required",
            ));
        };
        let row = crate::data::audit::sys_login_audit_logs::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("login audit log"))?;
        Ok(log_proto(row))
    }
}
