//! OrgUnitRepo — org-unit queries: the tenant-scoped listing, the
//! row lookups, and the tree-walk loads (materialized-path maintenance
//! rides these in the service).

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use crate::data::sys_org_units as entity;
use crate::state::{db_err, StatusError};

pub struct OrgUnitRepo<'a> {
    pub db: &'a DatabaseConnection,
}

impl<'a> OrgUnitRepo<'a> {
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }

    /// Paged listing within one tenant, sort_order ascending:
    /// returns (rows, total).
    pub async fn paged_list(
        &self,
        tenant_id: u32,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<entity::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            entity::Entity::find()
                .filter(entity::Column::TenantId.eq(tenant_id))
                .order_by_asc(entity::Column::SortOrder),
            req,
        )
        .await
    }

    pub async fn find(&self, id: u32) -> Result<Option<entity::Model>, StatusError> {
        entity::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn find_tenant(
        &self,
        id: u32,
        tenant_id: u32,
    ) -> Result<Option<entity::Model>, StatusError> {
        entity::Entity::find_by_id(id)
            .filter(entity::Column::TenantId.eq(tenant_id))
            .one(self.db)
            .await
            .map_err(db_err)
    }

    /// Every row of the tenant — the subtree walks iterate in memory.
    pub async fn all_tenant(&self, tenant_id: u32) -> Result<Vec<entity::Model>, StatusError> {
        entity::Entity::find()
            .filter(entity::Column::TenantId.eq(tenant_id))
            .all(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn delete_ids(&self, ids: &[u32]) -> Result<(), StatusError> {
        entity::Entity::delete_many()
            .filter(entity::Column::Id.is_in(ids.to_vec()))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
