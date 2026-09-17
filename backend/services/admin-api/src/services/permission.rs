//! PermissionService — //! internal/service/service: permission CRUD plus
//! SyncPermissions, the menu→permission rebuild (compose full menu
//! paths, convert to resource:action codes, CATALOG menus become
//! groups, enabled apis attach by converted path codes), then role
//! policy reload.

use std::collections::BTreeMap;
use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};

use crate::state::{db_err, not_found, operator_of, AppState, StatusError};
use pbjson_types::Empty;
use proto::proto::pagination::PagingRequest;
use proto::proto::permission::service::v1::{
    CreatePermissionRequest, DeletePermissionRequest, GetPermissionRequest, ListPermissionResponse,
    Permission, UpdatePermissionRequest,
};

async fn permission_proto(state: &AppState, r: crate::data::sys_permissions::Model) -> Permission {
    let menu_ids: Vec<u32> = crate::data::sys_permission_menus::Entity::find()
        .filter(crate::data::sys_permission_menus::Column::PermissionId.eq(r.id))
        .all(&state.db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.menu_id)
        .collect();
    let api_ids: Vec<u32> = crate::data::sys_permission_apis::Entity::find()
        .filter(crate::data::sys_permission_apis::Column::PermissionId.eq(r.id))
        .all(&state.db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.api_id)
        .collect();
    let group_name = match r.group_id {
        Some(gid) => crate::data::sys_permission_groups::Entity::find_by_id(gid)
            .one(&state.db)
            .await
            .ok()
            .flatten()
            .map(|g| g.name),
        None => None,
    };
    Permission {
        id: Some(r.id),
        name: Some(r.name),
        code: Some(r.code),
        description: r.description,
        status: r.status.as_deref().map(|s| if s == "OFF" { 0 } else { 1 }),
        group_id: r.group_id,
        group_name,
        menu_ids,
        api_ids,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

/// The naive English singularizer the converter relies on
/// (users→user, categories→category).
fn singularize(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if let Some(stem) = word.strip_suffix('s') {
        if !stem.is_empty() {
            return stem.to_string();
        }
    }
    word.to_string()
}

/// The BUTTON menu title→action keyword rule
/// (utils/converter/module:143-240).
fn button_action(title: &str) -> &'static str {
    let t = title.to_lowercase();
    for (keyword, action) in [
        ("新增", "create"),
        ("创建", "create"),
        ("添加", "create"),
        ("add", "create"),
        ("create", "create"),
        ("编辑", "edit"),
        ("修改", "edit"),
        ("edit", "edit"),
        ("update", "edit"),
        ("删除", "delete"),
        ("delete", "delete"),
        ("remove", "delete"),
        ("导入", "import"),
        ("import", "import"),
        ("导出", "export"),
        ("export", "export"),
    ] {
        if t.contains(keyword) {
            return action;
        }
    }
    "act"
}

/// Menu path → permission code (ConvertCode): drop the first segment,
/// skip `:param` segments, join the rest with `:`, append the type's
/// action suffix.
fn menu_code(path: &str, title: &str, menu_type: &str) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let rest: Vec<String> = segments
        .iter()
        .skip(1)
        .filter(|s| !s.starts_with(':'))
        .map(|s| singularize(s))
        .collect();
    let mut code = rest.join(":");
    if !code.is_empty() {
        code.push(':');
    }
    let suffix = match menu_type {
        "CATALOG" => "dir",
        "MENU" | "EMBEDDED" => "view",
        "LINK" => "jump",
        "BUTTON" => button_action(title),
        _ => "view",
    };
    format!("{code}{suffix}")
}

/// Api path+method → permission code (ConvertCodeByPath): strip the
/// version prefix, drop param segments, singularize the resource; the
/// action derives from the method.
fn api_code(method: &str, path: &str) -> String {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .skip(2) // /admin/v1
        .filter(|s| !s.starts_with('{') && !s.starts_with(':'))
        .collect();
    let resource = segments.first().map(|s| singularize(s)).unwrap_or_default();
    let action = if path.ends_with("/list") && method == "GET" {
        "view"
    } else {
        match method {
            "GET" => "view",
            "POST" => "create",
            "PUT" | "PATCH" => "edit",
            "DELETE" => "delete",
            _ => "view",
        }
    };
    if resource.is_empty() {
        return action.to_string();
    }
    format!("{resource}:{action}")
}

pub struct PermissionService {
    pub state: Arc<AppState>,
}

