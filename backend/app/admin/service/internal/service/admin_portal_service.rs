//! AdminPortalService — the reference admin_portal_service.go: the
//! post-login surface (navigation tree from the roles' menus, permission
//! codes, and the combined initial context).

use std::sync::Arc;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use gen_rust::gen::services::AdminPortalServiceHandlers;
use gen_rust::proto::admin::service::v1::{
    InitialContextResponse, ListPermissionCodeResponse, ListRouteResponse,
};
use gen_rust::proto::identity::service::v1::User;
use gen_rust::proto::permission::service::v1::MenuRouteItem;
use pbjson_types::Empty;

use crate::state::{internal_error, status_error, AppState};
use crate::token::UserTokenPayload;

pub struct AdminPortalService {
    pub state: Arc<AppState>,
}

fn operator(
    ctx: &rushwind_http_binding::ctx::RequestContext,
) -> Result<UserTokenPayload, crate::state::StatusError> {
    ctx.claims
        .as_ref()
        .and_then(UserTokenPayload::from_claims)
        .ok_or_else(|| status_error("UNAUTHORIZED", "missing identity"))
}

/// Loads the operator's user row with role codes resolved (the `/me`
/// shape).
pub async fn load_user(
    state: &AppState,
    uid: u32,
) -> Result<(crate::data::sys_users::Model, Vec<String>), crate::state::StatusError> {
    let user = crate::data::sys_users::Entity::find_by_id(uid)
        .one(&state.db)
        .await
        .map_err(|e| internal_error(format!("db: {e}")))?
        .ok_or_else(|| status_error("USER_NOT_FOUND", "user not found"))?;
    let role_ids: Vec<u32> = crate::data::sys_user_roles::Entity::find()
        .filter(crate::data::sys_user_roles::Column::UserId.eq(uid))
        .all(&state.db)
        .await
        .map_err(|e| internal_error(format!("db: {e}")))?
        .iter()
        .filter_map(|r| r.role_id)
        .collect();
    let codes = if role_ids.is_empty() {
        Vec::new()
    } else {
        crate::data::sys_roles::Entity::find()
            .filter(crate::data::sys_roles::Column::Id.is_in(role_ids))
            .all(&state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .into_iter()
            .map(|r| r.code)
            .collect()
    };
    Ok((user, codes))
}

/// User row → proto User (the identity fields; role codes ride the
/// `roles` list).
pub fn user_to_proto(user: crate::data::sys_users::Model, role_codes: Vec<String>) -> User {
    User {
        id: Some(user.id),
        tenant_id: user.tenant_id,
        username: Some(user.username),
        nickname: user.nickname,
        realname: user.realname,
        avatar: user.avatar,
        email: user.email,
        mobile: user.mobile,
        telephone: user.telephone,
        gender: user.gender.as_deref().map(|g| match g {
            "MALE" => 1,
            "FEMALE" => 2,
            _ => 0,
        }),
        address: user.address,
        region: user.region,
        description: user.description,
        last_login_at: user.last_login_at.and_then(crate::state::naive_to_ts),
        last_login_ip: user.last_login_ip,
        status: user.status.as_deref().map(|s| match s {
            "NORMAL" => 1,
            "PENDING" => 2,
            "LOCKED" => 3,
            "EXPIRED" => 4,
            "CLOSED" => 9,
            _ => 0,
        }),
        locked_until: user.locked_until.and_then(crate::state::naive_to_ts),
        created_by: user.created_by,
        updated_by: user.updated_by,
        deleted_by: user.deleted_by,
        created_at: user.created_at.and_then(crate::state::naive_to_ts),
        updated_at: user.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: user.deleted_at.and_then(crate::state::naive_to_ts),
        tenant_name: None,
        org_unit_id: None,
        org_unit_ids: Vec::new(),
        org_unit_name: None,
        org_unit_names: Vec::new(),
        position_id: None,
        position_ids: Vec::new(),
        position_name: None,
        position_names: Vec::new(),
        role_id: None,
        role_ids: Vec::new(),
        roles: role_codes,
        role_names: Vec::new(),
        remark: user.remark,
    }
}

/// entity jsonb (protojson keys) → MenuMeta proto.
pub fn menu_meta_from_json(
    value: &serde_json::Value,
) -> Option<gen_rust::proto::permission::service::v1::MenuMeta> {
    let obj = value.as_object()?;
    let str_at = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(String::from);
    let bool_at = |k: &str| obj.get(k).and_then(|v| v.as_bool());
    let i32_at = |k: &str| obj.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
    Some(gen_rust::proto::permission::service::v1::MenuMeta {
        active_icon: str_at("activeIcon"),
        active_path: str_at("activePath"),
        affix_tab: bool_at("affixTab"),
        affix_tab_order: i32_at("affixTabOrder"),
        authority: obj
            .get("authority")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        badge: str_at("badge"),
        badge_type: str_at("badgeType"),
        badge_variants: str_at("badgeVariants"),
        hide_children_in_menu: bool_at("hideChildrenInMenu"),
        hide_in_breadcrumb: bool_at("hideInBreadcrumb"),
        hide_in_menu: bool_at("hideInMenu"),
        hide_in_tab: bool_at("hideInTab"),
        icon: str_at("icon"),
        iframe_src: str_at("iframeSrc"),
        ignore_access: bool_at("ignoreAccess"),
        keep_alive: bool_at("keepAlive"),
        link: str_at("link"),
        loaded: bool_at("loaded"),
        max_num_of_open_tab: i32_at("maxNumOfOpenTab"),
        menu_visible_with_forbidden: bool_at("menuVisibleWithForbidden"),
        open_in_new_window: bool_at("openInNewWindow"),
        order: i32_at("order"),
        title: str_at("title"),
    })
}

/// MenuMeta proto → the jsonb shape (protojson camelCase keys).
pub fn menu_meta_to_json(
    meta: &gen_rust::proto::permission::service::v1::MenuMeta,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    let put_str =
        |obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, v: &Option<String>| {
            if let Some(v) = v {
                obj.insert(key.to_string(), serde_json::Value::String(v.clone()));
            }
        };
    let put_bool =
        |obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, v: Option<bool>| {
            if let Some(v) = v {
                obj.insert(key.to_string(), serde_json::Value::Bool(v));
            }
        };
    let put_i32 =
        |obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, v: Option<i32>| {
            if let Some(v) = v {
                obj.insert(key.to_string(), serde_json::json!(v));
            }
        };
    put_str(&mut obj, "activeIcon", &meta.active_icon);
    put_str(&mut obj, "activePath", &meta.active_path);
    put_bool(&mut obj, "affixTab", meta.affix_tab);
    put_i32(&mut obj, "affixTabOrder", meta.affix_tab_order);
    if !meta.authority.is_empty() {
        obj.insert("authority".into(), serde_json::json!(meta.authority));
    }
    put_str(&mut obj, "badge", &meta.badge);
    put_str(&mut obj, "badgeType", &meta.badge_type);
    put_str(&mut obj, "badgeVariants", &meta.badge_variants);
    put_bool(&mut obj, "hideChildrenInMenu", meta.hide_children_in_menu);
    put_bool(&mut obj, "hideInBreadcrumb", meta.hide_in_breadcrumb);
    put_bool(&mut obj, "hideInMenu", meta.hide_in_menu);
    put_bool(&mut obj, "hideInTab", meta.hide_in_tab);
    put_str(&mut obj, "icon", &meta.icon);
    put_str(&mut obj, "iframeSrc", &meta.iframe_src);
    put_bool(&mut obj, "ignoreAccess", meta.ignore_access);
    put_bool(&mut obj, "keepAlive", meta.keep_alive);
    put_str(&mut obj, "link", &meta.link);
    put_bool(&mut obj, "loaded", meta.loaded);
    put_i32(&mut obj, "maxNumOfOpenTab", meta.max_num_of_open_tab);
    put_bool(
        &mut obj,
        "menuVisibleWithForbidden",
        meta.menu_visible_with_forbidden,
    );
    put_bool(&mut obj, "openInNewWindow", meta.open_in_new_window);
    put_i32(&mut obj, "order", meta.order);
    put_str(&mut obj, "title", &meta.title);
    serde_json::Value::Object(obj)
}

impl AdminPortalService {
    /// The role-permitted menu ids → menu rows (non-BUTTON, status ON).
    async fn permitted_menus(
        &self,
        uid: u32,
    ) -> Result<Vec<crate::data::sys_menus::Model>, crate::state::StatusError> {
        let role_ids: Vec<u32> = crate::data::sys_user_roles::Entity::find()
            .filter(crate::data::sys_user_roles::Column::UserId.eq(uid))
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .iter()
            .filter_map(|r| r.role_id)
            .collect();
        let mut menu_ids: Vec<u32> = Vec::new();
        if !role_ids.is_empty() {
            let perm_ids: Vec<u32> = crate::data::sys_role_permissions::Entity::find()
                .filter(crate::data::sys_role_permissions::Column::RoleId.is_in(role_ids))
                .all(&self.state.db)
                .await
                .map_err(|e| internal_error(format!("db: {e}")))?
                .iter()
                .filter_map(|r| r.permission_id)
                .collect();
            if !perm_ids.is_empty() {
                menu_ids = crate::data::sys_permission_menus::Entity::find()
                    .filter(crate::data::sys_permission_menus::Column::PermissionId.is_in(perm_ids))
                    .all(&self.state.db)
                    .await
                    .map_err(|e| internal_error(format!("db: {e}")))?
                    .iter()
                    .filter_map(|r| r.menu_id)
                    .collect();
            }
        }
        let mut query = crate::data::sys_menus::Entity::find()
            .filter(crate::data::sys_menus::Column::Status.eq("ON"));
        // Platform operators see everything; tenant scope narrows to the
        // permitted set (empty set for tenant users → empty routes).
        query = if menu_ids.is_empty() {
            return Ok(Vec::new());
        } else {
            query.filter(crate::data::sys_menus::Column::Id.is_in(menu_ids))
        };
        let mut rows = query
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?;
        rows.sort_by_key(|m| m.id);
        Ok(rows)
    }

    fn build_route_tree(rows: &[crate::data::sys_menus::Model]) -> Vec<MenuRouteItem> {
        fn item(row: &crate::data::sys_menus::Model) -> MenuRouteItem {
            let meta = row.meta.as_ref().and_then(menu_meta_from_json);
            MenuRouteItem {
                path: row.path.clone(),
                redirect: row.redirect.clone(),
                alias: row.alias.clone(),
                name: Some(row.name.clone()),
                component: row.component.clone(),
                meta,
                children: Vec::new(),
            }
        }
        fn attach(
            rows: &[crate::data::sys_menus::Model],
            parent: Option<u32>,
        ) -> Vec<MenuRouteItem> {
            rows.iter()
                .filter(|r| r.parent_id == parent)
                .map(|r| {
                    let mut node = item(r);
                    node.children = attach(rows, Some(r.id));
                    node
                })
                .collect()
        }
        attach(rows, None)
    }

    async fn permission_codes(&self, uid: u32) -> Result<Vec<String>, crate::state::StatusError> {
        let role_ids: Vec<u32> = crate::data::sys_user_roles::Entity::find()
            .filter(crate::data::sys_user_roles::Column::UserId.eq(uid))
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .iter()
            .filter_map(|r| r.role_id)
            .collect();
        if role_ids.is_empty() {
            return Ok(Vec::new());
        }
        let perm_ids: Vec<u32> = crate::data::sys_role_permissions::Entity::find()
            .filter(crate::data::sys_role_permissions::Column::RoleId.is_in(role_ids))
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .iter()
            .filter_map(|r| r.permission_id)
            .collect();
        if perm_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(crate::data::sys_permissions::Entity::find()
            .filter(crate::data::sys_permissions::Column::Id.is_in(perm_ids))
            .all(&self.state.db)
            .await
            .map_err(|e| internal_error(format!("db: {e}")))?
            .into_iter()
            .map(|p| p.code)
            .collect())
    }
}

#[async_trait::async_trait]
impl AdminPortalServiceHandlers for AdminPortalService {
    async fn get_navigation(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        _req: Empty,
    ) -> Result<ListRouteResponse, crate::state::StatusError> {
        let payload = operator(&ctx)?;
        let rows = self.permitted_menus(payload.user_id).await?;
        Ok(ListRouteResponse {
            items: Self::build_route_tree(&rows),
        })
    }

    async fn get_my_permission_code(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        _req: Empty,
    ) -> Result<ListPermissionCodeResponse, crate::state::StatusError> {
        let payload = operator(&ctx)?;
        let codes = self.permission_codes(payload.user_id).await?;
        Ok(ListPermissionCodeResponse {
            codes,
            hidden_fields: payload.hidden_fields,
        })
    }

    async fn get_initial_context(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        _req: Empty,
    ) -> Result<InitialContextResponse, crate::state::StatusError> {
        let payload = operator(&ctx)?;
        let (user, role_codes) = load_user(&self.state, payload.user_id).await?;
        let rows = self.permitted_menus(payload.user_id).await?;
        let codes = self.permission_codes(payload.user_id).await?;
        let _ = user_to_proto(user, role_codes);
        Ok(InitialContextResponse {
            menus: Self::build_route_tree(&rows),
            permissions: codes,
            hidden_fields: payload.hidden_fields,
        })
    }
}
