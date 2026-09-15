//! Plan / PlanModule / PlanQuota services — the ports of the reference
//! plan_service.go, plan_module_service.go and plan_quota_service.go:
//! subscription-plan CRUD with their module-whitelist and quota children.

use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::state::{db_err, not_found, operator_of, status_error, AppState, StatusError};
use admin_api::proto::identity::service::v1::{
    CreatePlanModuleRequest, CreatePlanQuotaRequest, CreatePlanRequest, DeletePlanModuleRequest,
    DeletePlanQuotaRequest, DeletePlanRequest, GetPlanModuleRequest, GetPlanRequest,
    ListPlanModuleResponse, ListPlanQuotaResponse, ListPlanResponse, Plan, PlanModule, PlanQuota,
    UpdatePlanModuleRequest, UpdatePlanQuotaRequest, UpdatePlanRequest,
};
use admin_api::proto::pagination::PagingRequest;
use pbjson_types::Empty;

fn plan_version_to_str(v: i32) -> String {
    match v {
        1 => "STANDARD".into(),
        2 => "ENTERPRISE".into(),
        _ => "FREE".into(),
    }
}

fn plan_version_to_proto(s: &str) -> i32 {
    match s {
        "STANDARD" => 1,
        "ENTERPRISE" => 2,
        _ => 0,
    }
}

fn expiry_policy_to_proto(s: &str) -> i32 {
    match s {
        "BLOCK_LOGIN" => 1,
        "FREEZE" => 2,
        _ => 0, // READONLY
    }
}

fn expiry_policy_to_str(v: i32) -> String {
    match v {
        1 => "BLOCK_LOGIN".into(),
        2 => "FREEZE".into(),
        _ => "READONLY".into(),
    }
}

fn module_to_str(v: i32) -> String {
    match v {
        1 => "DASHBOARD".into(),
        2 => "OPM".into(),
        3 => "SYSTEM".into(),
        4 => "DICT".into(),
        5 => "TENANT".into(),
        6 => "PERMISSION".into(),
        7 => "LOG".into(),
        8 => "INTERNAL_MESSAGE".into(),
        9 => "FILE".into(),
        10 => "TASK".into(),
        _ => "SYSTEM".into(),
    }
}

fn quota_type_to_str(v: i32) -> String {
    match v {
        1 => "STORAGE".into(),
        2 => "API_CALL".into(),
        _ => "USER_LIMIT".into(),
    }
}

