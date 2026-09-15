//! LoginPolicyService — //! internal/service/service: tenant login-restriction
//! policies (BLACKLIST/WHITELIST × IP/MAC/REGION/TIME/DEVICE).

use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::state::{db_err, not_found, operator_of, status_error, AppState, StatusError};
use gen_rust::proto::authentication::service::v1::{
    CreateLoginPolicyRequest, DeleteLoginPolicyRequest, GetLoginPolicyRequest,
    ListLoginPolicyResponse, LoginPolicy, UpdateLoginPolicyRequest,
};
use gen_rust::proto::pagination::PagingRequest;
use pbjson_types::Empty;

fn type_to_str(v: i32) -> String {
    match v {
        1 => "BLACKLIST".into(),
        2 => "WHITELIST".into(),
        _ => "BLACKLIST".into(),
    }
}

fn type_to_proto(s: &str) -> i32 {
    match s {
        "WHITELIST" => 2,
        _ => 1,
    }
}

fn method_to_str(v: i32) -> String {
    match v {
        1 => "IP".into(),
        2 => "MAC".into(),
        3 => "REGION".into(),
        4 => "TIME".into(),
        5 => "DEVICE".into(),
        _ => "IP".into(),
    }
}

fn method_to_proto(s: &str) -> i32 {
    match s {
        "MAC" => 2,
        "REGION" => 3,
        "TIME" => 4,
        "DEVICE" => 5,
        _ => 1,
    }
}

fn policy_proto(r: crate::data::sys_login_policies::Model) -> LoginPolicy {
    LoginPolicy {
        id: Some(r.id),
        target_id: r.target_id.as_deref().and_then(|v| v.parse().ok()),
        method: r.method.as_deref().map(method_to_proto),
        value: r.value,
        reason: r.reason,
        r#type: r.type_column.as_deref().map(type_to_proto),
        tenant_id: r.tenant_id,
        tenant_name: None,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct LoginPolicyService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl gen_rust::gen::services::LoginPolicyServiceHandlers for LoginPolicyService {
    async fn list(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListLoginPolicyResponse, StatusError> {
        let repo = crate::data::repos::LoginPolicyRepo::new(
            &self.state.db,
            crate::data::scope::Viewer::from_ctx(&ctx),
        );
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListLoginPolicyResponse {
            items: rows.into_iter().map(policy_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetLoginPolicyRequest,
    ) -> Result<LoginPolicy, StatusError> {
        let Some(
            gen_rust::proto::authentication::service::v1::get_login_policy_request::QueryBy::Id(
                id,
            ),
        ) = req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_login_policies::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("login policy"))?;
        Ok(policy_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateLoginPolicyRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_login_policies::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            target_id: Set(data.target_id.map(|v| v.to_string())),
            value: Set(data.value),
            reason: Set(data.reason),
            type_column: Set(Some(
                data.r#type
                    .map(type_to_str)
                    .unwrap_or_else(|| "BLACKLIST".into()),
            )),
            method: Set(Some(
                data.method
                    .map(method_to_str)
                    .unwrap_or_else(|| "IP".into()),
            )),
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
        req: UpdateLoginPolicyRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_login_policies::Entity::find_by_id(req.id)
            .filter(crate::data::sys_login_policies::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("login policy"))?;
        let mut a: crate::data::sys_login_policies::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = data.target_id {
                a.target_id = Set(Some(v.to_string()));
            }
            if let Some(v) = &data.value {
                a.value = Set(Some(v.clone()));
            }
            if let Some(v) = &data.reason {
                a.reason = Set(Some(v.clone()));
            }
            if let Some(v) = data.r#type {
                a.type_column = Set(Some(type_to_str(v)));
            }
            if let Some(v) = data.method {
                a.method = Set(Some(method_to_str(v)));
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
        req: DeleteLoginPolicyRequest,
    ) -> Result<Empty, StatusError> {
        let Some(
            gen_rust::proto::authentication::service::v1::delete_login_policy_request::QueryBy::Id(
                id,
            ),
        ) = req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        crate::data::sys_login_policies::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
