//! OrgUnitService — the port of the reference
//! internal/service/org_unit_service.go: org-unit CRUD with the
//! materialized `path` maintenance (create appends the path segment;
//! reparenting rewrites the subtree's paths; deleting cascades to
//! descendants) and tree-assembly semantics (flat list ordered by
//! sort_order).

use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
};

use crate::state::{
    db_err, not_found, operator_of, status_error, tenant_of, AppState, StatusError,
};
use admin_api::proto::identity::service::v1::{
    CreateOrgUnitRequest, DeleteOrgUnitRequest, GetOrgUnitRequest, ListOrgUnitResponse, OrgUnit,
    UpdateOrgUnitRequest,
};
use admin_api::proto::pagination::PagingRequest;
use pbjson_types::Empty;

fn org_type_to_proto(s: &str) -> i32 {
    match s {
        "COMPANY" => 1,
        "DIVISION" => 2,
        "TEAM" => 3,
        "PROJECT" => 4,
        "COMMITTEE" => 5,
        "REGION" => 6,
        "OTHER" => 7,
        _ => 0, // DEPARTMENT
    }
}

fn org_type_to_str(v: i32) -> String {
    match v {
        1 => "COMPANY".into(),
        2 => "DIVISION".into(),
        3 => "TEAM".into(),
        4 => "PROJECT".into(),
        5 => "COMMITTEE".into(),
        6 => "REGION".into(),
        7 => "OTHER".into(),
        _ => "DEPARTMENT".into(),
    }
}

fn org_proto(r: crate::data::sys_org_units::Model) -> OrgUnit {
    OrgUnit {
        id: Some(r.id),
        parent_id: r.parent_id,
        name: Some(r.name),
        code: r.code,
        leader_id: r.leader_id,
        leader_name: None,
        r#type: Some(org_type_to_proto(
            r.type_column.as_deref().unwrap_or("DEPARTMENT"),
        )),
        path: r.path,
        status: r.status.as_deref().map(|s| if s == "OFF" { 0 } else { 1 }),
        sort_order: r.sort_order,
        business_scopes: r
            .business_scopes
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        external_id: r.external_id,
        is_legal_entity: r.is_legal_entity,
        registration_number: r.registration_number,
        tax_id: r.tax_id,
        address: r.address,
        phone: r.phone,
        email: r.email,
        timezone: r.timezone,
        country: r.country,
        remark: r.remark,
        description: r.description,
        tenant_id: r.tenant_id,
        tenant_name: None,
        contact_user_id: r.contact_user_id,
        contact_user_name: None,
        attributes: std::collections::HashMap::new(),
        permission_tags: r
            .permission_tags
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        children: Vec::new(),
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
        latitude: r.latitude,
        longitude: r.longitude,
        start_at: r.start_at.and_then(crate::state::naive_to_ts),
        end_at: r.end_at.and_then(crate::state::naive_to_ts),
        legal_entity_org_id: r.legal_entity_org_id,
    }
}

pub struct OrgUnitService {
    pub state: Arc<AppState>,
}

impl OrgUnitService {
    /// setTreePath: parent path + own id.
    async fn build_path(&self, parent_id: Option<u32>, own_id: u32) -> String {
        let parent_path = match parent_id {
            Some(pid) => crate::data::sys_org_units::Entity::find_by_id(pid)
                .one(&self.state.db)
                .await
                .ok()
                .flatten()
                .and_then(|p| p.path)
                .unwrap_or_else(|| "/".into()),
            None => "/".into(),
        };
        format!("{parent_path}{own_id}/")
    }
}

