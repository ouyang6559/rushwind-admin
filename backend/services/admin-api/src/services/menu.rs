//! MenuService — service layer:
//! menu CRUD over `sys_menus` plus SyncMenus (upsert the posted list).

use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::state::{db_err, not_found, operator_of, status_error, AppState, StatusError};
use pbjson_types::Empty;
use proto::proto::pagination::PagingRequest;
use proto::proto::permission::service::v1::{
    CreateMenuRequest, DeleteMenuRequest, GetMenuRequest, ListMenuResponse, Menu, SyncMenusRequest,
    UpdateMenuRequest,
};

fn status_to_proto(s: &str) -> i32 {
    if s == "OFF" {
        0
    } else {
        1
    }
}

fn type_to_proto(s: &str) -> i32 {
    match s {
        "CATALOG" => 0,
        "BUTTON" => 2,
        "EMBEDDED" => 3,
        "LINK" => 4,
        _ => 1,
    }
}

fn type_to_str(v: i32) -> String {
    match v {
        0 => "CATALOG".into(),
        2 => "BUTTON".into(),
        3 => "EMBEDDED".into(),
        4 => "LINK".into(),
        _ => "MENU".into(),
    }
}

fn module_to_proto(s: &str) -> i32 {
    match s {
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

fn menu_proto(r: crate::data::sys_menus::Model) -> Menu {
    Menu {
        id: Some(r.id),
        status: r.status.as_deref().map(status_to_proto),
        r#type: r.type_column.as_deref().map(type_to_proto),
        path: r.path,
        redirect: r.redirect,
        alias: r.alias,
        name: Some(r.name),
        component: r.component,
        meta: r
            .meta
            .as_ref()
            .and_then(crate::services::admin_portal::menu_meta_from_json),
        module: r.module.as_deref().map(module_to_proto),
        parent_id: r.parent_id,
        children: Vec::new(),
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct MenuService {
    pub state: Arc<AppState>,
}

impl MenuService {
    fn apply_fields(a: &mut crate::data::sys_menus::ActiveModel, data: &Menu) {
        if let Some(v) = &data.path {
            a.path = Set(Some(v.clone()));
        }
        if let Some(v) = &data.redirect {
            a.redirect = Set(Some(v.clone()));
        }
        if let Some(v) = &data.alias {
            a.alias = Set(Some(v.clone()));
        }
        if let Some(v) = &data.name {
            a.name = Set(v.clone());
        }
        if let Some(v) = &data.component {
            a.component = Set(Some(v.clone()));
        }
        if let Some(v) = data.r#type {
            a.type_column = Set(Some(type_to_str(v)));
        }
        if let Some(v) = data.status {
            a.status = Set(Some(if v == 0 { "OFF".into() } else { "ON".into() }));
        }
        if let Some(v) = data.module {
            a.module = Set(Some(module_to_str(v)));
        }
        if let Some(v) = data.parent_id {
            a.parent_id = Set(if v == 0 { None } else { Some(v) });
        }
        if let Some(meta) = &data.meta {
            a.meta = Set(Some(crate::services::admin_portal::menu_meta_to_json(meta)));
        }
    }
}

#[async_trait::async_trait]
impl proto::gen::services::MenuServiceHandlers for MenuService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListMenuResponse, StatusError> {
        let repo =
            crate::data::repos::MenuRepo::new(&self.state.db, crate::data::scope::Viewer::system());
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListMenuResponse {
            items: rows.into_iter().map(menu_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetMenuRequest,
    ) -> Result<Menu, StatusError> {
        let Some(proto::proto::permission::service::v1::get_menu_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_menus::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("menu"))?;
        Ok(menu_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateMenuRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        let mut a = crate::data::sys_menus::ActiveModel {
            name: Set(data.name.clone().unwrap_or_default()),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        };
        Self::apply_fields(&mut a, &data);
        if a.status.clone().unwrap().is_none() {
            a.status = Set(Some("ON".into()));
        }
        a.insert(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn update(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UpdateMenuRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_menus::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("menu"))?;
        let mut a: crate::data::sys_menus::ActiveModel = row.into();
        if let Some(data) = &req.data {
            Self::apply_fields(&mut a, data);
            a.updated_by = Set(Some(payload.user_id));
            a.updated_at = Set(Some(crate::data::now()));
            a.update(&self.state.db).await.map_err(db_err)?;
        }
        Ok(Empty {})
    }

    async fn delete(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteMenuRequest,
    ) -> Result<Empty, StatusError> {
        let Some(proto::proto::permission::service::v1::delete_menu_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        // Cascade: permission links then descendants then the row.
        let children: Vec<u32> = crate::data::sys_menus::Entity::find()
            .filter(crate::data::sys_menus::Column::ParentId.eq(id))
            .all(&self.state.db)
            .await
            .map_err(db_err)?
            .iter()
            .map(|m| m.id)
            .collect();
        if !children.is_empty() {
            crate::data::sys_menus::Entity::delete_many()
                .filter(crate::data::sys_menus::Column::Id.is_in(children))
                .exec(&self.state.db)
                .await
                .map_err(db_err)?;
        }
        crate::data::sys_permission_menus::Entity::delete_many()
            .filter(crate::data::sys_permission_menus::Column::MenuId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_menus::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn sync_menus(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: SyncMenusRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        // Upsert mode: rows with ids update; rows without ids insert.
        for data in &req.items {
            match data.id {
                Some(id) if id > 0 => {
                    if let Some(row) = crate::data::sys_menus::Entity::find_by_id(id)
                        .one(&self.state.db)
                        .await
                        .map_err(db_err)?
                    {
                        let mut a: crate::data::sys_menus::ActiveModel = row.into();
                        Self::apply_fields(&mut a, data);
                        a.updated_by = Set(Some(payload.user_id));
                        a.updated_at = Set(Some(crate::data::now()));
                        a.update(&self.state.db).await.map_err(db_err)?;
                    }
                }
                _ => {
                    let mut a = crate::data::sys_menus::ActiveModel {
                        name: Set(data.name.clone().unwrap_or_default()),
                        created_by: Set(Some(payload.user_id)),
                        created_at: Set(Some(crate::data::now())),
                        updated_at: Set(Some(crate::data::now())),
                        ..Default::default()
                    };
                    Self::apply_fields(&mut a, data);
                    if data.status.is_none() {
                        a.status = Set(Some("ON".into()));
                    }
                    a.insert(&self.state.db).await.map_err(db_err)?;
                }
            }
        }
        Ok(Empty {})
    }
}