fn plan_proto(r: crate::data::sys_plans::Model) -> Plan {
    Plan {
        id: Some(r.id),
        name: Some(r.name),
        version: r.version.as_deref().map(plan_version_to_proto),
        expiry_policy: r.expiry_policy.as_deref().map(expiry_policy_to_proto),
        data_retention_days: r.data_retention_days,
        description: r.description,
        remark: r.remark,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

fn plan_module_proto(r: crate::data::sys_plan_modules::Model) -> PlanModule {
    PlanModule {
        id: Some(r.id),
        plan_id: Some(r.plan_id),
        module: r.module.as_deref().map(|s| match s {
            "OPM" => 2,
            "SYSTEM" => 3,
            "DICT" => 4,
            "TENANT" => 5,
            "PERMISSION" => 6,
            "LOG" => 7,
            "INTERNAL_MESSAGE" => 8,
            "FILE" => 9,
            "TASK" => 10,
            "DASHBOARD" => 1,
            _ => 0,
        }),
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

fn plan_quota_proto(r: crate::data::sys_plan_quotas::Model) -> PlanQuota {
    PlanQuota {
        id: Some(r.id),
        plan_id: Some(r.plan_id),
        quota_type: r.quota_type.as_deref().map(|s| match s {
            "STORAGE" => 1,
            "API_CALL" => 2,
            _ => 0,
        }),
        quota_value: r.quota_value.map(|v| v as u64),
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct PlanService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl admin_api::gen::services::PlanServiceHandlers for PlanService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListPlanResponse, StatusError> {
        let repo =
            crate::data::repos::PlanRepo::new(&self.state.db, crate::data::scope::Viewer::system());
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListPlanResponse {
            items: rows.into_iter().map(plan_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetPlanRequest,
    ) -> Result<Plan, StatusError> {
        let Some(admin_api::proto::identity::service::v1::get_plan_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_plans::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("plan"))?;
        Ok(plan_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreatePlanRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_plans::ActiveModel {
            name: Set(data.name.unwrap_or_default()),
            version: Set(Some(plan_version_to_str(data.version.unwrap_or(0)))),
            expiry_policy: Set(Some(expiry_policy_to_str(data.expiry_policy.unwrap_or(0)))),
            data_retention_days: Set(data.data_retention_days),
            description: Set(data.description),
            remark: Set(data.remark),
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
        req: UpdatePlanRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_plans::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("plan"))?;
        let mut a: crate::data::sys_plans::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.name {
                a.name = Set(v.clone());
            }
            if let Some(v) = data.version {
                a.version = Set(Some(plan_version_to_str(v)));
            }
            if let Some(v) = data.expiry_policy {
                a.expiry_policy = Set(Some(expiry_policy_to_str(v)));
            }
            if let Some(v) = data.data_retention_days {
                a.data_retention_days = Set(Some(v));
            }
            if let Some(v) = &data.description {
                a.description = Set(Some(v.clone()));
            }
            if let Some(v) = &data.remark {
                a.remark = Set(Some(v.clone()));
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
        req: DeletePlanRequest,
    ) -> Result<Empty, StatusError> {
        let Some(admin_api::proto::identity::service::v1::delete_plan_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        // CASCADE children.
        crate::data::sys_plan_modules::Entity::delete_many()
            .filter(crate::data::sys_plan_modules::Column::PlanId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_plan_quotas::Entity::delete_many()
            .filter(crate::data::sys_plan_quotas::Column::PlanId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_plans::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}

pub struct PlanModuleService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl admin_api::gen::services::PlanModuleServiceHandlers for PlanModuleService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListPlanModuleResponse, StatusError> {
        let repo = crate::data::repos::PlanModuleRepo::new(
            &self.state.db,
            crate::data::scope::Viewer::system(),
        );
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListPlanModuleResponse {
            items: rows.into_iter().map(plan_module_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetPlanModuleRequest,
    ) -> Result<PlanModule, StatusError> {
        let Some(admin_api::proto::identity::service::v1::get_plan_module_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_plan_modules::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("plan module"))?;
        Ok(plan_module_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreatePlanModuleRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_plan_modules::ActiveModel {
            plan_id: Set(data.plan_id.unwrap_or(0)),
            module: Set(Some(module_to_str(data.module.unwrap_or(0)))),
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
        req: UpdatePlanModuleRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_plan_modules::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("plan module"))?;
        let mut a: crate::data::sys_plan_modules::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = data.module {
                a.module = Set(Some(module_to_str(v)));
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
        req: DeletePlanModuleRequest,
    ) -> Result<Empty, StatusError> {
        let Some(admin_api::proto::identity::service::v1::delete_plan_module_request::QueryBy::Id(
            id,
        )) = req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        crate::data::sys_plan_modules::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}

pub struct PlanQuotaService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl admin_api::gen::services::PlanQuotaServiceHandlers for PlanQuotaService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListPlanQuotaResponse, StatusError> {
        let repo = crate::data::repos::PlanQuotaRepo::new(
            &self.state.db,
            crate::data::scope::Viewer::system(),
        );
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListPlanQuotaResponse {
            items: rows.into_iter().map(plan_quota_proto).collect(),
            total,
        })
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreatePlanQuotaRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_plan_quotas::ActiveModel {
            plan_id: Set(data.plan_id.unwrap_or(0)),
            quota_type: Set(Some(quota_type_to_str(data.quota_type.unwrap_or(0)))),
            quota_value: Set(data.quota_value.map(|v| v as i64)),
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
        req: UpdatePlanQuotaRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_plan_quotas::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("plan quota"))?;
        let mut a: crate::data::sys_plan_quotas::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = data.quota_value {
                a.quota_value = Set(Some(v as i64));
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
        req: DeletePlanQuotaRequest,
    ) -> Result<Empty, StatusError> {
        let Some(admin_api::proto::identity::service::v1::delete_plan_quota_request::QueryBy::Id(
            id,
        )) = req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        crate::data::sys_plan_quotas::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
