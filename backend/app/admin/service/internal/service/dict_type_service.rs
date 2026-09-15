//! DictTypeService — the port of the reference internal/service/dict_type_service.go
//! (one file per service, mirroring the reference layout).

use std::sync::Arc;

use sea_orm::sea_query::Condition;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
};

use crate::state::{
    db_err, not_found, operator_of, status_error, tenant_of, AppState, StatusError,
};
use admin_api::proto::dict::service::v1::{
    CreateDictTypeRequest, DeleteDictTypeRequest, DictType, GetDictTypeRequest,
    ListDictTypeResponse, UpdateDictTypeRequest,
};
use admin_api::proto::pagination::PagingRequest;
use pbjson_types::Empty;

fn dict_type_proto(r: crate::data::sys_dict_types::Model) -> DictType {
    DictType {
        id: Some(r.id),
        tenant_id: r.tenant_id,
        type_code: Some(r.type_code),
        type_name: Some(r.type_name),
        is_enabled: r.is_enabled,
        sort_order: r.sort_order,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
        tenant_name: None,
    }
}

pub struct DictTypeService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl admin_api::gen::services::DictTypeServiceHandlers for DictTypeService {
    async fn list(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListDictTypeResponse, StatusError> {
        let tid = tenant_of(&ctx);
        let base = crate::data::sys_dict_types::Entity::find()
            .filter(crate::data::sys_dict_types::Column::TenantId.eq(tid))
            .order_by_asc(crate::data::sys_dict_types::Column::Id);
        let (paged, paging) = crate::paging::apply(base, &req);
        let rows = paged.all(&self.state.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            crate::data::sys_dict_types::Entity::find()
                .filter(crate::data::sys_dict_types::Column::TenantId.eq(tid))
                .count(&self.state.db)
                .await
                .unwrap_or(0)
        };
        Ok(ListDictTypeResponse {
            items: rows.into_iter().map(dict_type_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetDictTypeRequest,
    ) -> Result<DictType, StatusError> {
        let tenant = tenant_of(&ctx);
        let id = match req.query_by {
            Some(admin_api::proto::dict::service::v1::get_dict_type_request::QueryBy::Id(id)) => id,
            Some(admin_api::proto::dict::service::v1::get_dict_type_request::QueryBy::Code(
                code,
            )) => crate::data::sys_dict_types::Entity::find()
                .filter(
                    Condition::all()
                        .add(crate::data::sys_dict_types::Column::TenantId.eq(tenant))
                        .add(crate::data::sys_dict_types::Column::TypeCode.eq(code)),
                )
                .one(&self.state.db)
                .await
                .map_err(db_err)?
                .map(|r| r.id)
                .ok_or_else(|| not_found("dict type"))?,
            None => return Err(status_error("BAD_REQUEST", "query_by required")),
        };
        let row = crate::data::sys_dict_types::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("dict type"))?;
        Ok(dict_type_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateDictTypeRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_dict_types::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            type_code: Set(data.type_code.unwrap_or_default()),
            type_name: Set(data.type_name.unwrap_or_default()),
            is_enabled: Set(data.is_enabled.or(Some(true))),
            sort_order: Set(data.sort_order.or(Some(0))),
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
        req: UpdateDictTypeRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_dict_types::Entity::find_by_id(req.id)
            .filter(crate::data::sys_dict_types::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("dict type"))?;
        let mut a: crate::data::sys_dict_types::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.type_name {
                a.type_name = Set(v.clone());
            }
            if let Some(v) = data.is_enabled {
                a.is_enabled = Set(Some(v));
            }
            if let Some(v) = data.sort_order {
                a.sort_order = Set(Some(v));
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn delete(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteDictTypeRequest,
    ) -> Result<Empty, StatusError> {
        let _ = ctx;
        // Delete cascades the type's entries (the reference Delete =
        // BatchDelete(ids)).
        crate::data::sys_dict_entries::Entity::delete_many()
            .filter(crate::data::sys_dict_entries::Column::TypeId.is_in(req.ids.clone()))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_dict_types::Entity::delete_many()
            .filter(crate::data::sys_dict_types::Column::Id.is_in(req.ids))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
