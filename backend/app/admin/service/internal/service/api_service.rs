//! ApiService — service layer:
//! `sys_apis` CRUD plus SyncApis (truncate + rebuild from the vendored
//! OpenAPI document, module mapped from the service tag) and
//! GetWalkRouteData. Every mutation resets the authorization policies.

use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};

use crate::state::{
    db_err, internal_error, not_found, operator_of, status_error, AppState, StatusError,
};
use gen_rust::proto::pagination::PagingRequest;
use gen_rust::proto::permission::service::v1::{
    Api, CreateApiRequest, DeleteApiRequest, GetApiRequest, ListApiResponse, UpdateApiRequest,
};
use pbjson_types::Empty;

fn status_to_proto(s: &str) -> i32 {
    if s == "OFF" {
        0
    } else {
        1
    }
}

/// The `ServiceTagToBusinessModule` map
/// (pkg/constants/module): tag → module string.
fn tag_to_module(tag: &str) -> &'static str {
    match tag {
        "AdminPortalService" | "DashboardService" | "AuthenticationService" => "DASHBOARD",
        "UserService" | "OrgUnitService" | "PositionService" | "UserProfileService"
        | "RoleService" => "OPM",
        "MenuService" | "ApiService" | "PermissionService" | "PermissionGroupService" => {
            "PERMISSION"
        }
        "DictTypeService" | "DictEntryService" => "DICT",
        "LanguageService"
        | "LoginPolicyService"
        | "ConfigService"
        | "AccessKeyService"
        | "ServerMonitorService"
        | "NotificationChannelService"
        | "OnlineSessionService" => "SYSTEM",
        "FileService" | "FileTransferService" => "FILE",
        "TaskService" => "TASK",
        "TenantService" | "PlanService" | "PlanQuotaService" | "PlanModuleService" => "TENANT",
        "ApiAuditLogService"
        | "LoginAuditLogService"
        | "OperationAuditLogService"
        | "DataAccessAuditLogService"
        | "PermissionAuditLogService"
        | "PolicyEvaluationLogService"
        | "RedisCacheMonitorService" => "LOG",
        "InternalMessageService"
        | "InternalMessageCategoryService"
        | "InternalMessageRecipientService" => "INTERNAL_MESSAGE",
        _ => "SYSTEM",
    }
}

