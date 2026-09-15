//! OperationAuditLogService — List/Get over `sys_operation_audit_logs`
//! .

use std::sync::Arc;

use sea_orm::EntityTrait;

use crate::state::{db_err, not_found, AppState, StatusError};
use gen_rust::proto::audit::service::v1::{
    GetOperationAuditLogRequest, ListOperationAuditLogResponse, OperationAuditLog,
};
use gen_rust::proto::pagination::PagingRequest;

fn log_proto(r: crate::data::audit::sys_operation_audit_logs::Model) -> OperationAuditLog {
    OperationAuditLog {
        id: Some(r.id),
        tenant_id: r.tenant_id,
        tenant_name: None,
        user_id: r.user_id,
        username: r.username,
        resource_type: r.resource_type,
        resource_id: r.resource_id,
        action: r.action.as_deref().map(|s| match s {
            "CREATE" => 0,
            "UPDATE" => 1,
            "DELETE" => 2,
            "READ" => 3,
            "ASSIGN" => 4,
            "UNASSIGN" => 5,
            "EXPORT" => 6,
            "IMPORT" => 7,
            _ => 8, // OTHER
        }),
        before_data: r.before_data.as_ref().map(|v| v.to_string()),
        after_data: r.after_data.as_ref().map(|v| v.to_string()),
        sensitive_level: r.sensitive_level.as_deref().map(|s| match s {
            "PUBLIC" => 0,
            "INTERNAL" => 1,
            "CONFIDENTIAL" => 2,
            _ => 3,
        }),
        request_id: r.request_id,
        trace_id: r.trace_id,
        success: r.success,
        failure_reason: r.failure_reason,
        ip_address: r.ip_address,
        geo_location: None,
        log_hash: r.log_hash,
        signature: r.signature,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct OperationAuditLogService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl gen_rust::gen::services::OperationAuditLogServiceHandlers for OperationAuditLogService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListOperationAuditLogResponse, StatusError> {
        let repo = crate::data::repos::AuditRepo::new(&self.state.db);
        let (rows, total) = repo.paged_operation(&req).await?;
        Ok(ListOperationAuditLogResponse {
            items: rows.into_iter().map(log_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetOperationAuditLogRequest,
    ) -> Result<OperationAuditLog, StatusError> {
        let Some(
            gen_rust::proto::audit::service::v1::get_operation_audit_log_request::QueryBy::Id(id),
        ) = req.query_by
        else {
            return Err(crate::state::status_error(
                "BAD_REQUEST",
                "query_by required",
            ));
        };
        let row = crate::data::audit::sys_operation_audit_logs::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("operation audit log"))?;
        Ok(log_proto(row))
    }
}
