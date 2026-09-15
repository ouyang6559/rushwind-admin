//! LanguageService — the port of the reference internal/service/language_service.go
//! (one file per service, mirroring the reference layout).

use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
};

use crate::state::{db_err, not_found, operator_of, status_error, AppState, StatusError};
use admin_api::proto::dict::service::v1::{
    BatchCreateLanguagesRequest, CreateLanguageRequest, DeleteLanguageRequest, Language,
    ListLanguageResponse, UpdateLanguageRequest,
};
use admin_api::proto::pagination::PagingRequest;
use pbjson_types::Empty;

fn language_proto(r: crate::data::sys_languages::Model) -> Language {
    Language {
        id: Some(r.id),
        language_code: Some(r.language_code),
        language_name: Some(r.language_name),
        native_name: r.native_name,
        is_default: r.is_default,
        is_enabled: r.is_enabled,
        sort_order: r.sort_order,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct LanguageService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl admin_api::gen::services::LanguageServiceHandlers for LanguageService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListLanguageResponse, StatusError> {
        let base = crate::data::sys_languages::Entity::find()
            .order_by_asc(crate::data::sys_languages::Column::SortOrder);
        let (paged, paging) = crate::paging::apply(base, &req);
        let rows = paged.all(&self.state.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            crate::data::sys_languages::Entity::find()
                .count(&self.state.db)
                .await
                .unwrap_or(0)
        };
        Ok(ListLanguageResponse {
            items: rows.into_iter().map(language_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: admin_api::proto::dict::service::v1::GetLanguageRequest,
    ) -> Result<Language, StatusError> {
        let id = match req.query_by {
            Some(admin_api::proto::dict::service::v1::get_language_request::QueryBy::Id(id)) => id,
            Some(admin_api::proto::dict::service::v1::get_language_request::QueryBy::Code(
                code,
            )) => crate::data::sys_languages::Entity::find()
                .filter(crate::data::sys_languages::Column::LanguageCode.eq(code))
                .one(&self.state.db)
                .await
                .map_err(db_err)?
                .map(|r| r.id)
                .ok_or_else(|| not_found("language"))?,
            None => return Err(status_error("BAD_REQUEST", "query_by required")),
        };
        let row = crate::data::sys_languages::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("language"))?;
        Ok(language_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateLanguageRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        crate::data::sys_languages::ActiveModel {
            language_code: Set(data.language_code.unwrap_or_default()),
            language_name: Set(data.language_name.unwrap_or_default()),
            native_name: Set(data.native_name),
            is_default: Set(data.is_default.or(Some(false))),
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

    async fn batch_create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: BatchCreateLanguagesRequest,
    ) -> Result<Empty, StatusError> {
        for data in req.items {
            crate::data::sys_languages::ActiveModel {
                language_code: Set(data.language_code.unwrap_or_default()),
                language_name: Set(data.language_name.unwrap_or_default()),
                native_name: Set(data.native_name),
                is_default: Set(data.is_default.or(Some(false))),
                is_enabled: Set(data.is_enabled.or(Some(true))),
                sort_order: Set(data.sort_order.or(Some(0))),
                created_by: Set(operator_of(&ctx).ok().map(|p| p.user_id)),
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

    async fn update(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UpdateLanguageRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_languages::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("language"))?;
        let mut a: crate::data::sys_languages::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = data.language_name.clone() {
                a.language_name = Set(v);
            }
            if let Some(v) = data.native_name.clone() {
                a.native_name = Set(Some(v));
            }
            if let Some(v) = data.is_default {
                a.is_default = Set(Some(v));
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
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteLanguageRequest,
    ) -> Result<Empty, StatusError> {
        let Some(admin_api::proto::dict::service::v1::delete_language_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        crate::data::sys_languages::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