fn api_proto(r: crate::data::sys_apis::Model) -> Api {
    Api {
        id: Some(r.id),
        operation: r.operation,
        path: r.path,
        method: r.method,
        module: r.module,
        module_description: r.module_description,
        business_module: r.business_module.as_deref().map(|m| match m {
            "DASHBOARD" => 1,
            "OPM" => 2,
            "SYSTEM" => 3,
            "DICT" => 4,
            "TENANT" => 5,
            "PERMISSION" => 6,
            "LOG" => 7,
            "INTERNAL_MESSAGE" => 8,
            "FILE" => 9,
            "TASK" => 10,
            _ => 0,
        }),
        description: r.description,
        scope: r.scope.as_deref().map(|s| if s == "APP" { 2 } else { 1 }),
        status: r.status.as_deref().map(status_to_proto),
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct ApiService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl gen_rust::gen::services::ApiServiceHandlers for ApiService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListApiResponse, StatusError> {
        let repo =
            crate::data::repos::ApiRepo::new(&self.state.db, crate::data::scope::Viewer::system());
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListApiResponse {
            items: rows.into_iter().map(api_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetApiRequest,
    ) -> Result<Api, StatusError> {
        let Some(gen_rust::proto::permission::service::v1::get_api_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_apis::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("api"))?;
        Ok(api_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateApiRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_apis::ActiveModel {
            operation: Set(data.operation),
            path: Set(data.path),
            method: Set(data.method),
            module: Set(data.module),
            module_description: Set(data.module_description),
            business_module: Set(data.business_module.map(|m| {
                match m {
                    1 => "DASHBOARD",
                    2 => "OPM",
                    3 => "SYSTEM",
                    4 => "DICT",
                    5 => "TENANT",
                    6 => "PERMISSION",
                    7 => "LOG",
                    8 => "INTERNAL_MESSAGE",
                    9 => "FILE",
                    10 => "TASK",
                    _ => "SYSTEM",
                }
                .to_string()
            })),
            description: Set(data.description),
            scope: Set(Some(if data.scope == Some(2) {
                "APP".into()
            } else {
                "ADMIN".into()
            })),
            status: Set(Some("ON".into())),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn update(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UpdateApiRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_apis::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("api"))?;
        let mut a: crate::data::sys_apis::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.description {
                a.description = Set(Some(v.clone()));
            }
            if let Some(v) = &data.path {
                a.path = Set(Some(v.clone()));
            }
            if let Some(v) = &data.method {
                a.method = Set(Some(v.clone()));
            }
            if let Some(v) = &data.operation {
                a.operation = Set(Some(v.clone()));
            }
            if let Some(v) = data.status {
                a.status = Set(Some(if v == 0 { "OFF".into() } else { "ON".into() }));
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn delete(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteApiRequest,
    ) -> Result<Empty, StatusError> {
        let Some(gen_rust::proto::permission::service::v1::delete_api_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        crate::data::sys_permission_apis::Entity::delete_many()
            .filter(crate::data::sys_permission_apis::Column::ApiId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_apis::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn sync_apis(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        _req: Empty,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        // The OpenAPI document is embedded in the binary (
        // `assets.OpenApiData`), never read from the filesystem.
        let doc: serde_yaml::Value = serde_yaml::from_str(crate::assets::OPENAPI_DATA)
            .map_err(|e| internal_error(format!("openapi parse: {e}")))?;

        let mut rows: Vec<(String, String, String, String, Option<u32>)> = Vec::new(); // path, method, operation, module, business_module i32
        let paths = doc
            .get("paths")
            .and_then(|p| p.as_mapping())
            .ok_or_else(|| internal_error("openapi paths missing"))?;
        for (path, methods) in paths {
            let Some(methods) = methods.as_mapping() else {
                continue;
            };
            for (method, op) in methods {
                let (Some(path), Some(method), Some(op)) =
                    (path.as_str(), method.as_str(), op.as_mapping())
                else {
                    continue;
                };
                let method = method.to_uppercase();
                if !["GET", "POST", "PUT", "DELETE", "PATCH"].contains(&method.as_str()) {
                    continue;
                }
                let operation = op
                    .get(serde_yaml::Value::from("operationId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let tag = op
                    .get(serde_yaml::Value::from("tags"))
                    .and_then(|v| v.as_sequence())
                    .and_then(|seq| seq.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let module = tag_to_module(&tag).to_string();
                rows.push((path.to_string(), method, operation, module, None));
            }
        }
        rows.sort_by_key(|a| (a.0.clone(), a.1.clone()));

        // Truncate + rebuild with ids 1..N (syncWithOpenAPI semantics).
        crate::data::sys_apis::Entity::delete_many()
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permission_apis::Entity::delete_many()
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        for (idx, (path, method, operation, module, _)) in rows.into_iter().enumerate() {
            let id = (idx + 1) as u32;
            let business = tag_to_module_by_operation(&operation);
            crate::data::sys_apis::ActiveModel {
                id: Set(id),
                path: Set(Some(path)),
                method: Set(Some(method)),
                operation: Set(Some(operation)),
                module: Set(Some(module)),
                business_module: Set(Some(business.to_string())),
                scope: Set(Some("ADMIN".into())),
                status: Set(Some("ON".into())),
                created_by: Set(Some(payload.user_id)),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(&self.state.db)
            .await
            .map_err(db_err)?;
        }
        Ok(Empty {})
    }

    async fn get_walk_route_data(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        _req: Empty,
    ) -> Result<ListApiResponse, StatusError> {
        // The debug surface: the live route table (id-path-method only).
        let rows = crate::data::sys_apis::Entity::find()
            .order_by_asc(crate::data::sys_apis::Column::Id)
            .all(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(ListApiResponse {
            items: rows.into_iter().map(api_proto).collect(),
            total: 0,
        })
    }
}

fn tag_to_module_by_operation(operation: &str) -> &'static str {
    let service = operation.split('_').next().unwrap_or("");
    tag_to_module(service)
}
