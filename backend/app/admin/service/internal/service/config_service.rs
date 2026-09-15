//! ConfigService — the port of the reference internal/service/config_service.go:
//! key-value platform config CRUD; built-in rows refuse deletion; the
//! value_type INVALID zero-value is skipped on writes.

use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::state::{db_err, not_found, operator_of, status_error, AppState, StatusError};
use admin_api::proto::config::service::v1::{
    Config, CreateConfigRequest, DeleteConfigRequest, GetConfigRequest, ListConfigResponse,
    UpdateConfigRequest,
};
use admin_api::proto::pagination::PagingRequest;
use pbjson_types::Empty;

fn value_type_to_str(v: i32) -> Option<String> {
    // ConfigValueType: INVALID=0 (skipped), STRING=1, BOOL=2, INT=3.
    match v {
        1 => Some("STRING".into()),
        2 => Some("BOOL".into()),
        3 => Some("INT".into()),
        _ => None,
    }
}

fn value_type_to_proto(s: &str) -> i32 {
    match s {
        "BOOL" => 2,
        "INT" => 3,
        _ => 1,
    }
}

fn config_proto(r: crate::data::sys_configs::Model) -> Config {
    Config {
        id: Some(r.id),
        name: Some(r.name),
        key: Some(r.key),
        value: r.value,
        value_type: r.value_type.as_deref().map(value_type_to_proto),
        is_built_in: r.is_built_in,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct ConfigService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl admin_api::gen::services::ConfigServiceHandlers for ConfigService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListConfigResponse, StatusError> {
        let repo = crate::data::repos::ConfigRepo::new(
            &self.state.db,
            crate::data::scope::Viewer::system(),
        );
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListConfigResponse {
            items: rows.into_iter().map(config_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetConfigRequest,
    ) -> Result<Config, StatusError> {
        let Some(admin_api::proto::config::service::v1::get_config_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_configs::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("config"))?;
        Ok(config_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateConfigRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        let value_type = data.value_type.and_then(value_type_to_str);
        crate::data::sys_configs::ActiveModel {
            name: Set(data.name.unwrap_or_default()),
            key: Set(data.key.unwrap_or_default()),
            value: Set(data.value),
            value_type: Set(value_type.or(Some("STRING".into()))),
            is_built_in: Set(data.is_built_in.or(Some(false))),
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
        req: UpdateConfigRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        let key = data.key.clone().unwrap_or_default();

        // allow_missing upsert (config_repo.go semantics).
        let existing = if req.allow_missing == Some(true) && !key.is_empty() {
            crate::data::sys_configs::Entity::find()
                .filter(crate::data::sys_configs::Column::Key.eq(key.clone()))
                .one(&self.state.db)
                .await
                .map_err(db_err)?
        } else {
            crate::data::sys_configs::Entity::find_by_id(req.id)
                .one(&self.state.db)
                .await
                .map_err(db_err)?
        };
        let value_type = data.value_type.and_then(value_type_to_str);
        match existing {
            None if req.allow_missing == Some(true) => {
                crate::data::sys_configs::ActiveModel {
                    name: Set(data.name.unwrap_or_else(|| key.clone())),
                    key: Set(key),
                    value: Set(data.value),
                    value_type: Set(value_type.or(Some("STRING".into()))),
                    is_built_in: Set(Some(false)),
                    created_by: Set(Some(payload.user_id)),
                    created_at: Set(Some(crate::data::now())),
                    updated_at: Set(Some(crate::data::now())),
                    ..Default::default()
                }
                .insert(&self.state.db)
                .await
                .map_err(db_err)?;
            }
            Some(row) => {
                let mut a: crate::data::sys_configs::ActiveModel = row.into();
                if let Some(v) = &data.name {
                    a.name = Set(v.clone());
                }
                if let Some(v) = data.value.clone() {
                    a.value = Set(Some(v));
                }
                if let Some(v) = value_type {
                    a.value_type = Set(Some(v));
                }
                a.updated_by = Set(Some(payload.user_id));
                a.updated_at = Set(Some(crate::data::now()));
                a.update(&self.state.db).await.map_err(db_err)?;
            }
            None => return Err(not_found("config")),
        }
        Ok(Empty {})
    }

    async fn delete(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteConfigRequest,
    ) -> Result<Empty, StatusError> {
        let _ = operator_of(&ctx)?;
        let Some(admin_api::proto::config::service::v1::delete_config_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_configs::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?;
        // Built-in rows refuse deletion; missing rows delete idempotently.
        let Some(row) = row else {
            return Ok(Empty {});
        };
        if row.is_built_in == Some(true) {
            return Err(status_error(
                "FORBIDDEN",
                "built-in config cannot be deleted",
            ));
        }
        crate::data::sys_configs::Entity::delete_by_id(row.id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