impl PermissionService {
    /// SyncPermissions: truncate biz permissions + groups, rebuild from
    /// ON menus (codes per menu type; CATALOG → group) and from enabled
    /// apis (code by path, grouped under UncategorizedPermissionGroup
    /// when unmatched), then link menu/api ids.
    async fn sync_from_sources(&self, operator_id: u32) -> Result<(), StatusError> {
        crate::data::sys_permissions::Entity::delete_many()
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permission_groups::Entity::delete_many()
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permission_menus::Entity::delete_many()
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permission_apis::Entity::delete_many()
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;

        // Menus in path order (parents first, so group creation works).
        let menus = crate::data::sys_menus::Entity::find()
            .filter(crate::data::sys_menus::Column::Status.eq("ON"))
            .order_by_asc(crate::data::sys_menus::Column::Id)
            .all(&self.state.db)
            .await
            .map_err(db_err)?;

        // parent chain → full path.
        let mut path_by_id: BTreeMap<u32, String> = BTreeMap::new();
        for menu in &menus {
            let parent_path = menu
                .parent_id
                .and_then(|pid| path_by_id.get(&pid).cloned())
                .unwrap_or_default();
            let full = format!("{}{}", parent_path, menu.path.clone().unwrap_or_default());
            path_by_id.insert(menu.id, full);
        }

        let mut group_ids: BTreeMap<String, u32> = BTreeMap::new();
        let mut permission_id_by_menu: BTreeMap<u32, u32> = BTreeMap::new();
        let mut next_id = 1u32;

        for menu in &menus {
            let menu_type = menu.type_column.clone().unwrap_or_default();
            let full_path = path_by_id.get(&menu.id).cloned().unwrap_or_default();
            if full_path.is_empty() {
                continue;
            }
            let code = menu_code(&full_path, &menu.name, &menu_type);

            // CATALOG menus also become groups (module = 2nd path segment).
            if menu_type == "CATALOG" {
                let module = full_path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .nth(1)
                    .unwrap_or("sys")
                    .to_string();
                let group = crate::data::sys_permission_groups::ActiveModel {
                    name: Set(menu.name.clone()),
                    module: Set(Some(module)),
                    path: Set(Some(format!("/{}/", menu.id))),
                    status: Set(Some("ON".into())),
                    created_at: Set(Some(crate::data::now())),
                    updated_at: Set(Some(crate::data::now())),
                    ..Default::default()
                }
                .insert(&self.state.db)
                .await
                .map_err(db_err)?;
                group_ids.insert(code.clone(), group.id);
            }

            let group_id = full_path
                .rsplit_once('/')
                .and_then(|(_prefix, _)| {
                    group_ids
                        .iter()
                        .find(|(code, _)| full_path.starts_with(code.as_str()))
                        .map(|(_, gid)| *gid)
                })
                .or_else(|| group_ids.values().next().copied());

            let perm = crate::data::sys_permissions::ActiveModel {
                id: Set(next_id),
                name: Set(menu.name.clone()),
                code: Set(code.clone()),
                group_id: Set(group_id),
                status: Set(Some("ON".into())),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(&self.state.db)
            .await
            .map_err(db_err)?;
            next_id += 1;
            permission_id_by_menu.insert(menu.id, perm.id);
            crate::data::sys_permission_menus::ActiveModel {
                permission_id: Set(Some(perm.id)),
                menu_id: Set(Some(menu.id)),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(&self.state.db)
            .await
            .map_err(db_err)?;
        }

        // API-derived permissions attach to matching menu permissions by
        // code prefix, else land in the uncategorized group.
        let apis = crate::data::sys_apis::Entity::find()
            .filter(crate::data::sys_apis::Column::Status.eq("ON"))
            .all(&self.state.db)
            .await
            .map_err(db_err)?;
        for api in apis {
            let (Some(method), Some(path)) = (api.method.clone(), api.path.clone()) else {
                continue;
            };
            let code = api_code(&method, &path);
            let matched = permission_id_by_menu.iter().find_map(|(menu_id, perm_id)| {
                let menu = menus.iter().find(|m| m.id == *menu_id)?;
                let menu_type = menu.type_column.clone().unwrap_or_default();
                let full_path = path_by_id.get(menu_id)?;
                let base = menu_code(full_path, &menu.name, &menu_type);
                let base = base
                    .rsplit_once(':')
                    .map(|(r, _)| r.to_string())
                    .unwrap_or(base);
                (code.starts_with(&base) || base.starts_with(code.as_str())).then_some(*perm_id)
            });
            let group_id = group_ids.values().next().copied();
            let perm = crate::data::sys_permissions::ActiveModel {
                id: Set(next_id),
                name: Set(code.clone()),
                code: Set(code.clone()),
                group_id: Set(if matched.is_some() { None } else { group_id }),
                status: Set(Some("ON".into())),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(&self.state.db)
            .await
            .map_err(db_err)?;
            next_id += 1;
            if let Some(perm_id) = matched {
                crate::data::sys_permission_apis::ActiveModel {
                    permission_id: Set(Some(perm_id)),
                    api_id: Set(Some(api.id)),
                    created_at: Set(Some(crate::data::now())),
                    updated_at: Set(Some(crate::data::now())),
                    ..Default::default()
                }
                .insert(&self.state.db)
                .await
                .map_err(db_err)?;
            } else {
                crate::data::sys_permission_apis::ActiveModel {
                    permission_id: Set(Some(perm.id)),
                    api_id: Set(Some(api.id)),
                    created_at: Set(Some(crate::data::now())),
                    updated_at: Set(Some(crate::data::now())),
                    ..Default::default()
                }
                .insert(&self.state.db)
                .await
                .map_err(db_err)?;
            }
        }
        let _ = operator_id;
        Ok(())
    }
}

#[async_trait::async_trait]
impl proto::gen::services::PermissionServiceHandlers for PermissionService {
    async fn list(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListPermissionResponse, StatusError> {
        let repo =
            crate::data::repos::PermissionRepo::new(&self.state.db, crate::data::Viewer::system());
        let (rows, total) = repo.paged_list(&req).await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in rows {
            items.push(permission_proto(&self.state, r).await);
        }
        Ok(ListPermissionResponse { items, total })
    }

    async fn get(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetPermissionRequest,
    ) -> Result<Permission, StatusError> {
        let _ = &ctx;
        let id = crate::query_by_id!(
            req.query_by,
            proto::proto::permission::service::v1::get_permission_request::QueryBy
        );
        let row = crate::data::sys_permissions::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("permission"))?;
        Ok(permission_proto(&self.state, row).await)
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreatePermissionRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = crate::state::require_data(req.data)?;
        let perm = crate::data::sys_permissions::ActiveModel {
            name: Set(data.name.clone().unwrap_or_default()),
            code: Set(data.code.clone().unwrap_or_default()),
            description: Set(data.description.clone()),
            group_id: Set(data.group_id),
            status: Set(Some("ON".into())),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .map_err(db_err)?;
        for menu_id in &data.menu_ids {
            crate::data::sys_permission_menus::ActiveModel {
                permission_id: Set(Some(perm.id)),
                menu_id: Set(Some(*menu_id)),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(&self.state.db)
            .await
            .map_err(db_err)?;
        }
        for api_id in &data.api_ids {
            crate::data::sys_permission_apis::ActiveModel {
                permission_id: Set(Some(perm.id)),
                api_id: Set(Some(*api_id)),
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
        req: UpdatePermissionRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_permissions::Entity::find_by_id(req.id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("permission"))?;
        let mut a: crate::data::sys_permissions::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.name {
                a.name = Set(v.clone());
            }
            if let Some(v) = &data.code {
                a.code = Set(v.clone());
            }
            if let Some(v) = &data.description {
                a.description = Set(Some(v.clone()));
            }
            if let Some(v) = data.group_id {
                a.group_id = Set(Some(v));
            }
            if let Some(v) = data.status {
                a.status = Set(Some(if v == 0 { "OFF".into() } else { "ON".into() }));
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        if let Some(data) = &req.data {
            if !data.menu_ids.is_empty() {
                crate::data::sys_permission_menus::Entity::delete_many()
                    .filter(crate::data::sys_permission_menus::Column::PermissionId.eq(req.id))
                    .exec(&self.state.db)
                    .await
                    .map_err(db_err)?;
                for menu_id in &data.menu_ids {
                    crate::data::sys_permission_menus::ActiveModel {
                        permission_id: Set(Some(req.id)),
                        menu_id: Set(Some(*menu_id)),
                        created_at: Set(Some(crate::data::now())),
                        updated_at: Set(Some(crate::data::now())),
                        ..Default::default()
                    }
                    .insert(&self.state.db)
                    .await
                    .map_err(db_err)?;
                }
            }
            if !data.api_ids.is_empty() {
                crate::data::sys_permission_apis::Entity::delete_many()
                    .filter(crate::data::sys_permission_apis::Column::PermissionId.eq(req.id))
                    .exec(&self.state.db)
                    .await
                    .map_err(db_err)?;
                for api_id in &data.api_ids {
                    crate::data::sys_permission_apis::ActiveModel {
                        permission_id: Set(Some(req.id)),
                        api_id: Set(Some(*api_id)),
                        created_at: Set(Some(crate::data::now())),
                        updated_at: Set(Some(crate::data::now())),
                        ..Default::default()
                    }
                    .insert(&self.state.db)
                    .await
                    .map_err(db_err)?;
                }
            }
        }
        Ok(Empty {})
    }

    async fn delete(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeletePermissionRequest,
    ) -> Result<Empty, StatusError> {
        let _ = operator_of(&ctx)?;
        let id = crate::query_by_id!(
            req.query_by,
            proto::proto::permission::service::v1::delete_permission_request::QueryBy
        );
        crate::data::sys_role_permissions::Entity::delete_many()
            .filter(crate::data::sys_role_permissions::Column::PermissionId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permission_menus::Entity::delete_many()
            .filter(crate::data::sys_permission_menus::Column::PermissionId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permission_apis::Entity::delete_many()
            .filter(crate::data::sys_permission_apis::Column::PermissionId.eq(id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::sys_permissions::Entity::delete_by_id(id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn sync_permissions(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        _req: Empty,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        self.sync_from_sources(payload.user_id).await?;
        Ok(Empty {})
    }
}