#[async_trait::async_trait]
impl admin_api::gen::services::OrgUnitServiceHandlers for OrgUnitService {
    async fn list(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListOrgUnitResponse, StatusError> {
        let tid = tenant_of(&ctx);
        let base = crate::data::sys_org_units::Entity::find()
            .filter(crate::data::sys_org_units::Column::TenantId.eq(tid))
            .order_by_asc(crate::data::sys_org_units::Column::SortOrder);
        let (paged, paging) = crate::paging::apply(base, &req);
        let rows = paged.all(&self.state.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            crate::data::sys_org_units::Entity::find()
                .filter(crate::data::sys_org_units::Column::TenantId.eq(tid))
                .count(&self.state.db)
                .await
                .unwrap_or(0)
        };
        Ok(ListOrgUnitResponse {
            items: rows.into_iter().map(org_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetOrgUnitRequest,
    ) -> Result<OrgUnit, StatusError> {
        let Some(admin_api::proto::identity::service::v1::get_org_unit_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_org_units::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("org unit"))?;
        Ok(org_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateOrgUnitRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
        let inserted = crate::data::sys_org_units::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            parent_id: Set(data.parent_id),
            name: Set(data.name.unwrap_or_default()),
            code: Set(data.code),
            leader_id: Set(data.leader_id),
            type_column: Set(Some(org_type_to_str(data.r#type.unwrap_or(0)))),
            business_scopes: Set((!data.business_scopes.is_empty()).then(|| {
                serde_json::Value::Array(
                    data.business_scopes
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                )
            })),
            status: Set(Some("ON".into())),
            sort_order: Set(data.sort_order.or(Some(0))),
            remark: Set(data.remark),
            description: Set(data.description),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .map_err(db_err)?;
        // Materialized path maintenance (setTreePath).
        let path = self.build_path(inserted.parent_id, inserted.id).await;
        let mut a: crate::data::sys_org_units::ActiveModel = inserted.into();
        a.path = Set(Some(path));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn update(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UpdateOrgUnitRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::sys_org_units::Entity::find_by_id(req.id)
            .filter(crate::data::sys_org_units::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("org unit"))?;
        let mut a: crate::data::sys_org_units::ActiveModel = row.into();
        let mut reparent_to: Option<Option<u32>> = None;
        if let Some(data) = &req.data {
            if let Some(v) = &data.name {
                a.name = Set(v.clone());
            }
            if let Some(v) = &data.code {
                a.code = Set(Some(v.clone()));
            }
            if let Some(v) = &data.remark {
                a.remark = Set(Some(v.clone()));
            }
            if let Some(v) = &data.description {
                a.description = Set(Some(v.clone()));
            }
            if let Some(v) = data.r#type {
                a.type_column = Set(Some(org_type_to_str(v)));
            }
            if let Some(v) = data.sort_order {
                a.sort_order = Set(Some(v));
            }
            if let Some(v) = data.status {
                a.status = Set(Some(if v == 0 { "OFF".into() } else { "ON".into() }));
            }
            if let Some(v) = data.parent_id {
                reparent_to = Some(if v == 0 { None } else { Some(v) });
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        let updated = a.update(&self.state.db).await.map_err(db_err)?;

        // relocateSubtree: rewrite this node's and all descendants' paths.
        if let Some(new_parent) = reparent_to {
            let new_path = self.build_path(new_parent, updated.id).await;
            let old_path = updated
                .path
                .clone()
                .unwrap_or_else(|| format!("/{}/", updated.id));
            let descendants = crate::data::sys_org_units::Entity::find()
                .filter(crate::data::sys_org_units::Column::TenantId.eq(payload.tenant_id))
                .all(&self.state.db)
                .await
                .map_err(db_err)?;
            for d in descendants {
                if let Some(d_path) = &d.path {
                    if d.id == updated.id {
                        let mut a2: crate::data::sys_org_units::ActiveModel = d.clone().into();
                        a2.path = Set(Some(new_path.clone()));
                        a2.update(&self.state.db).await.map_err(db_err)?;
                    } else if d_path.starts_with(&old_path) {
                        let rewritten = d_path.replacen(&old_path, &new_path, 1);
                        let mut a2: crate::data::sys_org_units::ActiveModel = d.into();
                        a2.path = Set(Some(rewritten));
                        a2.update(&self.state.db).await.map_err(db_err)?;
                    }
                }
            }
        }
        Ok(Empty {})
    }

    async fn delete(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteOrgUnitRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let Some(admin_api::proto::identity::service::v1::delete_org_unit_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::sys_org_units::Entity::find_by_id(id)
            .filter(crate::data::sys_org_units::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("org unit"))?;
        let prefix = row.path.clone().unwrap_or_else(|| format!("/{}/", row.id));
        // Self + all descendants via the path prefix.
        let doomed: Vec<u32> = crate::data::sys_org_units::Entity::find()
            .filter(crate::data::sys_org_units::Column::TenantId.eq(payload.tenant_id))
            .all(&self.state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .filter(|d| d.id == row.id || d.path.as_deref().is_some_and(|p| p.starts_with(&prefix)))
            .map(|d| d.id)
            .collect();
        crate::data::sys_org_units::Entity::delete_many()
            .filter(crate::data::sys_org_units::Column::Id.is_in(doomed))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
